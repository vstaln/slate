use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slate_app::command::{CommandError, CommandResult, ErrorCode};
use slate_app::platform::{
    interrupt_process_tree, kill_process_tree, prime_conpty_handshake,
    shell_arguments as windows_utf8_shell_args, shell_command,
};
use slate_app::queue::ActorRateLimiter;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{Read, Write},
    path::{Path as FsPath, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc, Arc, Condvar, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_SCROLLBACK: usize = 1_000_000;
/// How far past the cap the buffer is allowed to grow before it is compacted.
///
/// Trimming to exactly `MAX_SCROLLBACK` meant `String::drain` ran on every
/// single chunk once a terminal had produced a megabyte — and a drain from the
/// front moves everything behind it, so that is a ~1MB memmove per chunk. Agent
/// output arrives token by token, so "per chunk" is hundreds of times a second,
/// all of it while holding the terminal's state mutex and therefore blocking
/// writes and resizes too. That is the sluggishness that only ever showed up
/// after a session had been running for a while.
///
/// Compacting one slack's worth at a time turns that into one memmove per
/// 256KB of output instead of one per token.
const SCROLLBACK_SLACK: usize = 256 * 1024;
const TOKEN_HEADER: &str = "x-slate-token";
/// Ceiling for output that has been read from the PTYs but not yet handed to
/// the app. When it is reached, the PTY reader waits here; the OS PTY buffer
/// then provides backpressure instead of deleting live terminal bytes.
const MAX_PENDING_EVENT_BYTES: usize = 8 * 1024 * 1024;
/// Ceiling for input queued for one terminal's writer thread. Writes are
/// handed over without waiting (see `enqueue_input`), so this is what keeps a
/// shell that has stopped reading its input from growing the queue forever.
const MAX_PENDING_INPUT_BYTES: usize = 1024 * 1024;
/// `readJson`'s cap in controlServer.ts — past it the request is a 413.
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;

/// Transport rules applied to every request, before and after routing:
/// preflights answer 204 ahead of the token check, every response carries
/// `nosniff` and `no-store`, an unrouted method is a 404 (the TypeScript falls
/// through to not-found, never 405), and every non-JSON rejection — axum's
/// extractor errors, an oversized body — is re-shaped as `{error, code}`.
async fn harden(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::{header, Method};
    use axum::response::IntoResponse;

    let preflight = request.method() == Method::OPTIONS;
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };

    let status = response.status();
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if !preflight && (status.is_client_error() || status.is_server_error()) && !is_json {
        let (code, message, status) = match status {
            StatusCode::METHOD_NOT_ALLOWED => ("not_found", "no such route".to_owned(), StatusCode::NOT_FOUND),
            StatusCode::PAYLOAD_TOO_LARGE => (
                "invalid",
                format!("request body exceeds {} bytes", MAX_HTTP_BODY_BYTES),
                status,
            ),
            _ => {
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes).trim().to_owned();
                let message = if text.is_empty() {
                    status.canonical_reason().unwrap_or("request failed").to_owned()
                } else {
                    text
                };
                ("invalid", message, status)
            }
        };
        let body = serde_json::json!({ "error": message, "code": code });
        response = (status, Json(body)).into_response();
    }

    let headers = response.headers_mut();
    headers.insert(
        axum::http::HeaderName::from_static("x-content-type-options"),
        axum::http::HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
const SUBMIT_GAP: Duration = Duration::from_millis(200);
/// How long a single write into a PTY may stay in flight before the terminal
/// is treated as no longer reading its input.
///
/// A write into ConPTY (and into a Unix pty master) blocks once the child
/// stops draining it, with no timeout available at this layer. Without this
/// check the queue simply filled to `MAX_PENDING_INPUT_BYTES` while every
/// keystroke — Ctrl+C included — was accepted and never delivered, which is
/// what made a busy terminal look permanently hung. Past this threshold new
/// input is rejected immediately so the caller learns the truth on the very
/// first keystroke instead of after a megabyte of silence.
const STUCK_WRITE_MS: u64 = 2_000;

/// Per-terminal saved tail size — the shell's terminalSnapshots kept 64 KiB.
const SAVED_SCROLLBACK: usize = 64 * 1024;

/// `~/.config/Slate/terminal-state/` — one json per terminal id.
fn terminal_state_dir() -> Option<std::path::PathBuf> {
    Some(slate_app::ipc::user_data_dir().join("terminal-state"))
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn terminal_state_file(id: &str) -> Option<std::path::PathBuf> {
    terminal_state_dir().map(|dir| dir.join(format!("{}.json", sanitize_id(id))))
}

/// Ctrl+C. Recognised in `deliver_input` only to spot the one keystroke that
/// must still mean something on a terminal that has stopped reading.
const INTERRUPT_BYTE: u8 = 0x03;
/// Longest payload still treated as "the user pressed Ctrl+C" rather than as
/// bulk input that happens to contain the byte. A keystroke is one byte; a
/// pasted file is not allowed to buy itself an exemption from the queue cap.
const MAX_INTERRUPT_INPUT_BYTES: usize = 8;
/// Take a lock, ignoring poisoning.
///
/// Every lock in this file used to be taken with either `.expect(..)` or
/// `if let Ok(..)`. Both turn one thread's panic into a much larger failure:
/// `.expect` aborts the whole engine, so a single terminal's panic kills every
/// other terminal in the app at once, and `if let Ok` silently skips the work
/// — for `push_event` that means dropping output, or an exit event, leaving a
/// widget waiting forever on a session that already ended.
///
/// The data these mutexes guard is a scrollback string, an event queue and a
/// handle map: a panic mid-update can leave a message truncated, but never
/// leaves any of them in a state the code below cannot handle. Carrying on
/// with the recovered value is strictly better than either alternative.
fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Debug)]
pub struct TerminalManager {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    retain_scrollback: bool,
    terminals: Mutex<HashMap<String, TerminalHandle>>,
    events: Mutex<EventQueue>,
    /// Signalled whenever output lands, so the engine's drain thread reacts
    /// immediately instead of polling on a fixed tick.
    events_ready: Condvar,
    token: String,
    control_socket: Mutex<Option<String>>,
    /// Terminal id to the name the UI shows and `slate` addresses. One map, so
    /// a terminal cannot be called one thing on screen and another on the CLI.
    names: Mutex<HashMap<String, String>>,
    /// A `/canvas/focus` request parked for the canvas view's next tick; the
    /// control server cannot reach the view directly, so it posts here.
    camera_request: Mutex<Option<CameraRequest>>,
    /// The same one-slot mailbox for `POST /raise`: the socket thread cannot
    /// touch the window, so it leaves a flag the view picks up and clears on
    /// its next tick. `Some(None)` is a bare raise; `Some(Some(dir))` also
    /// hands a workspace dir off — the shell's `handleSecondInstanceArgs`
    /// `setWorkspaceDir` call.
    raise_requested: Mutex<Option<Option<String>>>,
    /// `--headless` instances run the control server without a window — the
    /// `/raise` route reads this so it can refuse instead of pretending a
    /// raise happened.
    headless: Mutex<bool>,
}

/// What `POST /canvas/focus` wants the camera to do. Either center on a
/// widget or jump to an explicit position.
#[derive(Debug, Clone, Default)]
pub struct CameraRequest {
    pub widget_id: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub zoom: Option<f64>,
}

#[derive(Debug, Default)]
struct EventQueue {
    notified: bool,
    items: VecDeque<TerminalEvent>,
    bytes: usize,
    /// Ids being disposed; a reader blocked on a full queue must wake and stop
    /// instead of keeping a closed terminal alive forever.
    stopped: HashSet<String>,
}

#[derive(Debug, Clone)]
struct TerminalHandle {
    /// Input only. Kept separate from `control_tx` because a write into a PTY
    /// blocks for as long as the child refuses to read: sharing one channel
    /// made resize and dispose queue behind a stuck keystroke, so the only
    /// way out of a wedged terminal was closing the widget.
    input_tx: mpsc::Sender<TerminalInput>,
    /// Resize/dispose. Served by a thread that never writes to the PTY, so it
    /// stays responsive no matter what the child is doing.
    control_tx: mpsc::Sender<ControlCommand>,
    state: Arc<Mutex<TerminalState>>,
    input_guard: Arc<Mutex<()>>,
    /// Bytes handed to the writer thread that it has not written yet.
    pending_input: Arc<AtomicUsize>,
    /// Unix-millis timestamp of the write currently in flight, or 0 when the
    /// writer is idle. Read by `enqueue_input` to spot a wedged child.
    writing_since: Arc<AtomicU64>,
    /// Gate shared with the PTY reader so renderer backpressure can pause OS
    /// reads. The child then naturally blocks on its PTY output buffer.
    output_gate: Arc<(Mutex<bool>, Condvar)>,
    /// OS pid of the shell, used to kill the whole process tree on dispose.
    /// `child.kill()` alone only terminates the direct child and leaves
    /// grandchildren (agents, servers) orphaned.
    child_pid: Option<u32>,
}

#[derive(Debug)]
struct TerminalInput {
    data: Vec<u8>,
    press_enter: bool,
    acknowledgement: Option<mpsc::Sender<Result<(), String>>>,
}

#[derive(Debug)]
enum ControlCommand {
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Ctrl+C on a terminal whose input is already wedged. The byte itself
    /// cannot be typed — it would queue behind the write that is stuck — so
    /// the foreground program is stopped directly instead.
    Interrupt,
    Dispose,
    /// Sent by the writer thread when the PTY can no longer be written to, so
    /// the actor tears the session down exactly as it does for a dispose.
    WriterFailed,
    /// The waiter thread's `child.wait()` resolution — the code rides along
    /// so the exit marker can print `[Process exited (code N)]`.
    ChildExited {
        code: u32,
    },
}

#[derive(Debug, Default)]
struct TerminalState {
    output: String,
    /// Absolute stream offset of `output`'s first byte. Scrollback
    /// compaction drops from the front, so the incremental-read offsets below
    /// have to count from the stream's start, not the buffer's — this is the
    /// `startOffset` the TypeScript output buffer carried.
    output_base: usize,
    /// Where the last `?clear=1` read ended, as an absolute offset: the
    /// `readOffset` of the TypeScript terminal record. A plain incremental
    /// read is repeatable; a clearing one commits this cursor.
    read_offset: usize,
    /// Unix-millis of the last output chunk, for the workers list's
    /// `lastActiveAt` — `terminals.lastDataAt(id)` in the TypeScript.
    last_output_at: u64,
    /// The vt100 screen every output byte also lands in: snapshots show this
    /// parsed view, not the raw stream, so the GUI never renders escape
    /// sequences as text and input replies (DSR/CPR/DA) can be answered.
    screen: slate_app::terminal_screen::TerminalScreen,
    alive: bool,
    cwd: String,
    /// Whether the program running in this terminal has turned on DECSET
    /// 2004. Only then may a delivery be wrapped as a bracketed paste.
    bracketed_paste: bool,
    /// The child exit code when the waiter reported one — the marker prints
    /// `[Process exited (code N)]` like the shell did.
    exit_code: Option<u32>,
    /// Orphaned-mode recovery — the shell's `terminalModeRecovery.ts`. Every
    /// output chunk flows through its probes: echoed mouse reports and
    /// console-title child exits are how a crashed TUI's leftover modes are
    /// noticed when there is no process exit to hook.
    recovery: slate_app::terminal_mode_recovery::ModeRecovery,
}

#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    pub id: String,
    pub output: String,
    /// The visible screen after ANSI/VT parsing — what the GUI should draw.
    pub screen: String,
    /// The same screen as per-row styled runs, for the cell-colored view.
    pub styled: Vec<Vec<slate_app::terminal_screen::StyledRun>>,
    pub alive: bool,
    pub cwd: String,
    /// True while the program has a mouse-tracking mode armed (9/1000/1002/
    /// 1003) — the GUI routes pointer/wheel events to the PTY then instead of
    /// treating them as widget clicks and scrollback scrolls.
    pub mouse_reporting: bool,
    /// True while the emulator is on the alternate buffer — a full-screen
    /// application owns the screen. The initial-command verdict reads this
    /// alongside `mouse_reporting` (the shell's `bufferType` check).
    pub alternate: bool,
    /// Active drag-selection highlight, `(visible_row, start_col, end_col)`.
    pub selection: Vec<(u16, u16, u16)>,
    /// Unix-millis of the last output chunk, 0 when the terminal has been
    /// quiet since spawn. The workers list shows it as `lastActiveAt`.
    pub last_data_at: u64,
}

#[derive(Clone, Debug)]
pub enum TerminalEvent {
    Output {
        id: String,
        data: String,
    },
    Exited {
        id: String,
        /// The child's exit code when the waiter saw one; the marker the
        /// consumer writes is `[Process exited (code N)]` when present.
        code: Option<u32>,
    },
    /// An OSC 52 clipboard write: the base64 payload as the program sent it.
    /// Decoding and the actual clipboard store belong to the consumer.
    Clipboard {
        id: String,
        payload: String,
    },
}

/// The wire shape the shell returned: `{id, status:'delivered', confirmedAt,
/// evidence:'terminal-output'}`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeliveryReceipt {
    pub id: String,
    #[serde(skip)]
    pub terminal_id: String,
    pub status: &'static str,
    #[serde(skip)]
    pub bytes: usize,
    pub confirmed_at: u128,
    pub evidence: &'static str,
}

impl TerminalManager {
    pub fn new(token: String) -> Self {
        Self::with_scrollback(token, true)
    }

    pub fn streaming(token: String) -> Self {
        Self::with_scrollback(token, false)
    }

    fn with_scrollback(token: String, retain_scrollback: bool) -> Self {
        Self {
            inner: Arc::new(Inner {
                retain_scrollback,
                terminals: Mutex::new(HashMap::new()),
                events: Mutex::new(EventQueue::default()),
                events_ready: Condvar::new(),
                token,
                control_socket: Mutex::new(None),
                names: Mutex::new(HashMap::new()),
                camera_request: Mutex::new(None),
                raise_requested: Mutex::new(None),
                headless: Mutex::new(false),
            }),
        }
    }

    pub fn set_name(&self, id: &str, name: &str) {
        lock_recover(&self.inner.names).insert(id.to_owned(), name.to_owned());
    }

    pub fn name(&self, id: &str) -> Option<String> {
        lock_recover(&self.inner.names).get(id).cloned()
    }

    pub fn names(&self) -> HashMap<String, String> {
        lock_recover(&self.inner.names).clone()
    }

    /// `/canvas/focus` handler parks a request; the view picks it up on its
    /// next tick. A newer request replaces an unconsumed one.
    pub fn request_camera(&self, request: CameraRequest) {
        *lock_recover(&self.inner.camera_request) = Some(request);
    }

    /// One-shot read: the view applies at most one pending camera request.
    pub fn take_camera_request(&self) -> Option<CameraRequest> {
        lock_recover(&self.inner.camera_request).take()
    }

    /// `POST /raise` parks a flag; the canvas view picks it up and raises the
    /// window on its next tick (`slate` second-instance behaviour).
    pub fn request_raise(&self, dir: Option<String>) {
        let mut slot = lock_recover(&self.inner.raise_requested);
        // Two raises collapsing into one tick keep whichever dir was asked
        // for — a bare raise must not erase a pending workspace handoff.
        let dir = dir.or_else(|| slot.take().flatten());
        *slot = Some(dir);
    }

    /// One-shot read, same mailbox shape as `take_camera_request` — the
    /// slot clears when the view consumes it. `Some(dir)` means a
    /// workspace handoff rode along with the raise.
    pub fn take_raise_request(&self) -> Option<Option<String>> {
        lock_recover(&self.inner.raise_requested).take()
    }

    /// Set once at startup before the control server binds — `/raise` only
    /// has a window to raise when the instance is not headless.
    pub fn set_headless(&self, headless: bool) {
        *lock_recover(&self.inner.headless) = headless;
    }

    pub fn is_headless(&self) -> bool {
        *lock_recover(&self.inner.headless)
    }

