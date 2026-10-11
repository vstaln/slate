#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![recursion_limit = "256"]

pub mod assets;
pub mod canvas_view;
pub mod cli_run;
pub mod engine;
pub mod views_browser;
pub mod views_calendar;
pub mod views_code;
pub mod views_files;
pub mod views_image;
pub mod views_kanban;
pub mod views_links;
pub mod views_notes;
pub mod views_orchestration;
pub mod views_planner;
pub mod views_settings;
pub mod views_sidebar;
pub mod views_commandbar;
pub mod views_sysmon;
pub mod views_terminal;
pub mod views_timer;
pub mod views_titlebar;

use crate::engine::{ControlServer, TerminalEvent, TerminalManager};
use anyhow::Result;
use gpui::AppContext as _;
use gpui::Focusable as _;
use slate_app::ipc::persist_control_token;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, BufRead, BufWriter, Write};
use std::sync::mpsc;
use std::thread;

fn main() -> Result<()> {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `slate <path>` — the shell's `handleSecondInstanceArgs` scan: a lone
    // existing path is a workspace handoff, never a CLI verb. Only a single
    // non-flag token counts (a mixed argv is a CLI invocation) and known
    // verbs win outright — the shell never had the collision because `orc`
    // was a separate binary.
    let workspace_arg = workspace_dir_arg(&args);
    // The app only starts when the args are nothing but app flags or a
    // workspace path — any CLI verb still dispatches to cli_run.
    let app_mode = args.iter().all(|arg| arg == "--gui" || arg == "--headless")
        || workspace_arg.is_some();
    let headless = app_mode
        && (args.iter().any(|arg| arg == "--headless")
            || std::env::var_os("SLATE_HEADLESS").is_some());
    if !app_mode {
        if args[0] == "--engine" {
            let token = std::env::var("SLATE_TOKEN")
                .ok()
                .filter(|value| value.len() >= 32)
                .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
            if let Err(error) = slate_app::platform::contain_engine_process() {
                eprintln!("engine process containment unavailable: {error}");
            }
            return run_engine(TerminalManager::streaming(token));
        }

        // Run CLI command (whoami, workers, tell, plan, browser, status, help, etc.)
        // Rust ignores SIGPIPE by default, which turns `slate read | head`
        // into a broken-pipe panic — restore the default disposition so the
        // CLI dies quietly like every other Unix tool. The app path keeps
        // the ignore: a closed control socket must not kill the GUI.
        #[cfg(unix)]
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
        if let Err(error) = cli_run::run() {
            let prog = std::env::args()
                .next()
                .and_then(|p| {
                    std::path::Path::new(&p)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "slate".to_string());
            // Errors carry `code: ` when a code is known — the server body's
            // own code, or the transport's offline/timeout/no_token. orc's
            // exit map: invalid+unknown_command→2, no_token+offline→3,
            // timeout→4, not_found→5, everything else→1; and `--json`
            // prints the {ok:false, error, code} envelope to stdout.
            const KNOWN: &[&str] = &[
                "conflict",
                "locked",
                "forbidden",
                "not_found",
                "invalid",
                "unknown_command",
                "unknown_actor",
                "failed",
                "rate_limited",
                "backpressure",
                "cancelled",
                "needs_confirm",
                "no_token",
                "offline",
                "timeout",
                "http_404",
                "connection_lost",
                "response_interrupted",
            ];
            let (code, message) = match error.split_once(": ") {
                Some((code, rest)) if KNOWN.contains(&code) => (code, rest),
                _ => ("failed", error.as_str()),
            };
            if args.iter().any(|arg| arg == "--json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": false, "error": message, "code": code,
                    }))
                    .unwrap_or_else(|_| format!(
                        "{{\"ok\":false,\"error\":\"{message}\",\"code\":\"{code}\"}}"
                    ))
                );
            } else {
                eprintln!("{prog}: {message}");
            }
            let exit = match code {
                "invalid" | "unknown_command" => 2,
                "no_token" | "offline" => 3,
                "timeout" => 4,
                "not_found" | "http_404" => 5,
                _ => 1,
            };
            std::process::exit(exit);
        }
        return Ok(());
    }

    // The shell's controlToken: reuse a previously persisted token so
    // long-lived shells holding it stay authenticated across restarts —
    // env → file → fresh. (cli_run reads the same file; env still wins.)
    let token = std::env::var("SLATE_TOKEN")
        .ok()
        .filter(|value| value.len() >= 32)
        .or_else(|| {
            std::fs::read_to_string(slate_app::ipc::control_token_path())
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| value.len() >= 32)
        })
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let manager = TerminalManager::new(token.clone());
    let _manager_keepalive = manager.clone();
    // Known before the platform is built and needed by `/raise` sooner: a
    // headless instance must refuse raises or a second `slate` exits
    // believing a window came forward.
    manager.set_headless(headless);
    let control = match ControlServer::start(manager.clone(), token.clone()) {
        Ok(control) => control,
        Err(error) => {
            // Single-instance handoff, same as the shell's
            // requestSingleInstanceLock → second-instance → focusMainWindow:
            // ask the running window to raise, then leave quietly.
            if error.contains("already running") {
                match cli_run::raise_existing_instance(workspace_arg.as_deref()) {
                    Ok(()) => {
                        eprintln!("another Slate instance is already running — raised it");
                        return Ok(());
                    }
                    Err(reason) => {
                        return Err(anyhow::Error::msg(format!(
                            "another Slate instance is already running but raised no window ({reason}) — stop it and retry"
                        )));
                    }
                }
            }
            return Err(anyhow::Error::msg(error));
        }
    };
    // A second instance must not replace the active instance's token before
    // discovering that its control socket is already occupied. A publish
    // failure is a warning, not a startup abort — the shell tolerated it
    // (the token still authenticates; offline discovery just can't read it).
    if let Err(error) = persist_control_token(&token) {
        eprintln!("cannot publish control token: {error}");
    }
    manager.set_control_socket(control.socket_path());
    // Cold-start `slate <path>`: persist the workspace before `ensure()`
    // can adopt the launch dir — the same `setWorkspaceDir` the running
    // instance would have applied had it been up.
    if let Some(dir) = workspace_arg {
        let mut state = slate_app::workspace::WorkspaceState::load();
        if state.set_workspace_dir(Some(dir)) {
            state.save();
        }
    }
    // Workspace adoption runs once, here: after the single-instance
    // handoff above (a second instance has already returned, so it can
    // never retag the journal) and before the canvas fold inside
    // CanvasView::new claims the journal. A fresh install adopts the
    // launch dir as `workspaceDir`; the state file is the record, and
    // every reader goes back through `slate_app::workspace::current()`.
    let _ = slate_app::workspace::ensure();
    // runtimePresence.ts — offline discovery record for tools that can't
    // probe the socket (stale-instance checks, workspace lookups).
    slate_app::ipc::write_runtime_presence(
        &control.socket_path(),
        &std::env::current_dir()
            .map(|dir| dir.display().to_string())
            .unwrap_or_default(),
    );
    // The shell's syncOrcGuide: stamp the managed `slate` block into the
    // workspace's AGENTS.md/CLAUDE.md/GEMINI.md so agents spawned here know
    // the control verbs. Best-effort — a read-only dir can't block startup.
    if let Ok(dir) = std::env::current_dir() {
        slate_app::guide::sync_slate_guide(&dir);
    }
    // Widgets — and the terminal PTYs they point at — come back from the
    // journal inside CanvasView::new; nothing is spawned unconditionally here.

    // The control server outlives the window on its own thread; the manager
    // moves into the RootView, which polls snapshots()/drain_events() on a
    // 250ms refresh timer (same non-blocking drain the egui loop used).
    let _ = &control;
    let root_manager = manager.clone();
    // Clean quit keeps the last seconds too, not just the last 30s tick.
    let quit_manager = manager.clone();
    // SIGINT/SIGTERM take the same graceful path the shell's before-quit
    // did: snapshot terminal state, kill the process trees, exit. The
    // handler only trips a flag; the watcher thread does the real work —
    // Mutex+IO inside a signal handler isn't async-signal-safe.
    #[cfg(unix)]
    {
        use std::sync::atomic::{AtomicBool, Ordering};
        static SIGNALED: AtomicBool = AtomicBool::new(false);
        extern "C" fn on_signal(_sig: libc::c_int) {
            SIGNALED.store(true, Ordering::SeqCst);
        }
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
        let signal_manager = manager.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if SIGNALED.swap(false, Ordering::SeqCst) {
                flush_canvas_snapshot();
                signal_manager.save_terminal_states();
                signal_manager.dispose_all();
                slate_app::ipc::clear_runtime_presence();
                std::process::exit(0);
            }
        });
    }
    let platform = gpui_platform::current_platform(headless);
    gpui::Application::with_platform(platform)
        .with_assets(crate::assets::EmbeddedAssets)
        .run(|cx: &mut gpui::App| {
        // Graysite brand faces, embedded so the build looks right on any
        // machine without a fontconfig install: Plus Jakarta Sans is the UI
        // family, Departure Mono the label/keycap face, Brygada 1918 the
        // display face (splash wordmark). JetBrainsMono Nerd Font Mono is
        // the terminal face — the ghostty font, mono-width icons so a Nerd
        // glyph never pushes a cell run off the grid.
        if let Err(error) = cx.text_system().add_fonts(vec![
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/PlusJakartaSans-Variable.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/Brygada1918-Regular.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/Brygada1918-Italic.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/DepartureMono-Regular.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Regular.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Bold.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-Italic.ttf")[..]),
            std::borrow::Cow::Borrowed(&include_bytes!("../assets/fonts/JetBrainsMonoNerdFontMono-BoldItalic.ttf")[..]),
        ]) {
            eprintln!("font registration failed: {error}");
        }
        // gpui's implicit placement fell back to a ~300px box under this
        // WM; size the first window to most of the primary display
        // instead. A resize still wins after the window exists.
        let window_bounds = cx
            .primary_display()
            .map(|display| {
                let bounds = display.bounds();
                let size = gpui::size(
                    bounds.size.width * 0.8,
                    bounds.size.height * 0.8,
                );
                let origin = gpui::point(
                    bounds.origin.x + (bounds.size.width - size.width) / 2.0,
                    bounds.origin.y + (bounds.size.height - size.height) / 2.0,
                );
                gpui::WindowBounds::Windowed(gpui::Bounds::new(origin, size))
            });
        cx.open_window(
            gpui::WindowOptions {
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Slate".into()),
                    ..Default::default()
                }),
                window_bounds,
                // WM_CLASS on X11: rofi's window mode and desktop-file
                // matching both key off it, and an unset class is why the
                // window used to group as nothing.
                app_id: Some("slate".into()),
                window_background: gpui::WindowBackgroundAppearance::Opaque,
                ..Default::default()
            },
            move |window, cx| {
                let view =
                    cx.new(|cx| crate::canvas_view::CanvasView::new(root_manager.clone(), cx));
                // Focus the root at open so keystrokes reach the terminal
                // without a click first.
                window.focus(&view.focus_handle(cx), cx);
                view
            },
        )
        .expect("open Slate window");
        cx.activate(true);
        cx.on_window_closed(move |_, _| {
            // Final canvas flush first — the window is already gone at this
            // point, but the canvas is event-sourced, so the fold rebuilt
            // from the journal is the same state the view showed.
            flush_canvas_snapshot();
            quit_manager.save_terminal_states();
            // The shell's before-quit killed the process trees too — without
            // it, detached grandchildren outlive the app.
            quit_manager.dispose_all();
            slate_app::ipc::clear_runtime_presence();
        })
        .detach();
    });

    Ok(())
}