    /// Turns whatever a caller typed — an id, `@name`, a case-insensitive or
    /// unambiguous prefix, `self`, or `other` — into a terminal id, exactly
    /// the contract `resolveWorker` in workers.ts promised the CLI.
    /// `caller` is the request's agent id; inside a spawned terminal it is
    /// the terminal id, which is what makes `self` resolve.
    pub fn resolve(&self, target: &str, caller: Option<&str>) -> CommandResult<String> {
        let wanted = target.trim().trim_start_matches('@').to_owned();
        if wanted.is_empty() {
            return Err(CommandError::new(
                ErrorCode::Invalid,
                "a worker name or terminal id is required",
            ));
        }
        let terminals = lock_recover(&self.inner.terminals);
        let names = lock_recover(&self.inner.names);
        // Worker name = title when named, else the id — same fallback as
        // listWorkers' `terminal.title || terminal.id`.
        let worker_name = |id: &str| names.get(id).cloned().unwrap_or_else(|| id.to_owned());
        let is_self = |id: &str| caller.is_some_and(|caller| caller == id);

        if wanted == "self" || wanted == "me" {
            return terminals
                .keys()
                .find(|id| is_self(id))
                .cloned()
                .ok_or_else(|| {
                    CommandError::new(ErrorCode::NotFound, "this shell is not a Slate terminal")
                });
        }
        if matches!(wanted.as_str(), "other" | "neighbor" | "peer" | "сосед") {
            let others: Vec<&String> = terminals.keys().filter(|id| !is_self(id)).collect();
            return match others.len() {
                1 => Ok(others[0].clone()),
                0 => Err(CommandError::new(
                    ErrorCode::NotFound,
                    "no other terminals are open on the canvas",
                )),
                _ => Err(CommandError::new(
                    ErrorCode::Invalid,
                    format!(
                        "several other terminals are open: {} — specify one by name or id",
                        others
                            .iter()
                            .map(|id| format!("{} ({})", worker_name(id), id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )),
            };
        }

        if terminals.contains_key(wanted.as_str()) {
            return Ok(wanted);
        }

        // Ambiguous name matches prefer the non-self worker, matching the
        // shell: a worker asking for `backend` means the *other* backend.
        // `shrink` applies the non-self reduction to a pool — exactly what the
        // original does to `exact`/`insensitive`/`prefixed` in turn.
        let shrink = |pool: Vec<String>| -> Vec<String> {
            if pool.len() > 1 && caller.is_some() {
                let non_self: Vec<String> =
                    pool.iter().filter(|id| !is_self(id)).cloned().collect();
                if non_self.len() == 1 {
                    return non_self;
                }
            }
            pool
        };
        let exact = shrink(
            terminals
                .keys()
                .filter(|id| worker_name(id) == wanted)
                .cloned()
                .collect(),
        );
        if exact.len() == 1 {
            return Ok(exact.into_iter().next().unwrap());
        }
        let insensitive = shrink(
            terminals
                .keys()
                .filter(|id| worker_name(id).eq_ignore_ascii_case(&wanted))
                .cloned()
                .collect(),
        );
        if insensitive.len() == 1 {
            return Ok(insensitive.into_iter().next().unwrap());
        }
        let lower = wanted.to_lowercase();
        let prefixed = shrink(
            terminals
                .keys()
                .filter(|id| worker_name(id).to_lowercase().starts_with(&lower))
                .cloned()
                .collect(),
        );
        if prefixed.len() == 1 {
            return Ok(prefixed.into_iter().next().unwrap());
        }
        let candidates = if !exact.is_empty() {
            exact
        } else if !insensitive.is_empty() {
            insensitive
        } else {
            prefixed
        };
        if candidates.len() > 1 {
            return Err(CommandError::new(
                ErrorCode::Invalid,
                format!(
                    "\"{target}\" matches several workers: {} — use the terminal id",
                    candidates
                        .iter()
                        .map(|id| format!("{} ({})", worker_name(id), id))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        let known = terminals
            .keys()
            .map(|id| worker_name(id))
            .collect::<Vec<_>>()
            .join(", ");
        Err(CommandError::new(
            ErrorCode::NotFound,
            format!(
                "no worker called \"{target}\" — open workers: {}",
                if known.is_empty() {
                    "(none open)"
                } else {
                    &known
                }
            ),
        ))
    }

    /// Records where the control server is listening, so every terminal this
    /// manager spawns can be told how to reach it.
    pub fn set_control_socket(&self, path: String) {
        *lock_recover(&self.inner.control_socket) = Some(path);
    }

    pub fn control_socket(&self) -> Option<String> {
        lock_recover(&self.inner.control_socket).clone()
    }

    pub fn spawn(&self, id: impl Into<String>) -> Result<(), String> {
        self.spawn_with_options(id, 120, 32, None, shell_command(), HashMap::new())
            .map(|_| ())
    }

    /// Spawns the PTY, or adopts the existing one when `id` is already
    /// alive — the shell's `terminal.create` `live` flag as the return
    /// value: `Ok(true)` means the terminal was already running.
    pub fn spawn_with_options(
        &self,
        id: impl Into<String>,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: String,
        extra_env: HashMap<String, String>,
    ) -> Result<bool, String> {
        let id = id.into();
        // Fast check under the lock; the slow PTY work below runs unlocked so
        // one spawn can never stall every other terminal operation.
        {
            let terminals = lock_recover(&self.inner.terminals);
            if let Some(existing) = terminals.get(&id) {
                if lock_recover(&existing.state).alive {
                    return Ok(true);
                }
            }
        }
        allow_terminal_events(&self.inner, &id);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("open pty: {error}"))?;

        let mut command = CommandBuilder::new(&shell);
        for arg in windows_utf8_shell_args(&shell) {
            command.arg(arg);
        }
        command.env("TERM", "xterm-256color");
        command.env_remove("NO_COLOR");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "Slate");
        command.env("SLATE_TERMINAL_ID", &id);
        command.env("SLATE_AGENT_ID", &id);
        command.env("SLATE_TOKEN", &self.inner.token);
        if let Some(path) = self.control_socket() {
            command.env(slate_app::ipc::SOCKET_PATH_ENV, path);
        }
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                let separator = if cfg!(windows) { ';' } else { ':' };
                let current_path = std::env::var_os("PATH")
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default();
                command.env(
                    "PATH",
                    format!("{}{}{}", parent.display(), separator, current_path),
                );
            }
        }
        // The Windows-only `chcp` args above (`windows_utf8_shell_args`) and
        // this are the same fix for the same class of bug on the two
        // platform families: a shell that renders non-ASCII text as
        // replacement characters looks exactly like a frozen or corrupted
        // terminal even though the process underneath is fine. PTYs on
        // macOS/Linux have no equivalent to a console codepage — any
        // locale-aware program instead decides its own text encoding from
        // `LANG`/`LC_ALL`, so forcing a UTF-8 locale here is the correct
        // counterpart. Set unconditionally, before `extra_env` below, so an
        // explicit value the caller supplies (Electron always sends one)
        // still wins — this only fills the gap for callers that don't,
        // chiefly this binary's own standalone native-GUI mode, which used
        // to hand the shell an empty environment.
        let lang = std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_owned());
        command.env(
            "LC_ALL",
            std::env::var("LC_ALL").unwrap_or_else(|_| lang.clone()),
        );
        command.env("LANG", lang);
        // Session restore, the port of the shell's `terminalSnapshots`: a
        // terminal that survived an app restart keeps a saved tail — its
        // scrollback is re-fed below, and unless the caller picked a cwd the
        // fresh shell opens where the old session left off (OSC 7).
        let saved = terminal_state_file(&id)
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let saved_cwd = saved
            .as_ref()
            .and_then(|s| s.get("cwd"))
            .and_then(serde_json::Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        // A renamed terminal keeps its label across restarts, not just its
        // scrollback — the names map is what `slate terminal ls` and the
        // widget header both read.
        let saved_name = saved
            .as_ref()
            .and_then(|s| s.get("name"))
            .and_then(serde_json::Value::as_str)
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        let state_cwd = cwd
            .clone()
            .or(saved_cwd)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|path| path.to_str().map(str::to_owned))
            })
            .unwrap_or_default();
        if let Some(cwd) = cwd {
            command.cwd(cwd);
        } else {
            command.cwd(&state_cwd);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }

        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("spawn {shell}: {error}"))?;
        let child_pid = child.process_id();

        // The shell is running from here on. Every failure below used to
        // return with `?`, which drops the child handle without killing
        // anything — portable_pty's Child, like std's, does not kill on drop.
        // That left a live shell (and whatever it starts) with no handle, no
        // map entry and no way to ever reach it again. These failures are
        // resource exhaustion, which is exactly when leaking one per attempt
        // makes things worse. The concurrent-spawn path further down already
        // tears its own child down for the same reason.
        macro_rules! abandon_child {
            ($child:expr, $message:expr) => {{
                if let Some(pid) = child_pid {
                    kill_process_tree(pid);
                }
                let _ = $child.kill();
                let _ = $child.wait();
                return Err($message);
            }};
        }

        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(error) => abandon_child!(child, format!("clone pty reader: {error}")),
        };
        let mut writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(error) => abandon_child!(child, format!("take pty writer: {error}")),
        };
        prime_conpty_handshake(&mut writer);
        let state = Arc::new(Mutex::new(TerminalState {
            alive: true,
            cwd: state_cwd,
            ..Default::default()
        }));
        // The parser's default grid (32x120) matches TerminalManager::spawn;
        // explicit spawn sizes must be mirrored or the rendered pane wraps
        // differently than the PTY does.
        lock_recover(&state).screen.resize(rows.max(1), cols.max(1));
        // Re-feed the saved tail through the parser before the new shell's
        // first output lands: the pane opens showing the old session's
        // scrollback with a fresh prompt appended beneath — the shell's
        // `terminalRestoreData` writePaced replay.
        if let Some(output) = saved
            .as_ref()
            .and_then(|s| s.get("output"))
            .and_then(serde_json::Value::as_str)
        {
            let mut current = lock_recover(&state);
            current.screen.process(output.as_bytes());
            // The replayed tail may end inside a TUI's armed modes — the
            // original's terminalRestore wrote APP_OWNED_MODE_RESET before
            // announcing the fresh process.
            let restore = format!(
                "{}\r\n\x1b[90m[Session restored — new process started]\x1b[0m\r\n",
                slate_app::terminal_mode_recovery::APP_OWNED_MODE_RESET
            );
            current.screen.process(restore.as_bytes());
            // The history is terminal output that never passed through the
            // output path, so the recovery probes never saw it — a child-exit
            // title sitting at the tail is exactly the evidence a crashed
            // agent's leftover modes are noticed by.
            let _ = current.recovery.watch(output);
            let _ = current.screen.take_replies();
            let _ = current.screen.take_clipboard_writes();
            current.output.push_str(output);
            current.output.push_str(&restore);
        }
        let input_guard = Arc::new(Mutex::new(()));
        let pending_input = Arc::new(AtomicUsize::new(0));
        let writing_since = Arc::new(AtomicU64::new(0));
        let output_gate = Arc::new((Mutex::new(false), Condvar::new()));
        // Two channels, two threads: a write into a PTY blocks for as long as
        // the child refuses to drain it, so the thread that writes must never
        // be the thread that answers a resize, a dispose or an interrupt.
        // One shared channel is what let a single stuck keystroke freeze
        // everything about a terminal, Ctrl+C and close included.
        let (input_tx, input_rx) = mpsc::channel::<TerminalInput>();
        let (control_tx, control_rx) = mpsc::channel::<ControlCommand>();
        let events = Arc::clone(&self.inner);
        let reader_state = Arc::clone(&state);
        let reader_id = id.clone();
        let reader_gate = Arc::clone(&output_gate);
        let startup_input = cfg!(windows).then(|| (input_tx.clone(), Arc::clone(&pending_input)));

        if let Err(error) = thread::Builder::new()
            .name(format!("slate-pty-reader-{id}"))
            .spawn(move || {
                read_output(
                    reader,
                    reader_state,
                    events,
                    reader_id,
                    startup_input,
                    reader_gate,
                )
            })
        {
            abandon_child!(child, format!("spawn pty reader: {error}"));
        }

        let writer_pending = Arc::clone(&pending_input);
        let writer_writing_since = Arc::clone(&writing_since);
        let writer_control = control_tx.clone();
        let writer_id = id.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("slate-pty-writer-{id}"))
            .spawn(move || {
                run_writer(
                    writer,
                    input_rx,
                    writer_control,
                    writer_pending,
                    writer_writing_since,
                    writer_id,
                )
            })
        {
            abandon_child!(child, format!("spawn pty writer: {error}"));
        }

        let actor_state = Arc::clone(&state);
        // Weak, not Arc: `Inner` owns the map that owns this actor's command
        // sender, so a strong reference here would keep that sender alive
        // through the actor itself and its `recv()` could never end.
        let actor_inner = Arc::downgrade(&self.inner);
        let actor_pending = Arc::clone(&pending_input);
        let actor_id = id.clone();
        let actor_exit_tx = control_tx.clone();
        thread::Builder::new()
            .name(format!("slate-pty-actor-{id}"))
            .spawn(move || {
                run_actor(
                    pair.master,
                    child,
                    child_pid,
                    control_rx,
                    actor_state,
                    actor_inner,
                    actor_pending,
                    actor_id,
                    actor_exit_tx,
                )
            })
            .map_err(|error| {
                // The closure — and with it the child handle — is dropped when
                // the thread fails to start, so the pid is all that is left to
                // clean up with. Killing the tree also closes the PTY, which
                // ends the reader thread started just above.
                if let Some(pid) = child_pid {
                    kill_process_tree(pid);
                }
                format!("spawn pty actor: {error}")
            })?;

        {
            let mut terminals = lock_recover(&self.inner.terminals);
            // Re-check: a concurrent spawn for the same id may have won while
            // the PTY was created unlocked. The loser tears down its own
            // just-created child instead of orphaning it.
            if let Some(existing) = terminals.get(&id) {
                if lock_recover(&existing.state).alive {
                    drop(terminals);
                    if let Some(pid) = child_pid {
                        kill_process_tree(pid);
                    }
                    let _ = control_tx.send(ControlCommand::Dispose);
                    return Ok(true);
                }
                terminals.remove(&id);
            }
            terminals.insert(
                id.clone(),
                TerminalHandle {
                    input_tx,
                    control_tx,
                    state,
                    input_guard,
                    pending_input,
                    writing_since,
                    output_gate,
                    child_pid,
                },
            );
        }
        if let Some(name) = saved_name {
            self.set_name(&id, &name);
        }
        Ok(false)
    }

    /// Snapshot every live terminal's scrollback tail + tracked cwd to
    /// `terminal-state/<id>.json` — the shell's `terminalSnapshots` feature:
    /// on the next launch each respawned shell opens where the old one left
    /// off, its old scrollback already on screen.
    pub fn save_terminal_states(&self) {
        let Some(dir) = terminal_state_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let names = self.names();
        for (id, handle) in lock_recover(&self.inner.terminals).iter() {
            let state = lock_recover(&handle.state);
            let mut tail_start = state.output.len().saturating_sub(SAVED_SCROLLBACK);
            while !state.output.is_char_boundary(tail_start) {
                tail_start += 1;
            }
            let payload = serde_json::json!({
                "cwd": state.screen.cwd().unwrap_or_else(|| state.cwd.clone()),
                "name": names.get(id.as_str()),
                "output": &state.output[tail_start..],
            });
            let path = dir.join(format!("{}.json", sanitize_id(id)));
            if std::fs::write(&path, payload.to_string()).is_err() {
                continue;
            }
        }
    }

    pub fn dispose(&self, id: &str) -> Result<(), String> {
        self.dispose_inner(id, false)
    }

    fn dispose_inner(&self, id: &str, keep_state: bool) -> Result<(), String> {
        let handle = lock_recover(&self.inner.terminals)
            .remove(id)
            .ok_or_else(|| format!("unknown terminal {id}"))?;
        set_output_gate(&handle.output_gate, false);
        stop_terminal_events(&self.inner, id);
        // Freeing the name lets the pool hand it to the next terminal.
        lock_recover(&self.inner.names).remove(id);
        // A terminal the user closed is gone for good — its saved session
        // must not resurrect if a later spawn reuses the id. App teardown
        // (`dispose_all`) keeps the files instead: those are exactly the
        // sessions the next launch replays.
        if !keep_state {
            if let Some(path) = terminal_state_file(id) {
                let _ = std::fs::remove_file(path);
            }
        }
        // Kill the whole tree: the actor may be blocked behind a stuck write,
        // and `child.kill()` alone would orphan grandchildren. Killing runs on
        // its own thread because `taskkill` takes ~100ms and this call sits on
        // the engine's command loop, which must stay free to pump output.
        if let Some(pid) = handle.child_pid {
            let _ = thread::Builder::new()
                .name(format!("slate-pty-kill-{id}"))
                .spawn(move || {
                    kill_process_tree(pid);
                });
        }
        handle
            .control_tx
            .send(ControlCommand::Dispose)
            .map_err(|_| format!("terminal {id} actor stopped"))
    }

    pub fn dispose_all(&self) {
        let ids: Vec<String> = lock_recover(&self.inner.terminals)
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let _ = self.dispose_inner(&id, true);
        }
    }

    #[allow(dead_code)]
    pub fn write_raw(&self, id: &str, data: &[u8]) -> Result<(), String> {
        self.deliver_input(id, data.to_vec())
    }

    pub fn write_message(&self, id: &str, text: &str) -> Result<DeliveryReceipt, String> {
        let bracketed = lock_recover(&self.handle(id)?.state).bracketed_paste;
        let bytes = encode_terminal_message_input(text, bracketed)?;
        self.write_encoded(id, bytes, true)
    }

    pub fn write_text(
        &self,
        id: &str,
        text: &str,
        press_enter: bool,
    ) -> Result<DeliveryReceipt, String> {
        let bytes = encode_terminal_input(text, false)?;
        self.write_encoded(id, bytes, press_enter)
    }

    fn write_encoded(
        &self,
        id: &str,
        bytes: Vec<u8>,
        press_enter: bool,
    ) -> Result<DeliveryReceipt, String> {
        let count = bytes.len() + usize::from(press_enter);
        let handle = self.handle(id)?;
        let (tx, rx) = mpsc::channel();
        {
            let _guard = lock_recover(&handle.input_guard);
            if write_in_flight_ms(&handle.writing_since) >= STUCK_WRITE_MS
                || handle
                    .pending_input
                    .load(Ordering::Acquire)
                    .saturating_add(count)
                    > MAX_PENDING_INPUT_BYTES
            {
                return Err(format!("terminal {id} is not reading input"));
            }
            push_terminal_input(
                id,
                &handle,
                TerminalInput {
                    data: bytes,
                    press_enter,
                    acknowledgement: Some(tx),
                },
            )?;
        }
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| format!("terminal {id}: input write not confirmed; delivery is uncertain, do not resend automatically"))??;
        Ok(DeliveryReceipt {
            id: format!("delivery-{}", uuid::Uuid::new_v4().simple()),
            terminal_id: id.to_owned(),
            status: "delivered",
            bytes: count,
            confirmed_at: now_millis(),
            evidence: "terminal-output",
        })
    }

    #[allow(dead_code)]
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let handle = self.handle(id)?;
        handle
            .control_tx
            .send(ControlCommand::Resize { cols, rows })
            .map_err(|_| format!("terminal {id} actor stopped"))?;
        // The parsed screen must agree with the kernel PTY size or the pane
        // wraps differently than the shell believes it should.
        lock_recover(&handle.state)
            .screen
            .resize(rows.max(1), cols.max(1));
        Ok(())
    }

    /// The process-exit marker: the full app-owned-mode reset followed by a
    /// dim `[Process exited]` line (with the code when the waiter saw one), so
    /// a crashed full-screen TUI never strands the pane in alternate-screen or
    /// mouse-reporting modes.
    pub fn mark_exited(&self, id: &str, code: Option<u32>) {
        if let Ok(handle) = self.handle(id) {
            let reset = slate_app::terminal_mode_recovery::APP_OWNED_MODE_RESET;
            let marker = match code {
                Some(code) => {
                    format!("{reset}\r\n\x1b[90m[Process exited (code {code})]\x1b[0m\r\n")
                }
                None => format!("{reset}\r\n\x1b[90m[Process exited]\x1b[0m\r\n"),
            };
            lock_recover(&handle.state)
                .screen
                .process(marker.as_bytes());
        }
    }

    /// `scrollOnUserInput` — typing snaps the pane to the live edge.
    pub fn scroll_to_bottom(&self, id: &str) -> Result<(), String> {
        let handle = self.handle(id)?;
        lock_recover(&handle.state).screen.scroll_to_bottom();
        Ok(())
    }

    /// Line-granular scrollback nav — the xterm Shift+PageUp/Down/Arrow keys.
    /// `lines` is positive toward history, negative toward the live edge.
    pub fn scroll_lines(&self, id: &str, lines: i32) -> Result<(), String> {
        let handle = self.handle(id)?;
        lock_recover(&handle.state).screen.scroll(lines);
        Ok(())
    }

    /// Visible rows — the "page" in Shift+PageUp/Down.
    pub fn screen_rows(&self, id: &str) -> usize {
        self.handle(id)
            .map(|h| lock_recover(&h.state).screen.screen().size().0 as usize)
            .unwrap_or(24)
    }

    pub fn scroll_pixels(&self, id: &str, pixels: f32, row_height: f32) -> Result<(), String> {
        let handle = self.handle(id)?;
        lock_recover(&handle.state)
            .screen
            .scroll_pixels(pixels, row_height);
        Ok(())
    }

    /// First `terminal-N` not already in use — what `slate terminal new` and
    /// `worker-start` without a terminal fall back to.
    pub fn fresh_terminal_id(&self) -> String {
        let terminals = lock_recover(&self.inner.terminals);
        for n in 1..10_000u32 {
            let id = format!("terminal-{n}");
            if !terminals.contains_key(&id) {
                return id;
            }
        }
        format!("terminal-{}", now_millis())
    }

    /// One keystroke from the GUI. The terminal's own screen state decides
    /// the byte encoding (bracketed paste, application cursor keys); an
    /// unmappable key is a no-op rather than an error.
    pub fn key_input(
        &self,
        id: &str,
        event: &slate_app::terminal_screen::KeyInput,
    ) -> Result<(), String> {
        let handle = self.handle(id)?;
        let mut guard = lock_recover(&handle.state);
        let state = &mut *guard;
        let bytes = state.screen.input(event);
        // Ctrl-C / Ctrl-D may kill the foreground TUI without cleanup,
        // leaving origin mode or its scroll region behind — the "typing at
        // the top" state. The armed check only acts on the self-evident
        // corpse state, so a TUI that handled the key is never disturbed.
        if bytes
            .as_ref()
            .is_some_and(|b| b.contains(&0x03) || b.contains(&0x04))
        {
            state.recovery.arm_interrupt(&mut state.screen);
        }
        drop(guard);
        let Some(bytes) = bytes else { return Ok(()) };
        if bytes.is_empty() {
            return Ok(());
        }
        enqueue_input(id, &handle, bytes)
    }

    /// Drive every terminal's recovery timers — the child-exit confirmation
    /// and the Ctrl-C/D corpse-state check — from the view's refresh tick.
    pub fn tick_mode_recovery(&self) {
        let states: Vec<Arc<Mutex<TerminalState>>> = lock_recover(&self.inner.terminals)
            .values()
            .map(|handle| Arc::clone(&handle.state))
            .collect();
        for state in states {
            let mut current = lock_recover(&state);
            let current = &mut *current;
            current.recovery.tick(&mut current.screen);
        }
    }

    /// One pointer event translated to cell coordinates at the UI boundary.
    /// The screen's mouse-reporting mode decides whether bytes reach the PTY
    /// at all — programs that never enabled a mouse mode see nothing.
    pub fn mouse_input(
        &self,
        id: &str,
        kind: slate_app::terminal_screen::MouseKind,
        col: u16,
        row: u16,
        modifiers: slate_app::terminal_screen::KeyMods,
    ) -> Result<(), String> {
        let handle = self.handle(id)?;
        let bytes = lock_recover(&handle.state)
            .screen
            .mouse_event(kind, col, row, modifiers);
        let Some(bytes) = bytes else { return Ok(()) };
        if bytes.is_empty() {
            return Ok(());
        }
        enqueue_input(id, &handle, bytes)
    }

    /// xterm's drag-selection: begin at a visible cell.
    pub fn selection_begin(&self, id: &str, col: u16, row: u16) -> Result<(), String> {
        let handle = self.handle(id)?;
        lock_recover(&handle.state).screen.selection_begin(col, row);
        Ok(())
    }

    /// Drag the selection head to a visible cell.
    pub fn selection_update(&self, id: &str, col: u16, row: u16) -> Result<(), String> {
        let handle = self.handle(id)?;
        lock_recover(&handle.state)
            .screen
            .selection_update(col, row);
        Ok(())
    }

    pub fn selection_clear(&self, id: &str) {
        if let Ok(handle) = self.handle(id) {
            lock_recover(&handle.state).screen.selection_clear();
        }
    }

    /// The highlight's `(visible_row, start_col, end_col)` spans — cheap
    /// enough to poll per-pointer-move, unlike a full snapshot.
    pub fn selection_ranges(&self, id: &str) -> Vec<(u16, u16, u16)> {
        self.handle(id)
            .map(|h| lock_recover(&h.state).screen.selection_ranges())
            .unwrap_or_default()
    }

    /// xterm's `getSelection` — the text under the highlight.
    pub fn selection_text(&self, id: &str) -> Option<String> {
        let handle = self.handle(id).ok()?;
        let state = lock_recover(&handle.state);
        state.screen.selection_text()
    }

    /// Window focus in/out, for programs that armed DECSET 1004.
    pub fn focus_input(&self, id: &str, gained: bool) -> Result<(), String> {
        let handle = self.handle(id)?;
        let bytes = lock_recover(&handle.state).screen.focus_event(gained);
        let Some(bytes) = bytes else { return Ok(()) };
        if bytes.is_empty() {
            return Ok(());
        }
        enqueue_input(id, &handle, bytes)
    }

    pub fn set_output_paused(&self, id: &str, paused: bool) -> Result<(), String> {
        let handle = self.handle(id)?;
        set_output_gate(&handle.output_gate, paused);
        Ok(())
    }

    pub fn snapshot(&self, id: &str) -> Result<TerminalSnapshot, String> {
        let handle = self.handle(id)?;
        let state = lock_recover(&handle.state);
        Ok(TerminalSnapshot {
            id: id.to_owned(),
            output: state.output.clone(),
            screen: state.screen.screen().contents(),
            styled: state.screen.styled_rows(),
            alive: state.alive,
            // A program that reports its directory through OSC 7 knows better
            // than whatever cwd the terminal was spawned in.
            cwd: state.screen.cwd().unwrap_or_else(|| state.cwd.clone()),
            mouse_reporting: state.screen.mouse_reporting(),
            alternate: state.screen.alternate_screen(),
            selection: state.screen.selection_ranges(),
            last_data_at: state.last_output_at,
        })
    }

    /// `GET /terminal/{id}/output`, mirroring the TypeScript
    /// `fullOutput`/`readOutput` pair.
    ///
    /// * `full` hands back the whole retained scrollback and does not move
    ///   the read cursor.
    /// * Otherwise the read is incremental: it starts at the explicit
    ///   `offset`, else at the last `clear`ed position, both clamped to what
    ///   scrollback still retains (`output_base` … `output_base + len`), and
    ///   returns at most `limit` bytes — the `OUTPUT_BUFFER_LIMIT` of 50_000
    ///   by default.
    /// * `clear` commits the cursor past what was returned, so the next
    ///   incremental read sees only new output.
    pub fn read_terminal_output(
        &self,
        id: &str,
        full: bool,
        clear: bool,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String, String> {
        const OUTPUT_BUFFER_LIMIT: usize = 50_000;
        let handle = self.handle(id)?;
        let mut state = lock_recover(&handle.state);
        if full {
            return Ok(state.output.clone());
        }
        let end = state.output_base + state.output.len();
        let since = offset
            .unwrap_or(state.read_offset)
            .clamp(state.output_base, end);
        let limit = limit
            .unwrap_or(OUTPUT_BUFFER_LIMIT)
            .clamp(1, MAX_SCROLLBACK);
        let mut start = since - state.output_base;
        while !state.output.is_char_boundary(start) {
            start += 1;
        }
        let mut stop = (start + limit).min(state.output.len());
        while !state.output.is_char_boundary(stop) {
            stop += 1;
        }
        let data = state.output[start..stop].to_owned();
        if clear {
            state.read_offset = state.output_base + stop;
        }
        Ok(data)
    }

    pub fn snapshots(&self) -> Vec<TerminalSnapshot> {
        let mut ids: Vec<String> = lock_recover(&self.inner.terminals)
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| self.snapshot(&id).ok())
            .collect()
    }

    pub fn terminal_ids(&self) -> Vec<String> {
        lock_recover(&self.inner.terminals)
            .keys()
            .cloned()
            .collect()
    }

    /// Non-blocking drain, used by the egui frame loop.
    pub fn drain_events(&self) -> Vec<TerminalEvent> {
        let mut queue = lock_recover(&self.inner.events);
        let events = take_events(&mut queue);
        self.inner.events_ready.notify_all();
        events
    }

    /// Blocking drain, used by the headless engine so output is forwarded the
    /// moment it is read instead of on a polling tick — and so forwarding can
    /// never end up queued behind command handling.
    pub fn notify_response(&self) {
        lock_recover(&self.inner.events).notified = true;
        self.inner.events_ready.notify_one();
    }

    pub fn wait_events(&self) -> Vec<TerminalEvent> {
        let queue = lock_recover(&self.inner.events);
        let mut queue = self
            .inner
            .events_ready
            .wait_while(queue, |queue| queue.items.is_empty() && !queue.notified)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        queue.notified = false;
        let events = take_events(&mut queue);
        self.inner.events_ready.notify_all();
        events
    }

    fn handle(&self, id: &str) -> Result<TerminalHandle, String> {
        lock_recover(&self.inner.terminals)
            .get(id)
            .map(|handle| TerminalHandle {
                input_tx: handle.input_tx.clone(),
                control_tx: handle.control_tx.clone(),
                state: Arc::clone(&handle.state),
                input_guard: Arc::clone(&handle.input_guard),
                pending_input: Arc::clone(&handle.pending_input),
                writing_since: Arc::clone(&handle.writing_since),
                output_gate: Arc::clone(&handle.output_gate),
                child_pid: handle.child_pid,
            })
            .ok_or_else(|| format!("unknown terminal {id}"))
    }

    /// The one entry point for everything typed into a terminal.
    ///
    /// The guard keeps concurrent callers from interleaving their bytes. The
    /// check under it is the escape hatch for a terminal that has stopped
    /// reading: the Ctrl+C the user just pressed could only ever queue behind
    /// the write that is stuck, so it is escalated to the control thread —
    /// the one thread a blocked PTY cannot reach.
    fn deliver_input(&self, id: &str, data: Vec<u8>) -> Result<(), String> {
        let handle = self.handle(id)?;
        let _guard = lock_recover(&handle.input_guard);
        if is_interrupt(&data) {
            if write_in_flight_ms(&handle.writing_since) >= STUCK_WRITE_MS {
                handle
                    .control_tx
                    .send(ControlCommand::Interrupt)
                    .map_err(|_| format!("terminal {id} actor stopped"))?;
                // Reported as an error on purpose: the keystroke did not
                // reach the child (nothing can, in this state), and the
                // widget prints this, so the user learns why the terminal
                // went quiet and what was done about it instead of typing
                // into a void.
                return Err(format!(
                    "terminal {id} is not reading input; stopped what it was running"
                ));
            }
            // Backed up but still moving: the interrupt takes its turn behind
            // the input already queued, and is never refused for want of
            // queue space. The cap is there to bound bulk input, and one
            // keystroke — this keystroke above all — is not that.
            return push_input(id, &handle, data);
        }
        enqueue_input(id, &handle, data)
    }
}

/// Hand input to the terminal's writer thread without waiting for the write
/// to complete.
///
/// Waiting here used to stall the caller for up to a second per keystroke,
/// and the engine's command loop is a caller: a single shell that was slow to
/// read its input froze input *and* output for every terminal in the app,
/// which looked exactly like a hung session that only closing could fix. The
/// channel already guarantees ordering, so the only thing the wait bought was
/// error reporting — and a write that genuinely fails means the session is
/// gone, which the actor now reports as an exit instead.
fn enqueue_input(id: &str, handle: &TerminalHandle, data: Vec<u8>) -> Result<(), String> {
    // A single write that has been in flight this long means the child has
    // stopped draining its stdin. Accepting more input behind it would report
    // success for bytes that may never be delivered — the exact silence that
    // made a wedged terminal look alive until the widget was closed.
    if write_in_flight_ms(&handle.writing_since) >= STUCK_WRITE_MS {
        return Err(format!("terminal {id} is not reading input"));
    }
    let queued = handle.pending_input.load(Ordering::Acquire);
    if queued.saturating_add(data.len()) > MAX_PENDING_INPUT_BYTES {
        return Err(format!("terminal {id} is not reading input"));
    }
    push_input(id, handle, data)
}

/// Hand bytes to the writer thread. No capacity check: callers decide whether
/// this payload is allowed past the queue cap.
fn push_input(id: &str, handle: &TerminalHandle, data: Vec<u8>) -> Result<(), String> {
    push_terminal_input(
        id,
        handle,
        TerminalInput {
            data,
            press_enter: false,
            acknowledgement: None,
        },
    )
}

fn push_terminal_input(
    id: &str,
    handle: &TerminalHandle,
    input: TerminalInput,
) -> Result<(), String> {
    let len = input.data.len() + usize::from(input.press_enter);
    handle.pending_input.fetch_add(len, Ordering::AcqRel);
    handle.input_tx.send(input).map_err(|_| {
        // The writer thread is gone, so nothing will ever drain this. Undo
        // the reservation or the terminal would look permanently backed up.
        handle.pending_input.fetch_sub(len, Ordering::AcqRel);
        format!("terminal {id} actor stopped")
    })
}

/// True for the Ctrl+C keystroke itself, not for bulk input that happens to
/// carry the byte somewhere inside it.
fn is_interrupt(data: &[u8]) -> bool {
    data.len() <= MAX_INTERRUPT_INPUT_BYTES && data.contains(&INTERRUPT_BYTE)
}

/// The one thread that writes into a PTY.
///
/// Everything here is allowed to block. `write_all` into ConPTY (and into a
/// unix pty master) parks for as long as the child refuses to drain its
/// stdin, with no timeout available at this layer — which is exactly why it
/// gets a thread to itself. The control thread stays free to resize, kill or
/// interrupt the session while this one is parked, so a child that stopped
/// reading no longer takes the whole terminal down with it.
fn run_writer(
    mut writer: Box<dyn Write + Send>,
    rx: mpsc::Receiver<TerminalInput>,
    control_tx: mpsc::Sender<ControlCommand>,
    pending_input: Arc<AtomicUsize>,
    writing_since: Arc<AtomicU64>,
    id: String,
) {
    let mut last_bulk_write: Option<Instant> = None;
    while let Ok(input) = rx.recv() {
        // Published before the write and cleared after it: this is what lets
        // `enqueue_input` tell "busy for a moment" from "stopped reading",
        // and reject new input on the spot instead of queueing it behind
        // bytes that may never be delivered.
        writing_since.store(now_millis_u64(), Ordering::Release);
        // Electron sends text and Enter as separate raw requests. A slow
        // writer can receive both at once despite the caller's own delay.
        if input.data == b"\r" {
            if let Some(last) = last_bulk_write {
                thread::sleep(SUBMIT_GAP.saturating_sub(last.elapsed()));
            }
        }
        let result = writer
            .write_all(&input.data)
            .and_then(|_| writer.flush())
            .and_then(|_| {
                if input.press_enter {
                    // Delay at the actual writer, after the paste has drained, not
                    // at the caller where a backed-up queue can erase the gap.
                    thread::sleep(SUBMIT_GAP);
                    writer.write_all(b"\r")?;
                    writer.flush()?;
                }
                Ok(())
            });
        last_bulk_write = if input.data.len() > 1 && !input.press_enter {
            Some(Instant::now())
        } else {
            None
        };
        writing_since.store(0, Ordering::Release);
        pending_input.fetch_sub(
            input.data.len() + usize::from(input.press_enter),
            Ordering::AcqRel,
        );
        if let Some(ack) = input.acknowledgement {
            let _ = ack.send(
                result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| format!("write terminal {id}: {error}")),
            );
        }
        if let Err(error) = result {
            // A PTY whose writer is broken cannot be typed into again. The
            // control thread turns this into the same teardown a dispose
            // gets, so the widget shows an exit instead of silently
            // swallowing every keystroke.
            eprintln!("write terminal {id}: {error}");
            let _ = control_tx.send(ControlCommand::WriterFailed);
            break;
        }
    }
    // Whatever is still queued will never be written.
    pending_input.store(0, Ordering::Release);
    writing_since.store(0, Ordering::Release);
}