/// The quit-time half of `canvas_store::maybe_snapshot`: fold the journal
/// once more and write the per-workspace snapshot unconditionally — a clean
/// exit shouldn't have to wait for the 50-event interval to keep the
/// canvas. No gpui context needed: every mutation journals synchronously,
/// so the fold rebuilt from disk is the canvas the user was looking at.
fn flush_canvas_snapshot() {
    let dir = slate_app::workspace::current();
    let journal_path = slate_app::ipc::user_data_dir().join("command-journal.ndjson");
    if let Ok(log) = slate_app::journal_log::JournalLog::open(&journal_path) {
        let (canvas, _snapshot_seq, last_seq) =
            slate_app::canvas_store::load_with_tail(dir.as_deref(), &log);
        slate_app::canvas_store::write(dir.as_deref(), &canvas, last_seq);
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum EngineCommand {
    Spawn {
        id: String,
        #[serde(default = "default_cols")]
        cols: u16,
        #[serde(default = "default_rows")]
        rows: u16,
        cwd: Option<String>,
        shell: String,
        #[serde(default)]
        env: HashMap<String, String>,
        request_id: Option<String>,
    },
    Write {
        id: String,
        data: String,
        request_id: Option<String>,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
        request_id: Option<String>,
    },
    PauseOutput {
        id: String,
        paused: bool,
        request_id: Option<String>,
    },
    Dispose {
        id: String,
        request_id: Option<String>,
    },
    Shutdown,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum EngineEvent {
    Ready,
    Data {
        id: String,
        data: String,
    },
    Exit {
        id: String,
        /// The child's exit code when known — the wire marker prints
        /// `(code N)` like the shell's own.
        code: Option<u32>,
    },
    Response {
        request_id: Option<String>,
        ok: bool,
        id: Option<String>,
        error: Option<String>,
    },
}

/// `slate <path>` workspace resolution — the shell's
/// `handleSecondInstanceArgs` candidate scan: a lone non-flag token that
/// exists on disk resolves to itself (a directory) or its parent (a file).
/// URLs and known CLI verbs are excluded; a mixed argv is a CLI
/// invocation, not a handoff.
fn workspace_dir_arg(args: &[String]) -> Option<String> {
    let non_flags: Vec<&str> = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .collect();
    let [arg] = non_flags[..] else {
        return None;
    };
    if arg.contains("://") || slate_app::cli::is_known_command(arg) {
        return None;
    }
    let path = std::path::Path::new(arg);
    if !path.exists() {
        return None;
    }
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf()
    };
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    Some(dir.to_string_lossy().into_owned())
}

fn default_cols() -> u16 {
    80
}

fn default_rows() -> u16 {
    24
}

fn run_engine(manager: TerminalManager) -> Result<()> {
    let (commands_tx, commands_rx) = mpsc::sync_channel::<Option<EngineCommand>>(1024);
    thread::Builder::new()
        .name("slate-engine-stdin".to_owned())
        .spawn(move || {
            let stdin = io::stdin();
            for line in stdin.lock().lines() {
                let line = match line {
                    Ok(line) => line,
                    Err(error) => {
                        eprintln!("engine stdin read failed: {error}");
                        break;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                match parse_engine_command(&line) {
                    Ok(None) => continue,
                    Ok(command) => {
                        if commands_tx.send(command).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // A malformed command must not be confused with EOF:
                        // dropping one bad packet should not kill every PTY.
                        eprintln!("ignored malformed engine command: {error}");
                    }
                }
            }
            let _ = commands_tx.send(None);
        })?;

    // Only the output thread touches stdout. A full OS pipe must never hold
    // the command loop hostage while it is trying to interrupt or dispose.
    let (output, responses) = mpsc::sync_channel::<Vec<u8>>(4096);
    let output = EngineOutput {
        sender: output,
        manager: manager.clone(),
    };
    emit(&output, &EngineEvent::Ready)?;

    let pump_manager = manager.clone();
    thread::Builder::new()
        .name("slate-engine-output".to_owned())
        .spawn(move || {
            let mut writer = BufWriter::new(io::stdout());
            loop {
                if flush_responses(&mut writer, &responses).is_err() {
                    pump_manager.dispose_all();
                    return;
                }
                let batch: Vec<EngineEvent> = pump_manager
                    .wait_events()
                    .into_iter()
                    .filter_map(|event| match event {
                        TerminalEvent::Output { id, data } => Some(EngineEvent::Data { id, data }),
                        TerminalEvent::Exited { id, code } => Some(EngineEvent::Exit { id, code }),
                        // OSC 52 clipboard writes only matter to the GUI; a
                        // headless engine pump has no clipboard to feed.
                        TerminalEvent::Clipboard { .. } => None,
                    })
                    .collect();
                // stdout is gone (the app closed the pipe): nothing left to
                // forward to, so stop pumping.
                for event in batch {
                    // Give ACKs priority between output chunks, preserving
                    // the PTY stream's own data/exit order.
                    if flush_responses(&mut writer, &responses)
                        .and_then(|_| write_event(&mut writer, &event))
                        .is_err()
                    {
                        pump_manager.dispose_all();
                        return;
                    }
                }
            }
        })?;

    // Blocking receive: no polling tick between a keystroke arriving and the
    // shell being told about it.
    let result = (|| -> Result<()> {
        while let Ok(command) = commands_rx.recv() {
            let Some(command) = command else { break };
            if matches!(command, EngineCommand::Shutdown) {
                break;
            }
            handle_engine_command(&manager, command, &output)?;
        }
        Ok(())
    })();
    manager.dispose_all();
    result
}

fn parse_engine_command(line: &str) -> Result<Option<EngineCommand>, serde_json::Error> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(line).map(Some)
}

fn handle_engine_command(
    manager: &TerminalManager,
    command: EngineCommand,
    output: &EngineOutput,
) -> Result<()> {
    match command {
        EngineCommand::Spawn {
            id,
            cols,
            rows,
            cwd,
            shell,
            env,
            request_id,
        } => {
            let result = manager.spawn_with_options(id.clone(), cols, rows, cwd, shell, env);
            emit(
                output,
                &EngineEvent::Response {
                    request_id,
                    ok: result.is_ok(),
                    id: Some(id),
                    error: result.err(),
                },
            )?;
        }
        EngineCommand::Write {
            id,
            data,
            request_id,
        } => {
            let result = manager.write_raw(&id, data.as_bytes());
            emit(
                output,
                &EngineEvent::Response {
                    request_id,
                    ok: result.is_ok(),
                    id: Some(id),
                    error: result.err(),
                },
            )?;
        }
        EngineCommand::Resize {
            id,
            cols,
            rows,
            request_id,
        } => {
            let result = manager.resize(&id, cols, rows);
            emit(
                output,
                &EngineEvent::Response {
                    request_id,
                    ok: result.is_ok(),
                    id: Some(id),
                    error: result.err(),
                },
            )?;
        }
        EngineCommand::PauseOutput {
            id,
            paused,
            request_id,
        } => {
            let result = manager.set_output_paused(&id, paused);
            emit(
                output,
                &EngineEvent::Response {
                    request_id,
                    ok: result.is_ok(),
                    id: Some(id),
                    error: result.err(),
                },
            )?;
        }
        EngineCommand::Dispose { id, request_id } => {
            let result = manager.dispose(&id);
            emit(
                output,
                &EngineEvent::Response {
                    request_id,
                    ok: result.is_ok(),
                    id: Some(id),
                    error: result.err(),
                },
            )?;
        }
        EngineCommand::Shutdown => {}
    }
    Ok(())
}

struct EngineOutput {
    sender: mpsc::SyncSender<Vec<u8>>,
    manager: TerminalManager,
}

fn emit(output: &EngineOutput, event: &EngineEvent) -> Result<()> {
    let mut encoded = serde_json::to_vec(event)?;
    encoded.push(b'\n');
    output
        .sender
        .try_send(encoded)
        .map_err(|error| anyhow::anyhow!("engine response queue unavailable: {error}"))?;
    output.manager.notify_response();
    Ok(())
}

fn flush_responses(writer: &mut impl Write, responses: &mpsc::Receiver<Vec<u8>>) -> Result<()> {
    // Bound each pass so continuous input cannot starve terminal output.
    for _ in 0..4096 {
        let Ok(encoded) = responses.try_recv() else {
            break;
        };
        writer.write_all(&encoded)?;
    }
    writer.flush()?;
    Ok(())
}

fn write_event(writer: &mut impl Write, event: &EngineEvent) -> Result<()> {
    serde_json::to_writer(&mut *writer, event)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_finish_while_stdout_is_not_being_drained() {
        let manager = TerminalManager::new("test".to_owned());
        let (output, responses) = mpsc::sync_channel(2);
        let output = EngineOutput {
            sender: output,
            manager: manager.clone(),
        };
        // No output consumer: handling a command must still finish.
        for _ in 0..2 {
            handle_engine_command(
                &manager,
                EngineCommand::Write {
                    id: "missing".to_owned(),
                    data: "\u{3}".to_owned(),
                    request_id: Some("w1".to_owned()),
                },
                &output,
            )
            .unwrap();
        }
        // Bounded overload is explicit, never an unbounded blocking send.
        assert!(emit(&output, &EngineEvent::Ready).is_err());
        let mut received = Vec::new();
        flush_responses(&mut received, &responses).unwrap();
        let lines = String::from_utf8(received).unwrap();
        assert_eq!(lines.lines().count(), 2);
        for line in lines.lines() {
            let event: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(event["type"], "response");
            assert_eq!(event["ok"], false);
        }
    }

    #[test]
    fn malformed_engine_command_is_rejected_without_being_eof() {
        assert!(parse_engine_command("not-json").is_err());
        assert!(parse_engine_command("   ").unwrap().is_none());
    }
}