/// A terminal's control thread: resize, interrupt, dispose.
///
/// It owns the master and the child handle but never the writer, so none of
/// these can end up queued behind a write the child is not reading. That
/// queueing is what left closing the widget as the only way out of a wedged
/// terminal.
#[allow(clippy::too_many_arguments)]
fn run_actor(
    master: Box<dyn MasterPty + Send>,
    mut child: Box<dyn Child + Send + Sync>,
    child_pid: Option<u32>,
    rx: mpsc::Receiver<ControlCommand>,
    state: Arc<Mutex<TerminalState>>,
    inner: Weak<Inner>,
    _pending_input: Arc<AtomicUsize>,
    id: String,
    exit_tx: mpsc::Sender<ControlCommand>,
) {
    let mut disposed = false;
    let mut natural_exit = false;
    let mut killer = child.clone_killer();
    // The OS signals process exit directly. ConPTY's reader may not reach EOF
    // until this actor closes the master, so it cannot be our exit detector.
    let waiter = thread::Builder::new()
        .name(format!("slate-pty-wait-{id}"))
        .spawn(move || {
            let result = child.wait();
            let _ = exit_tx.send(match result {
                Ok(status) => ControlCommand::ChildExited {
                    code: status.exit_code(),
                },
                Err(_) => ControlCommand::WriterFailed,
            });
        });
    if let Err(error) = &waiter {
        eprintln!("wait terminal {id}: {error}");
        if let Some(pid) = child_pid {
            kill_process_tree(pid);
        }
        let _ = killer.kill();
    }
    while waiter.is_ok() {
        let Ok(command) = rx.recv() else {
            break;
        };
        match command {
            ControlCommand::ChildExited { code } => {
                lock_recover(&state).exit_code = Some(code);
                natural_exit = true;
                break;
            }
            ControlCommand::Resize { cols, rows } => {
                if let Err(error) = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    // Geometry is cosmetic: never end a session over it.
                    eprintln!("resize terminal {id}: {error}");
                }
            }
            ControlCommand::Interrupt => {
                // Reached only when the child has stopped reading its stdin,
                // so the \x03 the user typed cannot be delivered at all.
                // What the terminal is running is stopped directly instead;
                // the shell itself is deliberately left alive, because
                // closing the widget was the only way out before and that
                // took the whole session with it.
                if let Some(pid) = child_pid {
                    interrupt_process_tree(pid);
                }
            }
            ControlCommand::Dispose => {
                // Tree-kill first: the direct child's own exit does not stop
                // grandchildren (agents, dev servers), which would otherwise
                // keep running headless after the widget is closed.
                if let Some(pid) = child_pid {
                    kill_process_tree(pid);
                }
                let _ = killer.kill();
                disposed = true;
                break;
            }
            ControlCommand::WriterFailed => break,
        }
    }
    // ClosePseudoConsole may wait for its processes. Stop them before dropping
    // the master, including on a broken writer rather than explicit disposal.
    if !disposed {
        if let Some(pid) = child_pid {
            kill_process_tree(pid);
        }
        let _ = killer.kill();
    }
    // Closes the PTY, which ends the reader thread and releases the writer
    // thread if it is still parked inside a write.
    drop(master);
    lock_recover(&state).alive = false;
    if !disposed && !natural_exit {
        // The reader thread announces the ordinary end of a session, but it
        // only notices once the PTY reaches EOF. When the actor stops first
        // (broken writer, dropped handle) nothing else would ever tell the
        // app, and the widget would sit there accepting input that goes
        // nowhere. Duplicate exits are harmless — consumers ignore the second.
        if let Some(inner) = inner.upgrade() {
            let code = lock_recover(&state).exit_code;
            push_event(&inner, TerminalEvent::Exited { id, code });
        }
    }
}

fn set_output_gate(gate: &Arc<(Mutex<bool>, Condvar)>, paused: bool) {
    let (state, ready) = &**gate;
    *lock_recover(state) = paused;
    if !paused {
        ready.notify_all();
    }
}

fn allow_terminal_events(inner: &Arc<Inner>, id: &str) {
    lock_recover(&inner.events).stopped.remove(id);
    inner.events_ready.notify_all();
}

fn stop_terminal_events(inner: &Arc<Inner>, id: &str) {
    let mut queue = lock_recover(&inner.events);
    queue.stopped.insert(id.to_owned());
    let mut retained = VecDeque::with_capacity(queue.items.len());
    while let Some(event) = queue.items.pop_front() {
        let event_id = match &event {
            TerminalEvent::Output { id, .. }
            | TerminalEvent::Exited { id, .. }
            | TerminalEvent::Clipboard { id, .. } => id,
        };
        if event_id == id {
            queue.bytes = queue.bytes.saturating_sub(event_bytes(&event));
        } else {
            retained.push_back(event);
        }
    }
    queue.items = retained;
    inner.events_ready.notify_all();
}

fn push_event(inner: &Arc<Inner>, event: TerminalEvent) {
    let (id, bytes) = match &event {
        TerminalEvent::Output { id, data } => (id.as_str(), data.len()),
        TerminalEvent::Exited { id, .. } => (id.as_str(), 0),
        TerminalEvent::Clipboard { id, payload } => (id.as_str(), payload.len()),
    };
    let mut queue = lock_recover(&inner.events);
    while queue.bytes > 0
        && queue.bytes.saturating_add(bytes) > MAX_PENDING_EVENT_BYTES
        && !queue.stopped.contains(id)
    {
        queue = inner
            .events_ready
            .wait(queue)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    if queue.stopped.contains(id) {
        return;
    }
    queue.bytes = queue.bytes.saturating_add(bytes);
    queue.items.push_back(event);
    inner.events_ready.notify_all();
}

fn take_events(queue: &mut EventQueue) -> Vec<TerminalEvent> {
    let events = queue.items.drain(..).collect();
    queue.bytes = 0;
    events
}

fn event_bytes(event: &TerminalEvent) -> usize {
    match event {
        TerminalEvent::Output { data, .. } => data.len(),
        TerminalEvent::Exited { .. } => 0,
        TerminalEvent::Clipboard { payload, .. } => payload.len(),
    }
}

/// Milliseconds the writer thread has spent inside one write, or 0 when it
/// is not writing at all.
fn write_in_flight_ms(writing_since: &AtomicU64) -> u64 {
    let started = writing_since.load(Ordering::Acquire);
    if started == 0 {
        return 0;
    }
    // Saturating: a clock stepped backwards reads as "not stuck", which is
    // the safe direction — input keeps flowing instead of being refused.
    now_millis_u64().saturating_sub(started)
}

fn now_millis_u64() -> u64 {
    u64::try_from(now_millis()).unwrap_or(u64::MAX)
}

fn read_output(
    mut reader: Box<dyn Read + Send>,
    state: Arc<Mutex<TerminalState>>,
    inner: Arc<Inner>,
    id: String,
    startup_input: Option<(mpsc::Sender<TerminalInput>, Arc<AtomicUsize>)>,
    output_gate: Arc<(Mutex<bool>, Condvar)>,
) {
    let mut buffer = [0_u8; 16 * 1024];
    let mut decoder = Utf8Stream::default();
    // The tail of the previous chunk, so a mode toggle split across two reads
    // is still seen whole. `\x1b[?2004h` is eight bytes.
    let mut modes = slate_app::terminal_protocol::Decoder::default();
    let mut startup = slate_app::conpty_startup::StartupFilter::default();
    loop {
        let (paused, ready) = &*output_gate;
        let mut paused = lock_recover(paused);
        while *paused {
            paused = ready
                .wait(paused)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(paused);
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Ok(0) | Err(_) => 0,
            Ok(count) => count,
        };
        let filtered;
        let bytes = if let Some((sender, pending)) = &startup_input {
            let (mut output, replies) = startup.process(&buffer[..count]);
            if !replies.is_empty() {
                let size = replies.len();
                pending.fetch_add(size, Ordering::AcqRel);
                if sender
                    .send(TerminalInput {
                        data: replies,
                        press_enter: false,
                        acknowledgement: None,
                    })
                    .is_err()
                {
                    pending.fetch_sub(size, Ordering::AcqRel);
                }
            }
            if count == 0 {
                output.extend(startup.finish());
            }
            filtered = output;
            filtered.as_slice()
        } else {
            &buffer[..count]
        };
        let data = decoder.decode(bytes, count == 0);
        if data.is_empty() {
            if count == 0 {
                break;
            }
            continue;
        }
        for control in modes.advance(data.as_bytes()) {
            use slate_app::terminal_protocol::Control;
            match control {
                Control::Mode(2004, enabled) => lock_recover(&state).bracketed_paste = enabled,
                Control::Reset => lock_recover(&state).bracketed_paste = false,
                _ => {}
            }
        }
        let mut clipboards = Vec::new();
        // Streamed terminals keep no scrollback, but liveness is tracked
        // either way — the workers list reads it as `lastActiveAt`.
        if !inner.retain_scrollback {
            lock_recover(&state).last_output_at = now_millis_u64();
        }
        if inner.retain_scrollback {
            let mut current = lock_recover(&state);
            current.last_output_at = now_millis_u64();
            current.output.push_str(&data);
            current.screen.process(data.as_bytes());
            // The chunk is already parsed — this is the render-queue's
            // `afterPending` position in the shell. An echoed mouse report
            // means a dead application left tracking armed, so the reset
            // lands immediately rather than on the next tick.
            let echoed = current.recovery.watch(&data);
            if echoed {
                let state = &mut *current;
                state.recovery.recover(&mut state.screen);
            }
            // vt100 answers terminal queries itself (DSR, CPR, DA); the
            // program that asked is waiting on the PTY, so the reply must
            // be written back there or applications that probe stall.
            let replies = current.screen.take_replies();
            // OSC 52 writes ride the event stream so the view can put the
            // payloads on the real clipboard — still base64 here.
            clipboards = current.screen.take_clipboard_writes();
            if current.output.len() > MAX_SCROLLBACK + SCROLLBACK_SLACK {
                let mut cut = current.output.len() - MAX_SCROLLBACK;
                while !current.output.is_char_boundary(cut) {
                    cut += 1;
                }
                let boundary = current.output[cut..]
                    .find('\n')
                    .map(|offset| cut + offset + 1)
                    .unwrap_or(cut);
                current.output.drain(..boundary);
                current.output_base += boundary;
            }
            drop(current);
            if !replies.is_empty() {
                // Clone out of the map lock first: push_input takes the
                // terminal's input guard, and holding the map across it
                // would make map -> guard a lock order nobody else uses.
                let handle = lock_recover(&inner.terminals).get(&id).cloned();
                if let Some(handle) = handle {
                    let _ = push_input(&id, &handle, replies);
                }
            }
        }
        push_event(
            &inner,
            TerminalEvent::Output {
                id: id.clone(),
                data,
            },
        );
        for payload in clipboards {
            push_event(
                &inner,
                TerminalEvent::Clipboard {
                    id: id.clone(),
                    payload,
                },
            );
        }
        if count == 0 {
            break;
        }
    }
    let code = lock_recover(&state).exit_code;
    lock_recover(&state).alive = false;
    push_event(&inner, TerminalEvent::Exited { id, code });
}

#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            match std::str::from_utf8(&self.pending[offset..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    offset = self.pending.len();
                }
                Err(error) => {
                    let end = offset + error.valid_up_to();
                    text.push_str(std::str::from_utf8(&self.pending[offset..end]).unwrap());
                    offset = end;
                    if let Some(invalid) = error.error_len() {
                        text.push('\u{fffd}');
                        offset += invalid;
                    } else {
                        if eof {
                            text.push('\u{fffd}');
                            offset = self.pending.len();
                        }
                        break;
                    }
                }
            }
        }
        self.pending.drain(..offset);
        text
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn encode_terminal_input(text: &str, press_enter: bool) -> Result<Vec<u8>, String> {
    let line = text.replace(['\r', '\n'], " ");
    if line.trim().is_empty() {
        return Err("message must contain non-whitespace text".to_owned());
    }
    let mut bytes = line.into_bytes();
    if press_enter {
        bytes.push(b'\r');
    }
    Ok(bytes)
}

/// Encodes one delivery. The paste markers go on only when the receiving
/// program asked for them: `cmd.exe` and other plain shells never enable
/// DECSET 2004, and its line editor takes the leading escape as "clear the
/// line" — which ate all but the first character of every `slate tell`.
fn encode_terminal_message_input(text: &str, bracketed: bool) -> Result<Vec<u8>, String> {
    let line = text.replace("\r\n", "\n").replace('\r', "\n");
    if line.trim().is_empty() {
        return Err("message must contain non-whitespace text".to_owned());
    }
    if line
        .bytes()
        .any(|byte| (byte < b' ' && byte != b'\t' && byte != b'\n') || byte == 0x7f)
    {
        return Err("message contains unsafe terminal control characters".to_owned());
    }
    if !bracketed {
        // Without paste markers a newline is a submit, so a multi-line
        // message would run its first line and leave the rest as commands.
        return Ok(line.replace('\n', " ").into_bytes());
    }
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(line.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    Ok(bytes)
}

/// The state DECSET 2004 was last left in by this chunk of output.
#[cfg(test)]
fn bracketed_paste_toggle(data: &str) -> Option<bool> {
    slate_app::terminal_screen::last_mode_toggle(data.as_bytes(), 2004)
}

#[derive(Clone)]
pub struct ControlServer {
    socket_path: String,
}

impl ControlServer {
    /// Starts the control server on a named pipe (Windows) or unix domain
    /// socket, never on a TCP port.
    ///
    /// This matches what the Electron app does, and the reason is not
    /// aesthetic: no port is occupied so nothing can collide, access is
    /// governed by the operating system's permissions on the pipe rather than
    /// by "we only bound to loopback", and the command bus is not reachable
    /// from the network even in principle. `slate` finds either implementation at
    /// the same path without being told which is running.
    pub fn start(manager: TerminalManager, token: String) -> Result<Self, String> {
        let storage = slate_app::ipc::user_data_dir().join("orchestration.json");
        Self::start_at_with_storage(manager, token, slate_app::listener::default_path(), storage)
    }

    /// Starts on an explicit path. A test — or a second instance — uses this to
    /// stay off the real socket.
    #[cfg(test)]
    pub fn start_at(manager: TerminalManager, token: String, path: String) -> Result<Self, String> {
        let storage =
            std::env::temp_dir().join(format!("slate-test-{}.json", uuid::Uuid::new_v4()));
        Self::start_at_with_storage(manager, token, path, storage)
    }

    fn start_at_with_storage(
        manager: TerminalManager,
        token: String,
        path: String,
        orchestration_file: PathBuf,
    ) -> Result<Self, String> {
        let state = HttpState {
            manager,
            token,
            orchestration: Arc::new(Mutex::new(load_orchestration(&orchestration_file))),
            orchestration_file,
            rate_limiter: Arc::new(Mutex::new(ActorRateLimiter::default())),
            locks: Arc::new(Mutex::new(slate_app::locks::LockManager::new(None))),
        };

        // The listener is opened on this thread, before the server thread is
        // spawned, so a name already in use is reported to the caller rather
        // than printed into the void from a background thread.
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let listen_path = path.clone();

        thread::Builder::new()
            .name("slate-control-server".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(format!("control server runtime: {error}")));
                        return;
                    }
                };
                runtime.block_on(async move {
                    let listener = match slate_app::listener::bind(&listen_path).await {
                        Ok(listener) => listener,
                        Err(error) => {
                            let _ = ready_tx.send(Err(format!(
                                "bind control server at {listen_path}: {error}"
                            )));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));

                    let app = Router::new()
                        // Answered *before* the token check, exactly as in
                        // the TypeScript: presence is the discovery read — a
                        // CLI that knows nothing but the socket asks it. It
                        // carries no secrets, and the socket's filesystem
                        // permissions are the access boundary.
                        .route("/presence", get(presence))
                        .route("/raise", post(raise_window))
                        .route("/orchestration/workers", get(list_workers))
                        .route("/orchestration/workers/tell", post(tell_worker))
                        .route("/orchestration/workers/rename", post(rename_worker))
                        .route("/terminal/{id}/write", post(write_terminal))
                        .route("/terminal/{id}/attach", post(attach_terminal))
                        .route("/terminal/{id}/output", get(read_terminal_output))
                        // `POST /terminal` and `POST /widgets/terminal` are
                        // the same spawn in the TypeScript too (routeCanvas
                        // matches both spellings onto `terminal.create`).
                        .route("/terminal", post(spawn_terminal))
                        .route("/widgets/terminal", post(spawn_terminal))
                        .route("/terminal/{id}/rename", post(rename_terminal))
                        .route("/terminal/{id}", delete(dispose_terminal))
                        .route("/canvas/focus", post(canvas_focus))
                        .route("/screenshot", post(screenshot))
                        // Raw bytes — the JSON-envelope `media_route` stays
                        // for the socket client; real HTTP GET gets the file.
                        .route("/media/{name}", get(serve_media))
                        // Everything the ported router knows is served here, so
                        // adding a domain to `http::route` serves it without
                        // touching this file. The routes above stay explicit
                        // because they reach the terminal manager, the window,
                        // or the filesystem side of a capture — which the pure
                        // router deliberately does not see.
                        .fallback(handle_routed)
                        .layer(DefaultBodyLimit::max(MAX_HTTP_BODY_BYTES))
                        // Outermost, so it also sees the 413/415/405 answers
                        // the layers and routing below produce.
                        .layer(axum::middleware::from_fn(harden))
                        .with_state(state);
                    if let Err(error) = axum::serve(listener, app).await {
                        eprintln!("control server stopped: {error}");
                    }
                });
            })
            .map_err(|error| format!("spawn control server: {error}"))?;

        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "control server did not start".to_owned())??;

        Ok(Self { socket_path: path })
    }

    /// The pipe or socket path the server is listening on. Not an http address:
    /// there is no port, by design.
    pub fn socket_path(&self) -> String {
        self.socket_path.clone()
    }
}

fn load_orchestration(path: &FsPath) -> slate_app::orchestration::OrchestrationStore {
    let Ok(bytes) = fs::read(path) else {
        return slate_app::orchestration::OrchestrationStore::new();
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) if value.is_object() => {
            slate_app::orchestration::OrchestrationStore::load(&value)
        }
        Ok(_) | Err(_) => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let corrupt = PathBuf::from(format!("{}.corrupt-{stamp}", path.display()));
            let _ = fs::rename(path, corrupt);
            slate_app::orchestration::OrchestrationStore::new()
        }
    }
}

fn persist_orchestration(
    path: &FsPath,
    store: &slate_app::orchestration::OrchestrationStore,
) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| FsPath::new("."));
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".orchestration-{}-{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let data = serde_json::to_vec_pretty(&store.to_json()).map_err(std::io::Error::other)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[derive(Clone)]
struct HttpState {
    manager: TerminalManager,
    token: String,
    /// Shared because the fallback handler mutates it and axum hands each
    /// request its own clone of the state.
    orchestration: Arc<Mutex<slate_app::orchestration::OrchestrationStore>>,
    orchestration_file: PathBuf,
    rate_limiter: Arc<Mutex<ActorRateLimiter>>,
    /// `core.locks` in the TypeScript — in-memory resource locks behind the
    /// `/locks` routes.
    locks: Arc<Mutex<slate_app::locks::LockManager>>,
}

/// `detectRunning` from orchestration/workers.ts: which tool a terminal is
/// running, guessed from its title prefix or its output tail. The `~` prefix
/// marks a heuristic guess, distinguishing it from a dispatch-recorded agent.
fn detect_running(title: &str, tail: &str) -> Option<String> {
    const KNOWN_TOOLS: [&str; 11] = [
        "gray",
        "antigravity",
        "claude",
        "codex",
        "gemini",
        "opencode",
        "windsurf",
        "copilot",
        "aider",
        "cursor",
        "cline",
    ];
    const TAIL_TOOLS: [&str; 9] = [
        "gray",
        "antigravity",
        "claude",
        "codex",
        "gemini",
        "opencode",
        "windsurf",
        "copilot",
        "aider",
    ];
    // /^\s*([A-Za-z][A-Za-z0-9_+-]*)\s*:/ — a `claude:`-style title prefix.
    let trimmed = title.trim_start();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '+' || c == '-'))
        .unwrap_or(trimmed.len());
    if trimmed
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && trimmed[end..].trim_start().starts_with(':')
    {
        let head = trimmed[..end].to_ascii_lowercase();
        if KNOWN_TOOLS.contains(&head.as_str()) {
            return Some(format!("~{head}"));
        }
    }
    if tail.is_empty() {
        return None;
    }
    let text = strip_ansi(tail).to_lowercase();
    for name in TAIL_TOOLS {
        if contains_word(&text, name) {
            return Some(format!("~{name}"));
        }
    }
    None
}

/// The two regexes `detectRunning` applies to the tail — `\x1b[0-9;?]*[A-Za-z]`
/// (CSI) and `\x1b][^\x07\x9c\x1b]*(\x07|\x9c|\x1b\\)?` (OSC) — as a scanner:
/// each becomes a space so tool names can't hide inside escape bytes.
fn strip_ansi(data: &str) -> String {
    let bytes = data.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && matches!(bytes[j], b'0'..=b'9' | b';' | b'?') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                j += 1;
            }
            out.push(b' ');
            i = j;
        } else if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            let mut j = i + 2;
            while j < bytes.len() && !matches!(bytes[j], 0x07 | 0x9c | 0x1b) {
                j += 1;
            }
            // Optional terminator: BEL, 0x9c, or ESC \.
            if j < bytes.len() && matches!(bytes[j], 0x07 | 0x9c) {
                j += 1;
            } else if j + 1 < bytes.len() && bytes[j] == 0x1b && bytes[j + 1] == b'\\' {
                j += 2;
            }
            out.push(b' ');
            i = j;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `(^|[^a-z0-9_])name([^a-z0-9_]|$)` without a regex engine — the tool-name
/// scan runs once per terminal per listing.
fn contains_word(text: &str, name: &str) -> bool {
    let mut at = 0;
    while let Some(found) = text[at..].find(name) {
        let found = at + found;
        let before_ok = found == 0 || {
            let c = text.as_bytes()[found - 1];
            !(c.is_ascii_alphanumeric() || c == b'_')
        };
        let after = found + name.len();
        let after_ok = after >= text.len() || {
            let c = text.as_bytes()[after];
            !(c.is_ascii_alphanumeric() || c == b'_')
        };
        if before_ok && after_ok {
            return true;
        }
        at = found + 1;
    }
    false
}

/// The last `max` bytes of output on a char boundary — `tail.slice(-4000)`.
fn tail_of(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Append one chained entry to `command-journal.ndjson` — the same write path
/// the canvas and CLI use, so socket mutations replay identically.
fn commit_journal(
    actor: &str,
    entry_type: &str,
    target: &str,
    payload: serde_json::Value,
) -> Result<slate_app::journal::JournalEntry, String> {
    let path = slate_app::ipc::user_data_dir().join("command-journal.ndjson");
    slate_app::journal_log::JournalLog::open(&path)
        .and_then(|mut log| log.commit(actor, entry_type, target, payload))
}

/// The journal folded through `projection.rs` — the canvas the user sees.
/// Per-workspace: snapshot for the current slot plus the journal tail the
/// `workspaceDir` filter lets through.
fn folded_canvas() -> Result<slate_app::projection::CanvasState, String> {
    let path = slate_app::ipc::user_data_dir().join("command-journal.ndjson");
    let log = slate_app::journal_log::JournalLog::open(&path)?;
    Ok(slate_app::canvas_store::load_with_tail(slate_app::workspace::current().as_deref(), &log).0)
}

/// The `slate canvas` name ladder: exact id, case-insensitive title, then a
/// unique id prefix. Shared by the rename and screenshot resolutions.
fn find_widget<'a>(
    canvas: &'a slate_app::projection::CanvasState,
    name: &str,
) -> Result<&'a slate_app::projection::Widget, String> {
    if let Some(widget) = canvas.widgets.get(name) {
        return Ok(widget);
    }
    let lowered = name.to_lowercase();
    if let Some(widget) = canvas
        .widgets
        .values()
        .find(|widget| widget.title.to_lowercase() == lowered)
    {
        return Ok(widget);
    }
    let mut prefix = canvas
        .widgets
        .values()
        .filter(|widget| widget.id.starts_with(name));
    match (prefix.next(), prefix.next()) {
        (Some(widget), None) => Ok(widget),
        _ => Err(format!("no widget called '{name}'")),
    }
}

async fn list_workers(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    // The caller's own worker carries `self: true` — `slate workers` marks it
    // with a `*`. Identity is the same precedence the TypeScript uses: body,
    // then query, then the x-agent-id header (GETs have no body).
    let caller = uri
        .query()
        .and_then(|raw| {
            raw.split('&').find_map(|pair| {
                pair.split_once('=')
                    .filter(|(key, _)| *key == "agentId")
                    .map(|(_, value)| percent_decode_query(value))
            })
        })
        .or_else(|| {
            headers
                .get("x-agent-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
    check_rate_limit(&state, &headers, caller.as_deref())?;
    let names = state.manager.names();
    let workers = {
        let store = lock_recover(&state.orchestration);
        let task_titles: HashMap<String, String> = store
            .list_tasks(None, None, false)
            .iter()
            .map(|task| (task.id.clone(), task.title.clone()))
            .collect();
        state
            .manager
            .snapshots()
            .into_iter()
            .map(|snapshot| {
                let name = names
                    .get(&snapshot.id)
                    .cloned()
                    .unwrap_or_else(|| snapshot.id.clone());
                let live = store.dispatch_for_terminal(&snapshot.id);
                let tail = tail_of(&snapshot.output, 4_000);
                let running = live
                    .as_ref()
                    .map(|dispatch| dispatch.agent.clone())
                    .or_else(|| detect_running(&name, tail))
                    .unwrap_or_else(|| "shell".to_owned());
                let mut row = serde_json::json!({
                    "id": snapshot.id,
                    "name": name,
                    "busy": live.is_some(),
                    "self": caller.as_deref() == Some(snapshot.id.as_str()),
                    "alive": snapshot.alive,
                    "running": running,
                });
                if let Some(dispatch) = live {
                    row["agent"] = serde_json::json!(dispatch.agent);
                    row["dispatchId"] = serde_json::json!(dispatch.id);
                    row["taskId"] = serde_json::json!(dispatch.task_id);
                    if let Some(title) = task_titles.get(&dispatch.task_id) {
                        row["taskTitle"] = serde_json::json!(title);
                    }
                }
                if !snapshot.cwd.is_empty() {
                    row["cwd"] = serde_json::json!(snapshot.cwd);
                }
                if snapshot.last_data_at > 0 {
                    row["lastActiveAt"] = serde_json::json!(snapshot.last_data_at);
                }
                row
            })
            .collect::<Vec<_>>()
    };
    Ok(Json(serde_json::json!({ "workers": workers })))
}

#[derive(Deserialize)]
struct WriteRequest {
    #[serde(alias = "command", alias = "input", alias = "content")]
    text: Option<String>,
    #[serde(rename = "pressEnter", default = "default_true")]
    press_enter: bool,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct WriteResponse {
    ok: bool,
    id: String,
    text: String,
    delivery: Option<DeliveryReceipt>,
}

/// `slate tell <worker> "text"` — `POST /orchestration/workers/tell` in the
/// TypeScript. `to` resolves like every name-bearing route does, `images` are
/// media-store imports typed as path tokens, `confirmDelivery` is implicit:
/// the write already returns a receipt.
#[derive(Deserialize)]
struct TellRequest {
    to: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    images: Vec<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

async fn tell_worker(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(request): Json<TellRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    let manager = state.manager.clone();
    let to = state
        .manager
        .resolve(&request.to, agent.as_deref())
        .map_err(command_failure)?;
    let text = request.text.clone();
    if request.images.len() > 8 {
        return Err(bad_request("At most 8 images per delivery".into()));
    }
    let agent = lock_recover(&state.orchestration)
        .dispatch_for_terminal(&to)
        .map(|dispatch| dispatch.agent.clone());
    let sources = request.images.clone();
    let delivery_to = to.clone();
    let (receipt, images) = tokio::task::spawn_blocking(move || -> Result<_, String> {
        if sources.is_empty() {
            return manager
                .write_message(&delivery_to, &text)
                .map(|r| (r, Vec::<String>::new()));
        }
        if !manager.snapshot(&delivery_to)?.alive {
            return Err("Terminal has exited".into());
        }
        let directory = slate_app::ipc::user_data_dir().join("media");
        let images = sources
            .iter()
            .map(|source| {
                slate_app::attachments::import_image(std::path::Path::new(source), &directory)
                    .map(|path| path.to_string_lossy().into_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut content = String::new();
        for path in &images {
            content.push_str(&slate_app::attachments::path_token(path)?);
        }
        content.push_str(&text.replace(['\r', '\n'], " "));
        // This branch only runs when at least one image was attached, so the
        // delivery must be submitted even when the accompanying text is blank.
        let receipt = manager.write_text(&delivery_to, &content, true)?;
        Ok((receipt, images))
    })
    .await
    .map_err(|error| bad_request(format!("delivery task failed: {error}")))?
    .map_err(bad_request)?;
    // `submit('terminal.write'|'terminal.attach', …)` — the audit entry the
    // TypeScript journaled for every tell.
    let journaled = commit_journal(
        agent.as_deref().unwrap_or("api"),
        if images.is_empty() {
            "terminal.write"
        } else {
            "terminal.attach"
        },
        &format!("terminal:{to}"),
        serde_json::json!({
            "text": request.text,
            "images": images,
            "pressEnter": true,
            "confirmDelivery": true,
        }),
    );
    let mut response =
        serde_json::json!({"ok": true, "id": to, "text": request.text, "delivery": receipt});
    if let Some(agent) = agent {
        response["agent"] = serde_json::json!(agent);
    }
    if !images.is_empty() {
        response["images"] = serde_json::json!(images);
        response["mode"] = serde_json::json!("path");
    }
    let (version, seq) = journal_position(&journaled);
    Ok(Json(envelope(response, version, seq)))
}

/// `reply()`'s `{ok, version, seq, data}` around a flat answer. The flat fields
/// stay at the top level beside `data`, so `slate --json` callers keep reading
/// what they always read.
fn envelope(flat: serde_json::Value, version: i64, seq: u64) -> serde_json::Value {
    let mut body = serde_json::json!({
        "ok": true,
        "version": version,
        "seq": seq,
        "data": flat.clone(),
    });
    if let (Some(out), serde_json::Value::Object(fields)) = (body.as_object_mut(), flat) {
        for (key, value) in fields {
            out.insert(key, value);
        }
    }
    body
}

/// The version and seq a journal append landed at — zero when the journal
/// could not be written, which the caller already reports through the receipt.
fn journal_position(entry: &Result<slate_app::journal::JournalEntry, String>) -> (i64, u64) {
    entry
        .as_ref()
        .map(|entry| (entry.version.unwrap_or(0), entry.seq))
        .unwrap_or((0, 0))
}

async fn write_terminal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Path(id): Path<String>,
    Json(request): Json<WriteRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    let text = request
        .text
        .ok_or_else(|| bad_request("terminal.write requires text".to_owned()))?;
    // The shell refused writes over 64 KiB with 413 — a full file paste
    // belongs to terminal.attach, not a keystroke stream.
    if text.len() > 64 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "error": "terminal.write payload exceeds 64 KiB",
                "code": "invalid",
            })),
        ));
    }
    let manager = state.manager.clone();
    let press_enter = request.press_enter;
    let id = state
        .manager
        .resolve(&id, agent.as_deref())
        .map_err(command_failure)?;
    let delivery_id = id.clone();
    let delivery_text = text.clone();
    let receipt = tokio::task::spawn_blocking(move || {
        manager.write_text(&delivery_id, &delivery_text, press_enter)
    })
    .await
    .map_err(|error| bad_request(format!("delivery task failed: {error}")))?
    .map_err(bad_request)?;
    // `submit('terminal.write', …)` — the delivered write was journaled in
    // the TypeScript; the record is the audit trail, the PTY is the effect.
    let journaled = commit_journal(
        agent.as_deref().unwrap_or("api"),
        "terminal.write",
        &format!("terminal:{id}"),
        serde_json::json!({ "text": &text, "pressEnter": press_enter }),
    );
    let flat = serde_json::to_value(WriteResponse {
        ok: true,
        id,
        text,
        delivery: Some(receipt),
    })
    .unwrap_or_default();
    let (version, seq) = journal_position(&journaled);
    Ok(Json(envelope(flat, version, seq)))
}

#[derive(Serialize)]
struct OutputResponse {
    output: String,
}

async fn read_terminal_output(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
) -> Result<Json<OutputResponse>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    check_rate_limit(&state, &headers, params.get("agentId").map(String::as_str))?;
    // `full=1` is the whole scrollback; otherwise the read is incremental
    // (`?clear=1` commits the cursor, `?offset=`/`?limit=` slice it) — the
    // `terminals.readOutput` semantics the TypeScript server exposed.
    let truthy = |key: &str| {
        matches!(
            params.get(key).map(String::as_str),
            Some("1" | "true" | "yes")
        )
    };
    let id = state
        .manager
        .resolve(&id, params.get("agentId").map(String::as_str))
        .map_err(command_failure)?;
    let output = state
        .manager
        .read_terminal_output(
            &id,
            truthy("full"),
            truthy("clear"),
            params.get("offset").and_then(|v| v.parse::<usize>().ok()),
            params.get("limit").and_then(|v| v.parse::<usize>().ok()),
        )
        .map_err(not_found)?;
    Ok(Json(OutputResponse { output }))
}

#[derive(Deserialize)]
struct SpawnRequest {
    id: Option<String>,
    /// `POST /widgets/terminal` calls it `title`, `POST /terminal` calls it
    /// `name` — the TypeScript aliases them the same way.
    #[serde(alias = "title")]
    name: Option<String>,
    cwd: Option<String>,
    command: Option<String>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

/// `slate terminal new`: a shell on a fresh PTY. The canvas adopts it as a
/// widget on its next snapshot pass, so spawning over the socket is the same
/// as pressing T — this is also how `worker-start` without a terminal gets
/// one.
async fn spawn_terminal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(request): Json<SpawnRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    let id = request
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| state.manager.fresh_terminal_id());
    // `submit('terminal.create', 'terminal:new', …)` — the original journaled
    // the create before the PTY came up, so a failed spawn still leaves the
    // record of the attempt.
    commit_journal(
        agent.as_deref().unwrap_or("api"),
        "terminal.create",
        "terminal:new",
        serde_json::json!({
            "id": id,
            "title": request.name,
            "cwd": request.cwd,
            "agentOwned": true,
        }),
    )
    .map_err(bad_request)?;
    let manager = state.manager.clone();
    let spawn_id = id.clone();
    let launch = request
        .command
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let (id, name) = tokio::task::spawn_blocking(move || -> Result<_, String> {
        // `command` is a launch line, not an executable path — the original
        // always spawned the user's shell, and dispatch's launch path types
        // the agent command into it. Same here: exec'ing the raw string would
        // fail on anything with arguments.
        // No cwd in the request → the picked workspace, then the launch dir —
        // the same ladder `RouteDeps::workspace_dir` gives the routed path.
        let cwd = request
            .cwd
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(slate_app::workspace::current)
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            });
        manager.spawn_with_options(
            spawn_id.clone(),
            120,
            32,
            cwd,
            shell_command(),
            HashMap::new(),
        )?;
        if let Some(command) = &launch {
            let _ = manager.write_text(&spawn_id, command, true);
        }
        let name = request
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(name) = &name {
            manager.set_name(&spawn_id, name);
        }
        Ok((spawn_id, name))
    })
    .await
    .map_err(|error| bad_request(format!("spawn task failed: {error}")))?
    .map_err(bad_request)?;
    // The TypeScript answered `terminal.create`'s data: `{id, title, cwd,
    // ready}` — callers (`slate terminal open` among them) unwrap `.data`.
    let cwd = state
        .manager
        .snapshot(&id)
        .map(|snapshot| snapshot.cwd)
        .unwrap_or_default();
    let title = name.clone().unwrap_or_else(|| id.clone());
    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "name": name,
        "data": { "id": id, "title": title, "cwd": cwd, "ready": true },
    })))
}

#[derive(Deserialize)]
struct RenameRequest {
    name: String,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

/// `slate rename --to term-3 --name backend`. Names resolve in `tell`, the
/// worker list, and widget titles.
async fn rename_terminal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Path(id): Path<String>,
    Json(request): Json<RenameRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    let name = request.name.trim();
    if name.is_empty() {
        return Err(bad_request("rename needs a non-empty --name".into()));
    }
    let id = state
        .manager
        .resolve(&id, agent.as_deref())
        .map_err(command_failure)?;
    state.manager.snapshot(&id).map_err(not_found)?;
    state.manager.set_name(&id, name);
    Ok(Json(
        serde_json::json!({"ok": true, "id": id, "name": name}),
    ))
}

#[derive(Deserialize)]
struct WorkerRenameRequest {
    to: String,
    name: String,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

/// `slate rename --to term-3 --name backend` — `POST /orchestration/workers/
/// rename` in the TypeScript. Two paths: a terminal with no canvas widget is
/// renamed outright (`terminals.setTitle`); one the journal knows goes
/// through `widget.update` so the committed event is what the canvas replays.
async fn rename_worker(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(request): Json<WorkerRenameRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    let name = request.name.trim().chars().take(200).collect::<String>();
    if name.is_empty() {
        return Err(bad_request("name must not be empty".into()));
    }
    let id = state
        .manager
        .resolve(&request.to, agent.as_deref())
        .map_err(command_failure)?;
    let on_canvas = folded_canvas()
        .map(|canvas| canvas.widgets.contains_key(&id))
        .unwrap_or(false);
    if !on_canvas && state.manager.snapshot(&id).is_ok() {
        state.manager.set_name(&id, &name);
        return Ok(Json(
            serde_json::json!({"ok": true, "id": id, "name": name}),
        ));
    }
    if on_canvas {
        let actor = agent.as_deref().unwrap_or("api");
        commit_journal(
            actor,
            "widget.update",
            &format!("widget:{id}"),
            serde_json::json!({ "title": name }),
        )
        .map_err(bad_request)?;
        if state.manager.snapshot(&id).is_ok() {
            state.manager.set_name(&id, &name);
        }
        return Ok(Json(
            serde_json::json!({"ok": true, "id": id, "name": name}),
        ));
    }
    Err(not_found(format!("no worker called \"{}\"", request.to)))
}

/// The `/terminal/{id}/attach` body: text plus filesystem paths to deliver as
/// media-store imports — what `slate tell --image` sends.
#[derive(Deserialize)]
struct AttachRequest {
    #[serde(default)]
    text: String,
    #[serde(default)]
    images: Vec<String>,
    /// `imageList()` in the TypeScript merges a lone `image` into `images`.
    #[serde(default)]
    image: Option<String>,
    /// `pressEnter = text.length > 0` in the shell — bare image drops staged
    /// the tokens without submitting; with text they sent the line.
    #[serde(rename = "pressEnter")]
    press_enter: Option<bool>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

/// `POST /terminal/{id}/attach`: typed text plus file paths, imported into
/// the media store and typed as quoted path tokens — the same delivery
/// `tell` performs, reachable under the terminal's own route.
async fn attach_terminal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Path(id): Path<String>,
    Json(request): Json<AttachRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    let id = state
        .manager
        .resolve(&id, agent.as_deref())
        .map_err(command_failure)?;
    let sources: Vec<String> = {
        let mut list: Vec<String> = request
            .images
            .iter()
            .chain(request.image.iter())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        list.dedup();
        list
    };
    if sources.len() > 8 {
        return Err(bad_request("At most 8 images per delivery".into()));
    }
    // `terminal.attach` required at least one image — text-only calls are
    // writes and the shell sent them there.
    if sources.is_empty() {
        return Err(bad_request(
            "terminal.attach requires at least one image".into(),
        ));
    }
    let manager = state.manager.clone();
    let text = request.text.clone();
    let press_enter = request.press_enter.unwrap_or(!text.is_empty());
    let delivery_id = id.clone();
    let (receipt, images) = tokio::task::spawn_blocking(move || -> Result<_, String> {
        deliver_with_images(&manager, &delivery_id, &text, &sources, press_enter)
    })
    .await
    .map_err(|error| bad_request(format!("delivery task failed: {error}")))?
    .map_err(bad_request)?;
    // `submit('terminal.attach', …)` — same audit entry the TypeScript wrote.
    let _ = commit_journal(
        agent.as_deref().unwrap_or("api"),
        "terminal.attach",
        &format!("terminal:{id}"),
        serde_json::json!({
            "text": &request.text,
            "images": images,
            "pressEnter": press_enter,
        }),
    );
    let mut response = serde_json::json!({
        "ok": true, "id": id, "delivery": receipt,
    });
    if !images.is_empty() {
        response["images"] = serde_json::json!(images);
        response["mode"] = serde_json::json!("path");
    }
    Ok(Json(response))
}

/// The delivery `tell` and `attach` share: import each image into the media
/// store, type the path tokens followed by the text — or, with no images, a
/// plain message write.
fn deliver_with_images(
    manager: &TerminalManager,
    id: &str,
    text: &str,
    sources: &[String],
    press_enter: bool,
) -> Result<(DeliveryReceipt, Vec<String>), String> {
    if sources.is_empty() {
        let receipt = manager.write_text(id, text, press_enter)?;
        return Ok((receipt, Vec::new()));
    }
    if !manager.snapshot(id)?.alive {
        return Err("Terminal has exited".into());
    }
    let directory = slate_app::ipc::user_data_dir().join("media");
    let images = sources
        .iter()
        .map(|source| {
            slate_app::attachments::import_image(std::path::Path::new(source), &directory)
                .map(|path| path.to_string_lossy().into_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut content = String::new();
    for path in &images {
        content.push_str(&slate_app::attachments::path_token(path)?);
    }
    content.push_str(&text.replace(['\r', '\n'], " "));
    // This branch only runs when at least one image was attached, so the
    // delivery must be submitted even when the accompanying text is blank.
    let receipt = manager.write_text(id, &content, true)?;
    Ok((receipt, images))
}

/// `GET /presence` — the one route answered without the control token, so a
/// CLI that knows nothing but the socket can learn what is listening. Same
/// split as `isTrustedCaller` in the TypeScript: the socket's filesystem
/// permissions are the boundary, and the payload carries no secrets.
async fn presence(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    check_rate_limit(&state, &headers, None)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "app": "slate",
        "server": "slate-control",
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        // The active socket, not a captured path — `buildPresence` reads the
        // live one for the same reason (a fallback socket would be a lie).
        "socketPath": state.manager.control_socket(),
        // The picked workspace, not the process cwd — `slate ctx`/`doctor`
        // report `presence.workspaceDir || null`, so null is honest here.
        "workspaceDir": slate_app::workspace::current(),
    })))
}

/// `POST /raise`: a second `slate` invocation asks the running window to
/// come forward. The socket thread cannot touch the window, so the request
/// parks a flag the canvas view consumes on its next tick — the same mailbox
/// shape as `request_camera`/`take_camera_request`.
async fn raise_window(
    State(state): State<HttpState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    check_rate_limit(&state, &headers, None)?;
    // A headless instance has no window to raise — refuse so the second
    // `slate` reports the conflict instead of exiting on a raise that
    // parked a flag no canvas view will ever consume.
    if state.manager.is_headless() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "instance is running headless — no window to raise",
                "code": "conflict",
            })),
        ));
    }
    // Optional workspace handoff — the shell's `handleSecondInstanceArgs`
    // turned a path argv into `setWorkspaceDir`. A non-dir payload is
    // ignored, not fatal: the raise still applies.
    let dir = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("dir").and_then(|d| d.as_str()).map(str::to_owned))
        .filter(|d| std::path::Path::new(d).is_dir());
    state.manager.request_raise(dir);
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `slate terminal close <id>` — the original's `terminal.dispose`: kill the
/// PTY, fail its live dispatches, and take the canvas widget with it. The
/// canvas would notice the orphaned widget on its next tick anyway; the
/// `widget.remove` commit just makes the journal the prompt cause.
async fn dispose_terminal(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    check_rate_limit(&state, &headers, None)?;
    // DELETE carries `{agentId}` in its body when one exists; a bare
    // `curl -XDELETE` sends none, and the route must not 400 over that.
    let agent_id = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| value.get("agentId")?.as_str().map(str::to_owned));
    // `DELETE /widgets/:id`/`DELETE /terminal/:id` matched `terminals.has(id)`
    // exactly in the shell — names and prefixes were never resolved here, so
    // `DELETE /terminal/other` 404s instead of killing a neighbor.
    let exists = state.manager.snapshot(&id).is_ok();
    if !exists {
        return Err(not_found(format!("terminal \"{id}\" not found")));
    }
    state.manager.dispose(&id).map_err(not_found)?;
    // `submit('terminal.dispose', …)` — the journal records the kill before
    // the fallout is cleaned up, as the TypeScript did.
    let _ = commit_journal(
        agent_id
            .as_deref()
            .or_else(|| headers.get("x-agent-id").and_then(|v| v.to_str().ok()))
            .unwrap_or("api"),
        "terminal.dispose",
        &format!("terminal:{id}"),
        serde_json::json!({}),
    );
    // `failTerminalDispatches` — a running dispatch on a dead shell cannot
    // report, so it settles as failed rather than hanging forever.
    {
        let mut store = lock_recover(&state.orchestration);
        let live: Vec<String> = store
            .list_dispatches(None, None, Some(&id))
            .iter()
            .filter(|dispatch| dispatch.state == "running")
            .map(|dispatch| dispatch.id.clone())
            .collect();
        let mut touched = false;
        for dispatch_id in live {
            touched |= store
                .settle_dispatch(&dispatch_id, "failed", None, now_millis() as i64)
                .is_ok();
        }
        if touched {
            let _ = persist_orchestration(&state.orchestration_file, &store);
        }
    }
    // The `requestWidgetRemoval` half — only when the journal knows a widget
    // by this id; an adopted-in-flight terminal may not have one yet.
    if folded_canvas()
        .map(|canvas| canvas.widgets.contains_key(&id))
        .unwrap_or(false)
    {
        let _ = commit_journal(
            "api",
            "widget.remove",
            &format!("widget:{id}"),
            serde_json::json!({}),
        );
    }
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// `slate canvas focus`: park a camera request for the view's next tick.
/// The socket and the window live on different threads, so the exchange is a
/// one-slot mailbox on the manager.
#[derive(Deserialize)]
struct CanvasFocusRequest {
    id: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    zoom: Option<f64>,
    #[serde(rename = "agentId")]
    agent_id: Option<String>,
}

async fn canvas_focus(
    State(state): State<HttpState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
    Json(request): Json<CanvasFocusRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let agent = agent_identity(&headers, &uri, request.agent_id.as_deref());
    check_rate_limit(&state, &headers, agent.as_deref())?;
    if request.id.is_none() && request.x.is_none() && request.y.is_none() && request.zoom.is_none()
    {
        return Err(bad_request(
            "canvas focus needs an id or an x/y/zoom".into(),
        ));
    }
    // The id can be a canvas widget (`notes-3`), not only a terminal — a
    // resolution miss passes the raw id through to the canvas lookup.
    let widget_id = request.id.as_deref().map(|id| {
        state
            .manager
            .resolve(id, agent.as_deref())
            .unwrap_or_else(|_| id.to_owned())
    });
    state.manager.request_camera(CameraRequest {
        widget_id,
        x: request.x,
        y: request.y,
        zoom: request.zoom,
    });
    Ok(Json(serde_json::json!({"ok": true})))
}

/// `POST /screenshot` — `slate screenshot [--worker|--widget <name>]`. With no
/// target it is the whole Slate window; with one it is that canvas widget's
/// screen rect, exactly what the TypeScript's `capture(widgetId)` shot.
/// X11 only — `import` reads the root window and `xdotool` finds the window
/// by its WM_CLASS.
async fn screenshot(
    State(state): State<HttpState>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let parsed: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::json!({}))
    };
    let agent_id = parsed
        .get("agentId")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or_else(|| query.get("agentId").cloned());
    check_rate_limit(&state, &headers, agent_id.as_deref())?;
    // `worker`, `target`, `widget` and `widgetId` all name the same thing in
    // the original; the body wins over the query string.
    let raw = ["worker", "target", "widget", "widgetId"]
        .iter()
        .find_map(|key| parsed.get(key).and_then(|value| value.as_str()))
        .or_else(|| {
            ["worker", "widget", "target", "widgetId"]
                .iter()
                .find_map(|key| query.get(*key).map(String::as_str))
        })
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let manager = state.manager.clone();
    tokio::task::spawn_blocking(move || capture_screenshot(&manager, raw.as_deref()))
        .await
        .map_err(|error| bad_request(format!("screenshot task failed: {error}")))?
        .map(Json)
        .map_err(bad_request)
}

/// `GET /media/{name}` — the raw-bytes counterpart of http.rs `media_route`
/// (which answers the JSON socket protocol) and the port of the shell's
/// `orc://media` handler. Same single-component name guard; the Content-Type
/// is `mimeTypeForPath` over the full media table. Like the original's
/// `ACTIVE_EXTS` downgrade, markup/script extensions are served as plain text
/// with an attachment disposition so a fetched page can never execute.
async fn serve_media(
    State(state): State<HttpState>,
    headers: HeaderMap,
    axum::extract::Path(name): axum::extract::Path<String>,
) -> Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)> {
    authenticate(&headers, &state.token)?;
    let mut components = std::path::Path::new(&name).components();
    let single_name = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !single_name {
        return Err(bad_request("media needs a file name, not a path".to_owned()));
    }
    let extension = std::path::Path::new(&name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let active = slate_app::attachments::is_active_extension(&extension);
    let content_type = if active {
        "text/plain; charset=utf-8"
    } else {
        slate_app::attachments::mime_type_for_extension(&extension)
    };
    let file = slate_app::ipc::user_data_dir().join("media").join(&name);
    let bytes = std::fs::read(&file).map_err(|_| bad_request(format!("no such media: {name}")))?;
    let mut builder = axum::response::Response::builder()
        .header("content-type", content_type)
        .header("x-content-type-options", "nosniff")
        .header("content-security-policy", "default-src 'none'; sandbox");
    if active {
        builder = builder.header("content-disposition", "attachment");
    }
    Ok(builder.body(axum::body::Body::from(bytes)).unwrap())
}

/// The windowed portion of a screenshot request: find the Slate window, crop
/// `import` to it — or to a widget's screen rect inside it when the caller
/// named a target.
fn capture_screenshot(
    manager: &TerminalManager,
    target: Option<&str>,
) -> Result<serde_json::Value, String> {
    let windows = Command::new("xdotool")
        .args(["search", "--classname", "slate"])
        .output()
        .map_err(|e| format!("xdotool: {e}"))?;
    let window = String::from_utf8_lossy(&windows.stdout)
        .lines()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| "no Slate window is mapped".to_owned())?;
    // `import -window <id>` is flaky across ImageMagick builds; cropping the
    // root window by geometry is reliable everywhere.
    let geometry = Command::new("xdotool")
        .args(["getwindowgeometry", "--shell", &window])
        .output()
        .map_err(|e| format!("xdotool geometry: {e}"))?;
    let mut pos = (0i64, 0i64, 0i64, 0i64);
    for line in String::from_utf8_lossy(&geometry.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let parsed = value.trim().parse::<i64>().unwrap_or(0);
        match key {
            "X" => pos.0 = parsed,
            "Y" => pos.1 = parsed,
            "WIDTH" => pos.2 = parsed,
            "HEIGHT" => pos.3 = parsed,
            _ => {}
        }
    }
    let (wx, wy, ww, wh) = pos;
    if ww <= 0 || wh <= 0 {
        return Err("window has no geometry".to_owned());
    }
    let mut widget_id: Option<String> = None;
    let mut crop = pos;
    if let Some(name) = target {
        // Worker names resolve through the terminal table; widget names
        // through the journal — the same ladder `slate canvas` uses. A
        // resolution miss passes the raw name through (screenshots can be
        // taken outside a Slate terminal, so `self` has no caller).
        let resolved = manager
            .resolve(name, None)
            .unwrap_or_else(|_| name.to_owned());
        let canvas =
            folded_canvas().map_err(|error| format!("cannot read the canvas journal: {error}"))?;
        let widget = find_widget(&canvas, &resolved)
            .or_else(|_| find_widget(&canvas, name))
            .map_err(|_| {
                if manager.snapshot(&resolved).is_ok() {
                    format!("widget '{resolved}' is not on the canvas yet")
                } else {
                    format!("no widget called '{name}'")
                }
            })?;
        widget_id = Some(widget.id.clone());
        let camera = &canvas.camera;
        let zoom = camera.zoom.max(0.0001);
        // `widgetScreenRect`: maximized widgets fill the viewport under the
        // floating title band; everything else is the world rect through the
        // camera, with the canvas surface at window origin (Pos2::ZERO).
        let (sx, sy, sw, sh) = if widget.maximized {
            (
                0.0,
                slate_app::canvas::TITLE_BAR_HEIGHT as f64,
                ww as f64,
                (wh as f64 - slate_app::canvas::TITLE_BAR_HEIGHT as f64).max(0.0),
            )
        } else {
            (
                camera.x + widget.x * zoom,
                camera.y + widget.y * zoom,
                widget.w * zoom,
                widget.h * zoom,
            )
        };
        // Clamp to the window — a half-panned widget still has a shot to take.
        let left = (wx as f64 + sx).max(wx as f64);
        let top = (wy as f64 + sy).max(wy as f64);
        let right = (wx as f64 + sx + sw).min((wx + ww) as f64);
        let bottom = (wy as f64 + sy + sh).min((wy + wh) as f64);
        crop = (
            left as i64,
            top as i64,
            (right - left) as i64,
            (bottom - top) as i64,
        );
        if crop.2 <= 0 || crop.3 <= 0 {
            return Err(format!("widget '{}' is off-screen", widget.id));
        }
    }
    let (x, y, w, h) = crop;
    let dir = slate_app::ipc::user_data_dir().join("screenshots");
    fs::create_dir_all(&dir).map_err(|e| format!("screenshots dir: {e}"))?;
    let path = dir.join(format!(
        "slate-{}.png",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let status = Command::new("import")
        .args(["-window", "root", "-crop", &format!("{w}x{h}+{x}+{y}")])
        .arg(&path)
        .status()
        .map_err(|e| format!("import: {e}"))?;
    if !status.success() {
        return Err(format!("import exited {status}"));
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut data = serde_json::json!({ "path": path.to_string_lossy(), "name": name });
    if let Some(id) = widget_id {
        data["widgetId"] = serde_json::json!(id);
    }
    Ok(serde_json::json!({"ok": true, "data": data}))
}

/// Bridges the pure router to axum: authenticate, translate, answer.
///
/// The authorization check happens here, once, before anything is routed —
/// exactly as `isTrustedCaller` gates `route()` in the TypeScript. The router
/// itself never sees a token, so no route can forget to check one.
async fn handle_routed(
    State(state): State<HttpState>,
    headers: HeaderMap,
    method: axum::http::Method,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    if authenticate(&headers, &state.token).is_err() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "a valid control token is required" })),
        );
    }

    let mut query = indexmap::IndexMap::new();
    for pair in uri.query().unwrap_or_default().split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(key.to_owned(), percent_decode_query(value));
    }

    // GET carries no body in the TypeScript — it is never read.
    let parsed_body = if body.is_empty() || method == axum::http::Method::GET {
        serde_json::json!({})
    } else {
        match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "request body must be a JSON object",
                        "code": "invalid"
                    })),
                );
            }
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "request body must be valid JSON",
                        "code": "invalid"
                    })),
                );
            }
        }
    };
    // The shell's precedence: body > query > header.
    let agent_id = parsed_body
        .get("agentId")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .or_else(|| query.get("agentId").cloned())
        .or_else(|| {
            headers
                .get("x-agent-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        });
    if let Err(error) = check_rate_limit(&state, &headers, agent_id.as_deref()) {
        return error;
    }

    let request = slate_app::http::Request {
        method: method.as_str().to_owned(),
        path: uri.path().to_owned(),
        query,
        // A body that is absent or unparseable is null, which every route
        // treats as "no fields given" rather than as an error — the same as a
        // GET with no body at all.
        body: parsed_body,
        agent_id,
    };

    // `wait`+`timeoutMs` on the inbox is a server hold: the TypeScript kept
    // the request open until a message landed or the deadline passed, and
    // answered `{waited:true}` whenever it had actually waited.
    let wait_path = uri.path().to_owned();
    let wait_deadline = if method == axum::http::Method::GET
        && (wait_path == "/orchestration/inbox" || wait_path.starts_with("/orchestration/replies/"))
        && matches!(
            request.query.get("wait").map(String::as_str),
            Some("1") | Some("true")
        ) {
        let ms = request
            .query
            .get("timeoutMs")
            .and_then(|raw| raw.parse::<u64>().ok())
            .unwrap_or(900_000)
            .clamp(1_000, 900_000);
        Some(std::time::Instant::now() + std::time::Duration::from_millis(ms))
    } else {
        None
    };
    let mut waited = false;

    loop {
        // Every borrow the router needs ends inside this block, so nothing
        // !Send (the store guard, the env, the deps) lives across the
        // long-poll `.await` below.
        let routed: Result<_, (StatusCode, Json<serde_json::Value>)> = {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let mut store = lock_recover(&state.orchestration);
            let previous = store.to_json();
            // Routes spawn terminals and browse files under the picked
            // workspace; a missing pick falls back to the launch dir.
            let workspace_dir = slate_app::workspace::current()
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .unwrap_or_default();
            let mut env = EngineEnv {
                manager: &state.manager,
                locks: state.locks.as_ref(),
            };
            let mut deps = slate_app::http::RouteDeps {
                orchestration: &mut store,
                app_version: env!("CARGO_PKG_VERSION"),
                workspace_dir: Some(&workspace_dir),
                now,
                env: Some(&mut env),
            };

            match slate_app::http::route(&request, &mut deps) {
                Some(response) => {
                    if method != axum::http::Method::GET.as_str() && response.status < 400 {
                        if let Err(error) = persist_orchestration(&state.orchestration_file, &store)
                        {
                            *store = slate_app::orchestration::OrchestrationStore::load(&previous);
                            Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({
                                    "error": format!("failed to persist orchestration: {error}"),
                                    "code": "failed"
                                })),
                            ))
                        } else {
                            Ok(response)
                        }
                    } else {
                        Ok(response)
                    }
                }
                None => Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "no such route", "code": "not_found" })),
                )),
            }
        };
        let (status, mut body) = match routed {
            Ok(response) => (response.status, response.body),
            Err(error) => return error,
        };

        // Only successful responses are held — a 404 on a missing message
        // answers immediately, waiting or not.
        if status < 400 {
            if let Some(deadline) = wait_deadline {
                // Inbox waits on an empty `messages`; the replies route waits
                // on `reply` still being null.
                let empty = if wait_path == "/orchestration/inbox" {
                    body.get("messages")
                        .and_then(|messages| messages.as_array())
                        .is_none_or(|messages| messages.is_empty())
                } else {
                    body.get("reply").is_none_or(|reply| reply.is_null())
                };
                if empty && std::time::Instant::now() < deadline {
                    waited = true;
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    continue;
                }
                if waited {
                    body["waited"] = serde_json::json!(true);
                }
            }
        }
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(body),
        );
    }
}

/// The live-app capabilities `http::route` borrows while handling a request:
/// the PTY table, the lock manager, the journal on disk. Holding references
/// rather than cloning keeps every read the routes make the same object the
/// GUI reads — a rename lands in the names map the view also sees.
struct EngineEnv<'a> {
    manager: &'a TerminalManager,
    locks: &'a Mutex<slate_app::locks::LockManager>,
}

impl slate_app::http::RouteEnv for EngineEnv<'_> {
    fn journal(&mut self) -> Result<slate_app::journal_log::JournalLog, String> {
        let path = slate_app::ipc::user_data_dir().join("command-journal.ndjson");
        slate_app::journal_log::JournalLog::open(&path)
    }

    /// `terminals.list()` — the shell rows `/widgets` leads with. `title` and
    /// `name` are one value here: the names map is the only display label the
    /// canvas and the CLI share.
    fn terminals(&mut self) -> Vec<serde_json::Value> {
        let names = self.manager.names();
        self.manager
            .snapshots()
            .iter()
            .map(|snapshot| {
                let name = names.get(&snapshot.id).cloned();
                serde_json::json!({
                    "id": snapshot.id,
                    "name": name,
                    "title": name,
                    "kind": "terminal",
                    "alive": snapshot.alive,
                    "cwd": snapshot.cwd,
                    "lastActiveAt": snapshot.last_data_at,
                })
            })
            .collect()
    }

    fn terminal_exists(&mut self, id: &str) -> bool {
        self.manager.snapshot(id).is_ok()
    }

    fn dispose_terminal(&mut self, id: &str) -> Result<(), String> {
        self.manager.dispose(id)
    }

    fn set_terminal_title(&mut self, id: &str, title: &str) {
        self.manager.set_name(id, title);
    }

    fn resolve_terminal(
        &mut self,
        target: &str,
        caller: Option<&str>,
    ) -> Result<String, CommandError> {
        let id = self.manager.resolve(target, caller)?;
        self.manager
            .snapshot(&id)
            .map(|_| id.clone())
            .map_err(|_| CommandError::new(ErrorCode::NotFound, format!("no terminal \"{id}\"")))
    }

    fn fresh_terminal_id(&mut self) -> String {
        self.manager.fresh_terminal_id()
    }

    fn spawn_terminal(&mut self, id: &str, title: &str, cwd: &str) -> Result<(), String> {
        self.manager.spawn_with_options(
            id.to_owned(),
            120,
            32,
            if cwd.is_empty() {
                None
            } else {
                Some(cwd.to_owned())
            },
            shell_command(),
            std::collections::HashMap::new(),
        )?;
        self.manager.set_name(id, title);
        Ok(())
    }

    fn write_terminal(&mut self, id: &str, text: &str, press_enter: bool) -> Result<(), String> {
        self.manager.write_text(id, text, press_enter).map(|_| ())
    }

    fn tail_terminal(&mut self, id: &str, max: usize) -> Option<String> {
        let output = self
            .manager
            .read_terminal_output(id, true, false, None, None)
            .ok()?;
        Some(
            output
                .chars()
                .rev()
                .take(max)
                .collect::<String>()
                .chars()
                .rev()
                .collect(),
        )
    }

    fn locks(&self) -> std::sync::MutexGuard<'_, slate_app::locks::LockManager> {
        lock_recover(self.locks)
    }

    fn data_dir(&self) -> PathBuf {
        slate_app::ipc::user_data_dir()
    }

    fn socket_path(&self) -> Option<String> {
        self.manager.control_socket()
    }
}

/// Query values arrive percent-encoded, with `+` for a space.
fn percent_decode_query(raw: &str) -> String {
    let bytes = raw.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The shared identity read: `agentId` from the JSON body wins, then the
/// query string, then `x-agent-id` — the precedence the TypeScript applied
/// to every mutation route, so a caller that only sends the header still
/// rate-limits and journals under its own name.
fn agent_identity(
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    body: Option<&str>,
) -> Option<String> {
    body.filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            uri.query().and_then(|raw| {
                raw.split('&').find_map(|pair| {
                    pair.split_once('=')
                        .filter(|(key, _)| *key == "agentId")
                        .map(|(_, value)| percent_decode_query(value))
                })
            })
        })
        .or_else(|| {
            headers
                .get("x-agent-id")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
}

fn check_rate_limit(
    state: &HttpState,
    headers: &HeaderMap,
    hint: Option<&str>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let header = headers
        .get("x-agent-id")
        .and_then(|value| value.to_str().ok());
    let raw = hint
        .filter(|value| !value.trim().is_empty())
        .or(header)
        .unwrap_or("api")
        .trim();
    let key = raw
        .char_indices()
        .nth(128)
        .map(|(index, _)| &raw[..index])
        .unwrap_or(raw);
    let now = now_millis().min(i64::MAX as u128) as i64;
    if lock_recover(&state.rate_limiter).try_consume(key, 1.0, now) {
        Ok(())
    } else {
        Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "error": "429 Rate limit exceeded for actor",
                "code": "rate_limited",
            })),
        ))
    }
}

fn authenticate(
    headers: &HeaderMap,
    token: &str,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let supplied = headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if constant_time_token_eq(supplied, token) {
        Ok(())
    } else {
        // One body for every 401 — the shell answered "a valid control
        // token is required" on routed and axum paths alike.
        Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "a valid control token is required",
            })),
        ))
    }
}

fn constant_time_token_eq(supplied: &str, expected: &str) -> bool {
    let supplied_hash = Sha256::digest(supplied.as_bytes());
    let expected_hash = Sha256::digest(expected.as_bytes());
    supplied_hash
        .iter()
        .zip(expected_hash.iter())
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

fn not_found(error: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": error, "code": "not_found" })),
    )
}

fn bad_request(error: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error, "code": "invalid" })),
    )
}

/// A `CommandError` from worker resolution or the store, as the axum
/// handlers' `(StatusCode, Json)` pair — `{error, code}` like every routed
/// failure, so the CLI's envelope reader works on either half of the API.
fn command_failure(error: CommandError) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::from_u16(error.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(serde_json::json!({
            "error": error.message,
            "code": error.code.as_str(),
        })),
    )
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::windows_utf8_shell_args;
    use super::{
        constant_time_token_eq, encode_terminal_input, encode_terminal_message_input, take_events,
        EventQueue, TerminalEvent, WriteRequest, MAX_PENDING_EVENT_BYTES,
    };
    use serde_json::json;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    #[cfg(windows)]
    #[test]
    fn bundled_conpty_preserves_synchronized_cursor_commands() {
        use std::time::{Duration, Instant};
        let manager = super::TerminalManager::new("cursor-probe".into());
        manager.spawn("cursor-probe").unwrap();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(5) {
            if manager
                .snapshot("cursor-probe")
                .unwrap()
                .output
                .contains('>')
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let command = "powershell.exe -NoLogo -NoProfile -Command \"$e=[char]27; [Console]::Write($e+'[?2026h'+$e+'[?25l'+$e+'[4;9H'+'SLATE_CURSOR_PROBE'+$e+'[6 q'+$e+'[?25h'+$e+'[?2026l')\"\r";
        manager
            .write_raw("cursor-probe", command.as_bytes())
            .unwrap();
        let expected =
            "\x1b[?2026h\x1b[?25l\x1b[4;9HSLATE_CURSOR_PROBE\x1b[6 q\x1b[?25h\x1b[?2026l";
        let started = Instant::now();
        let preserved = loop {
            if manager
                .snapshot("cursor-probe")
                .unwrap()
                .output
                .contains(expected)
            {
                break true;
            }
            if started.elapsed() > Duration::from_secs(10) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        manager.dispose("cursor-probe").unwrap();
        assert!(preserved, "ConPTY rewrote the synchronized cursor frame");
    }

    #[test]
    fn line_normalization_rejects_space_only_messages() {
        assert!(encode_terminal_input("   ", true).is_err());
        assert_eq!(encode_terminal_input("a\r\nb", true).unwrap(), b"a  b\r");
        assert_eq!(encode_terminal_input("a", false).unwrap(), b"a");
    }

    #[test]
    fn message_input_uses_bracketed_paste_markers_only_where_they_are_understood() {
        assert_eq!(
            encode_terminal_message_input("hello\r\nworld", true).unwrap(),
            b"\x1b[200~hello\nworld\x1b[201~"
        );
        // cmd.exe reads the leading escape as "clear the line", so a plain
        // shell gets the bare text, flattened to the single line it submits.
        assert_eq!(
            encode_terminal_message_input("hello\r\nworld", false).unwrap(),
            b"hello world"
        );
    }

    #[test]
    fn message_input_rejects_bracketed_paste_terminators() {
        assert!(encode_terminal_message_input("safe\x1b[201~echo unsafe", true).is_err());
        assert!(encode_terminal_message_input("safe\x1b[201~echo unsafe", false).is_err());
    }

    #[test]
    fn the_last_paste_mode_toggle_in_a_chunk_wins() {
        assert_eq!(super::bracketed_paste_toggle("no modes here"), None);
        assert_eq!(super::bracketed_paste_toggle("\x1b[?2004h"), Some(true));
        assert_eq!(
            super::bracketed_paste_toggle("\x1b[?2004h out \x1b[?2004l"),
            Some(false)
        );
        assert_eq!(
            super::bracketed_paste_toggle("\x1b[?2004l out \x1b[?2004h"),
            Some(true)
        );
    }

    #[test]
    fn control_tokens_are_compared_without_early_exit() {
        assert!(constant_time_token_eq("secret", "secret"));
        assert!(!constant_time_token_eq("secret", "secrex"));
        assert!(!constant_time_token_eq("", "secret"));
    }

    #[cfg(windows)]
    #[test]
    fn cmd_starts_with_utf8_setup_args() {
        // Both cmd and PowerShell configure UTF-8 code page on startup.
        let cmd = windows_utf8_shell_args(r"C:\Windows\System32\cmd.exe");
        assert_eq!(
            cmd,
            vec!["/K", "chcp 65001 >nul"],
            "cmd.exe must configure UTF-8 code page"
        );

        let ps = windows_utf8_shell_args(r"C:\...\WindowsPowerShell\v1.0\powershell.exe");
        assert!(
            ps.contains(&"-NoExit"),
            "must keep the PowerShell session interactive"
        );
        assert!(ps.iter().any(|arg| arg.contains("65001")));
        // Nothing may end with a carriage return: these are argv entries, not
        // keystrokes typed into a running shell.
        for arg in cmd.iter().chain(ps.iter()) {
            assert!(
                !arg.contains('\r'),
                "argv entry must not carry an Enter: {arg}"
            );
        }
    }

    #[test]
    fn write_request_defaults_to_enter_and_accepts_camel_case() {
        let defaulted: WriteRequest = serde_json::from_value(json!({"text": "run"})).unwrap();
        assert!(defaulted.press_enter);
        let raw: WriteRequest =
            serde_json::from_value(json!({"text": "run", "pressEnter": false})).unwrap();
        assert!(!raw.press_enter);
    }

    fn output(id: &str, data: &str) -> TerminalEvent {
        TerminalEvent::Output {
            id: id.to_owned(),
            data: data.to_owned(),
        }
    }

    #[test]
    fn unicode_scrollback_overflow_does_not_kill_the_reader() {
        let manager = super::TerminalManager::new("test".to_owned());
        let state = Arc::new(std::sync::Mutex::new(super::TerminalState {
            output: "я".repeat(super::MAX_SCROLLBACK / 2),
            alive: true,
            ..Default::default()
        }));
        // Enough to cross the slack so the compaction actually runs: that is
        // the path where the cut lands inside a two-byte char and used to be
        // able to kill the reader.
        let overflow = vec![b'x'; super::SCROLLBACK_SLACK + 1];
        super::read_output(
            Box::new(std::io::Cursor::new(overflow)),
            Arc::clone(&state),
            Arc::clone(&manager.inner),
            "test".to_owned(),
            None,
            Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        );
        let state = state.lock().unwrap();
        assert!(
            state.output.len() < super::MAX_SCROLLBACK + super::SCROLLBACK_SLACK,
            "the buffer must actually have been compacted"
        );
        assert!(!state.alive);
    }

    #[test]
    fn streaming_keeps_every_output_byte_without_a_second_history() {
        let manager = super::TerminalManager::streaming("test".to_owned());
        let state = Arc::new(std::sync::Mutex::new(super::TerminalState::default()));
        let expected = "hello\r\n".repeat(200_000);
        super::read_output(
            Box::new(std::io::Cursor::new(expected.clone())),
            Arc::clone(&state),
            Arc::clone(&manager.inner),
            "test".to_owned(),
            None,
            Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        );
        assert_eq!(state.lock().unwrap().output.capacity(), 0);
        let events = manager.drain_events();
        let actual: String = events
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::Output { data, .. } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(actual, expected);
        assert!(matches!(events.last(), Some(TerminalEvent::Exited { .. })));
    }

    #[test]
    fn osc52_writes_become_clipboard_events_and_osc7_updates_cwd() {
        let manager = super::TerminalManager::new("test".to_owned());
        let state = Arc::new(std::sync::Mutex::new(super::TerminalState::default()));
        let stream = b"prompt$ \x1b]52;c;aGVsbG8=\x07\x1b]7;file://host/tmp/dir\x07done";
        super::read_output(
            Box::new(std::io::Cursor::new(stream)),
            Arc::clone(&state),
            Arc::clone(&manager.inner),
            "term-1".to_owned(),
            None,
            Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
        );
        assert_eq!(
            state.lock().unwrap().screen.cwd().as_deref(),
            Some("/tmp/dir")
        );
        let events = manager.drain_events();
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::Clipboard { id, payload }
                if id == "term-1" && payload == "aGVsbG8="
        )));
        // The write rides the same ordered stream as output.
        assert!(matches!(events.last(), Some(TerminalEvent::Exited { .. })));
    }

    #[test]
    fn response_notification_survives_until_the_pump_waits() {
        let manager = super::TerminalManager::streaming("test".to_owned());
        manager.notify_response();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            tx.send(manager.wait_events()).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn utf8_split_across_reads_is_preserved_and_invalid_input_is_bounded() {
        let mut decoder = super::Utf8Stream::default();
        let expected = "Привет 🌍 世界";
        let mut actual = String::new();
        for byte in expected.as_bytes() {
            actual.push_str(&decoder.decode(&[*byte], false));
            assert!(decoder.pending.len() <= 3);
        }
        actual.push_str(&decoder.decode(&[], true));
        assert_eq!(actual, expected);
        assert_eq!(decoder.decode(&[0xff, 0xe2], false), "\u{fffd}");
        assert_eq!(decoder.decode(&[], true), "\u{fffd}");
        assert!(decoder.pending.is_empty());
    }

    #[test]
    fn output_pressure_waits_for_the_consumer_and_preserves_every_byte() {
        let inner = Arc::new(super::Inner {
            retain_scrollback: true,
            terminals: std::sync::Mutex::new(std::collections::HashMap::new()),
            events: std::sync::Mutex::new(EventQueue::default()),
            events_ready: std::sync::Condvar::new(),
            token: "t".to_owned(),
            control_socket: std::sync::Mutex::new(None),
            names: std::sync::Mutex::new(std::collections::HashMap::new()),
            camera_request: std::sync::Mutex::new(None),
            raise_requested: std::sync::Mutex::new(None),
            headless: std::sync::Mutex::new(false),
        });

        let first = "a".repeat(MAX_PENDING_EVENT_BYTES / 2);
        let second = "b".repeat(MAX_PENDING_EVENT_BYTES / 2 + 1);
        super::push_event(&inner, output("term-1", &first));

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let blocked_inner = Arc::clone(&inner);
        let blocked_second = second.clone();
        let writer = std::thread::spawn(move || {
            super::push_event(&blocked_inner, output("term-1", &blocked_second));
            done_tx.send(()).unwrap();
        });
        assert!(
            done_rx.try_recv().is_err(),
            "producer waits while the queue is full"
        );

        let mut queue = inner.events.lock().unwrap();
        let first_events = take_events(&mut queue);
        inner.events_ready.notify_all();
        drop(queue);
        assert_eq!(
            done_rx.recv_timeout(std::time::Duration::from_secs(2)),
            Ok(())
        );
        writer.join().unwrap();

        let mut queue = inner.events.lock().unwrap();
        let second_events = take_events(&mut queue);
        let collected: String = first_events
            .into_iter()
            .chain(second_events)
            .filter_map(|event| match event {
                TerminalEvent::Output { data, .. } => Some(data),
                _ => None,
            })
            .collect();
        assert_eq!(collected, first + &second);
    }

    #[test]
    fn disposing_a_terminal_releases_a_blocked_output_producer() {
        let inner = Arc::new(super::Inner {
            retain_scrollback: true,
            terminals: std::sync::Mutex::new(std::collections::HashMap::new()),
            events: std::sync::Mutex::new(EventQueue::default()),
            events_ready: std::sync::Condvar::new(),
            token: "t".to_owned(),
            control_socket: std::sync::Mutex::new(None),
            names: std::sync::Mutex::new(std::collections::HashMap::new()),
            camera_request: std::sync::Mutex::new(None),
            raise_requested: std::sync::Mutex::new(None),
            headless: std::sync::Mutex::new(false),
        });
        super::push_event(
            &inner,
            output("term-1", &"x".repeat(MAX_PENDING_EVENT_BYTES)),
        );
        let blocked_inner = Arc::clone(&inner);
        let writer = std::thread::spawn(move || {
            super::push_event(&blocked_inner, output("term-1", "after"))
        });
        super::stop_terminal_events(&inner, "term-1");
        writer.join().unwrap();
        assert!(inner.events.lock().unwrap().items.is_empty());
    }

    #[test]
    fn draining_leaves_output_untouched() {
        let mut queue = EventQueue::default();
        queue.items.push_back(output("term-1", "hello"));
        let events = take_events(&mut queue);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], TerminalEvent::Output { data, .. } if data == "hello"));
        assert_eq!(queue.bytes, 0);
    }

    #[test]
    fn an_idle_writer_never_reads_as_stuck() {
        // 0 is the idle marker, not a timestamp from 1970: reading it as one
        // would refuse every keystroke on a perfectly healthy terminal.
        let idle = AtomicU64::new(0);
        assert_eq!(super::write_in_flight_ms(&idle), 0);
    }

    #[test]
    fn message_enter_is_separate_and_acknowledged_after_flush() {
        use std::io::Write;
        use std::sync::{atomic::AtomicUsize, mpsc, Arc, Mutex};
        use std::time::{Duration, Instant};
        type RecordedWrites = Arc<Mutex<Vec<(Vec<u8>, Instant)>>>;
        struct RecordingWriter(RecordedWrites);
        impl Write for RecordingWriter {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().push((data.to_vec(), Instant::now()));
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let writes = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let (control_tx, _) = mpsc::channel();
        let (ack_tx, ack_rx) = mpsc::channel();
        let pending = Arc::new(AtomicUsize::new(13));
        tx.send(super::TerminalInput {
            data: b"hello".to_vec(),
            press_enter: true,
            acknowledgement: Some(ack_tx),
        })
        .unwrap();
        tx.send(super::TerminalInput {
            data: b"x".to_vec(),
            press_enter: false,
            acknowledgement: None,
        })
        .unwrap();
        tx.send(super::TerminalInput {
            data: b"world".to_vec(),
            press_enter: false,
            acknowledgement: None,
        })
        .unwrap();
        tx.send(super::TerminalInput {
            data: b"\r".to_vec(),
            press_enter: false,
            acknowledgement: None,
        })
        .unwrap();
        drop(tx);
        let output = Arc::clone(&writes);
        let queued = Arc::clone(&pending);
        let worker = std::thread::spawn(move || {
            super::run_writer(
                Box::new(RecordingWriter(output)),
                rx,
                control_tx,
                queued,
                Arc::new(AtomicU64::new(0)),
                "test".to_owned(),
            )
        });
        ack_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        assert!(writes
            .lock()
            .unwrap()
            .iter()
            .any(|(bytes, _)| bytes == b"\r"));
        worker.join().unwrap();
        let writes = writes.lock().unwrap();
        assert_eq!(
            writes
                .iter()
                .map(|(bytes, _)| bytes.as_slice())
                .collect::<Vec<_>>(),
            vec![b"hello".as_slice(), b"\r", b"x", b"world", b"\r"]
        );
        assert!(writes[1].1.duration_since(writes[0].1) >= Duration::from_millis(200));
        assert!(writes[4].1.duration_since(writes[3].1) >= Duration::from_millis(200));
        assert_eq!(pending.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn failed_submit_is_not_acknowledged_as_success() {
        struct FailedSubmit;
        impl std::io::Write for FailedSubmit {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                if data == b"\r" {
                    return Err(std::io::Error::other("submit failed"));
                }
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (control_tx, control_rx) = std::sync::mpsc::channel();
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        tx.send(super::TerminalInput {
            data: b"hello".to_vec(),
            press_enter: true,
            acknowledgement: Some(ack_tx),
        })
        .unwrap();
        drop(tx);
        super::run_writer(
            Box::new(FailedSubmit),
            rx,
            control_tx,
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(6)),
            std::sync::Arc::new(AtomicU64::new(0)),
            "test".to_owned(),
        );
        assert!(ack_rx
            .recv()
            .unwrap()
            .unwrap_err()
            .contains("submit failed"));
        assert!(matches!(
            control_rx.recv().unwrap(),
            super::ControlCommand::WriterFailed
        ));
    }

    #[test]
    fn a_write_that_never_returns_crosses_the_stuck_threshold() {
        let started = super::now_millis_u64() - super::STUCK_WRITE_MS - 1;
        let stuck = AtomicU64::new(started);
        assert!(super::write_in_flight_ms(&stuck) >= super::STUCK_WRITE_MS);
    }

    #[test]
    fn a_write_that_just_started_is_not_stuck() {
        let fresh = AtomicU64::new(super::now_millis_u64());
        assert!(super::write_in_flight_ms(&fresh) < super::STUCK_WRITE_MS);
    }

    #[test]
    fn a_ctrl_c_keystroke_is_recognised_as_an_interrupt() {
        assert!(super::is_interrupt(b"\x03"));
        assert!(super::is_interrupt(b"\x03\r"));
    }

    #[test]
    fn bulk_input_carrying_the_byte_is_not_an_interrupt() {
        // Otherwise a pasted file with a stray 0x03 in it would let itself
        // past the queue cap, or worse, kill what the terminal is running.
        let mut pasted = vec![b'a'; 4096];
        pasted[2048] = 0x03;
        assert!(!super::is_interrupt(&pasted));
    }

    #[test]
    fn interrupt_refuses_pid_zero() {
        // Same trap as kill_process_tree: 0 would target the engine itself.
        assert!(!super::interrupt_process_tree(0));
    }

    #[test]
    fn kill_process_tree_refuses_pid_zero() {
        // kill(2) reads 0 as "my own process group", so letting one through
        // would have the engine kill itself and every terminal it owns.
        assert!(!super::kill_process_tree(0));
    }

    #[cfg(windows)]
    #[test]
    fn kill_process_tree_on_missing_pid_reports_failure() {
        // Must never panic and must report that nothing was killed. Windows
        // only: on unix this now sends a real signal, and no pid is
        // guaranteed to be free, so a live process could be caught.
        assert!(!super::kill_process_tree(4_194_303));
    }
}

/// End-to-end checks that the control server really answers over the pipe or
/// socket it claims to — not over a port, and not only in the pure router.
#[cfg(test)]
mod transport_tests {
    use super::{ControlServer, TerminalManager, TOKEN_HEADER};
    use std::io::{Read, Write};

    /// A test path, so a run never touches the real Slate socket.
    fn test_path(name: &str) -> String {
        let unique = std::process::id();
        if cfg!(windows) {
            format!(r"\\.\pipe\slate-test-{name}-{unique}")
        } else {
            std::env::temp_dir()
                .join(format!("slate-test-{name}-{unique}.sock"))
                .to_string_lossy()
                .into_owned()
        }
    }

    /// Minimal HTTP/1.1 over the transport, so the test exercises the real
    /// socket rather than calling the router directly.
    fn request(
        path_to_socket: &str,
        method: &str,
        target: &str,
        token: &str,
        body: Option<&str>,
    ) -> String {
        let mut stream = open(path_to_socket);
        let body = body.unwrap_or("");
        let request = format!(
            "{method} {target} HTTP/1.1\r\nHost: slate\r\n{TOKEN_HEADER}: {token}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("write request");
        stream.flush().expect("flush");
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        String::from_utf8_lossy(&response).into_owned()
    }

    #[cfg(windows)]
    fn open(path: &str) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        // A pipe instance may be momentarily busy between accepts; a short
        // retry is normal client behaviour, not a workaround.
        for _ in 0..40 {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .attributes(0)
                .open(path)
            {
                Ok(file) => return file,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        panic!("could not open {path}");
    }

    #[cfg(unix)]
    fn open(path: &str) -> std::os::unix::net::UnixStream {
        for _ in 0..40 {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(stream) => return stream,
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
            }
        }
        panic!("could not connect to {path}");
    }

    fn body_of(response: &str) -> serde_json::Value {
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or("");
        serde_json::from_str(body.trim()).unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn the_server_answers_over_the_pipe_and_reports_its_path() {
        let path = test_path("health");
        let manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let server = ControlServer::start_at(manager, "tok".to_owned(), path.clone())
            .expect("server starts");

        // The reported address is the socket path, not an http:// URL — there
        // is no port to report.
        assert_eq!(server.socket_path(), path);
        assert!(
            !server.socket_path().starts_with("http"),
            "got {}",
            server.socket_path()
        );

        let response = request(&path, "GET", "/health", "tok", None);
        assert!(response.starts_with("HTTP/1.1 200"), "got {response}");
        let body = body_of(&response);
        assert_eq!(body["app"], serde_json::json!("slate"));
        assert_eq!(body["server"], serde_json::json!("slate-control"));
    }

    /// The token is checked once, before routing. A caller without it gets
    /// nothing, not even a 404 that would confirm which routes exist.
    #[test]
    fn a_request_without_the_token_is_refused() {
        let path = test_path("auth");
        let manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let _server = ControlServer::start_at(manager, "right".to_owned(), path.clone())
            .expect("server starts");

        let refused = request(&path, "GET", "/health", "wrong", None);
        assert!(refused.starts_with("HTTP/1.1 401"), "got {refused}");
        assert_eq!(
            body_of(&refused)["error"],
            serde_json::json!("a valid control token is required")
        );
    }

    /// The whole stack: a real socket, the axum adapter, the ported router and
    /// the orchestration store, answering two requests that depend on each
    /// other.
    #[test]
    fn state_persists_across_requests_on_one_server() {
        let path = test_path("state");
        let manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let _server = ControlServer::start_at(manager, "tok".to_owned(), path.clone())
            .expect("server starts");

        let created = request(
            &path,
            "POST",
            "/orchestration/runs",
            "tok",
            Some(r#"{"objective":"ship it"}"#),
        );
        assert!(created.starts_with("HTTP/1.1 201"), "got {created}");
        let run_id = body_of(&created)["data"]["id"].as_str().unwrap().to_owned();

        let listed = request(&path, "GET", "/orchestration/runs", "tok", None);
        assert!(listed.starts_with("HTTP/1.1 200"), "got {listed}");
        let body = body_of(&listed);
        assert_eq!(body["runs"].as_array().unwrap().len(), 1);
        assert_eq!(body["active"]["id"], serde_json::json!(run_id));
    }

    #[test]
    fn an_unknown_route_is_a_404_rather_than_a_hang() {
        let path = test_path("404");
        let manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let _server = ControlServer::start_at(manager, "tok".to_owned(), path.clone())
            .expect("server starts");

        let response = request(&path, "GET", "/nothing/here", "tok", None);
        assert!(response.starts_with("HTTP/1.1 404"), "got {response}");
        assert_eq!(body_of(&response)["code"], serde_json::json!("not_found"));
    }

    #[test]
    fn terminal_output_route_reports_output_or_not_found() {
        let path = test_path("output");
        let manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let _server = ControlServer::start_at(manager, "tok".to_owned(), path.clone())
            .expect("server starts");

        let response = request(&path, "GET", "/terminal/nonexistent/output", "tok", None);
        assert!(response.starts_with("HTTP/1.1 404"), "got {response}");
    }

    /// A second server on a name already in use must fail loudly on the
    /// caller's thread rather than print into the void from a background one.
    #[test]
    fn a_second_server_on_the_same_path_is_refused() {
        let path = test_path("collision");
        let first_manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let _first = ControlServer::start_at(first_manager, "tok".to_owned(), path.clone())
            .expect("the first server starts");

        let second_manager = TerminalManager::new("token-for-tests-0123456789abcdef".to_owned());
        let second = ControlServer::start_at(second_manager, "tok".to_owned(), path.clone());
        assert!(
            second.is_err(),
            "a second instance must not silently steal the first one's clients"
        );
    }
}
