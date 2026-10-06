//! The infinite canvas — the actual Slate UI, replacing the tab-strip
//! placeholder. Widgets live at world coordinates under a pannable/zoomable
//! camera; everything mutates through the journal so a relaunch restores the
//! layout. Geometry comes from `canvas.rs`, state reduction from
//! `projection.rs`, colours from `theme.rs` — all ports of the Electron
//! renderer's `canvasState.ts`/`canvasLayout.ts`/`tokens.ts`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};

use crate::engine::{TerminalManager, TerminalSnapshot};
use slate_app::canvas::{self, is_visible, zoom_at, Pos2, Vec2};
use slate_app::journal_log::JournalLog;
use slate_app::projection::{self, CanvasState, Widget};
use slate_app::terminal_screen::{KeyInput, KeyMods, MouseButton as TermMouseButton, MouseKind};
use slate_app::theme;

/// Widget sizes the Electron renderer defaulted to (`types.ts` WIDGET_DEFAULTS).
/// The shell's `Ht` spawn-size table — every canvas kind got a default
/// rect here; unknown kinds fell through to the terminal's 680×420.
fn default_size(kind: &str) -> (f64, f64) {
    match kind {
        "timer" => (300.0, 220.0),
        "planner" => (420.0, 520.0),
        "files" => (580.0, 480.0),
        "sys-monitor" => (460.0, 380.0),
        "browser" => (720.0, 480.0),
        "image" => (560.0, 420.0),
        "links" => (420.0, 360.0),
        "music-player" => (460.0, 420.0),
        "orchestration" => (520.0, 560.0),
        "chat" => (560.0, 560.0),
        "notes" => (520.0, 540.0),
        "calendar" => (560.0, 520.0),
        "kanban" => (760.0, 520.0),
        _ => (680.0, 420.0),
    }
}

enum Drag {
    /// Empty-space drag pans the camera by the pointer's screen delta.
    Pan { last: Pos2 },
    /// Header drag moves the widget in world space; `grab` is the offset of
    /// the pointer inside the widget's own origin so the frame does not jump.
    /// `press`/`moved` gate the gesture on the original's 3px threshold —
    /// under it the press is a click and the widget stays put.
    Widget {
        id: String,
        grab: Pos2,
        press: Pos2,
        moved: bool,
    },
    /// Edge/corner drag resizes the widget: `dir` uses n/s/e/w letters like
    /// the Electron shell's RESIZE_HANDLES; `origin` is (x, y, w, h) at
    /// gesture start and `anchor` the world point grabbed.
    Resize {
        id: String,
        dir: &'static str,
        origin: (f64, f64, f64, f64),
        anchor: Pos2,
    },
    /// Draw-mode left-drag on empty canvas: world-space points collect in
    /// `pending_stroke` and commit to `canvas.strokes` on release. `start`
    /// is the screen point pressed at — under `DRAW_CLICK_THRESHOLD` of
    /// travel the gesture is a click and draws nothing.
    Stroke { start: Pos2, moved: bool },
    /// Erase-mode press on empty canvas: every pointer position cuts the
    /// strokes under it (eraseAt in useCanvas.ts — a click erases too, so
    /// the down itself applies). `dirty` means strokes changed and the
    /// array must be committed on release.
    Erase { dirty: bool },
    /// Select-tool drag on empty canvas: a screen-space marquee rect; on
    /// release the topmost widget inside it comes to the front (the
    /// shell's select tool — bringToFront of the top-z hit, not a
    /// multi-select).
    Select {
        start: Pos2,
        rect: Option<(Pos2, Pos2)>,
    },
}

/// Resize cursors per direction, matching the Electron frame's handle map.
fn resize_cursor(dir: &str) -> gpui::CursorStyle {
    use gpui::CursorStyle as C;
    match dir {
        "n" | "s" => C::ResizeUpDown,
        "e" | "w" => C::ResizeLeftRight,
        "ne" | "sw" => C::ResizeUpRightDownLeft,
        _ => C::ResizeUpLeftDownRight,
    }
}

/// DRAW_CLICK_THRESHOLD_PX from canvasMetrics.ts: a draw-mode press that
/// moves less than this many *screen* px is a click, not a stroke.
const DRAW_CLICK_THRESHOLD: f32 = 4.0;

/// types.ts STROKE_COLORS — the toolbar's swatch row, in order. New strokes
/// take `STROKE_COLORS[self.stroke_color]`; the Electron default is [0].
const STROKE_COLORS: [&str; 8] = [
    "#ffffff", "#ff6b6b", "#ffa94d", "#ffd43b", "#69db7c", "#4dabf7", "#b197fc", "#f783ac",
];

/// eraseAt's screen-space radius (`eraseAt(point, radius = 14)`, already
/// under the 28px clamp): the hit distance is `ERASE_RADIUS_PX / zoom` in
/// world units.
const ERASE_RADIUS_PX: f32 = 14.0;

/// The shell's `isTerminal`: a widget with no kind is a terminal, not an
/// "unknown widget" — legacy journal entries rely on it.
fn is_terminal_kind(kind: Option<&str>) -> bool {
    matches!(kind, None | Some("terminal"))
}

/// projection::MAX_POINTS_PER_STROKE — longer strokes are truncated here
/// rather than written and then dropped by the sanitizer on replay.
const MAX_STROKE_POINTS: usize = 10_000;

pub struct CanvasView {
    pub(crate) focus: FocusHandle,
    manager: TerminalManager,
    journal: Option<JournalLog>,
    canvas: CanvasState,
    /// The picked workspace directory this canvas belongs to — the shell's
    /// per-workspace canvas slot. `None` is the `__no-workspace__` slot;
    /// snapshots and the journal tail are keyed on it.
    workspace_dir: Option<String>,
    snapshots: Vec<TerminalSnapshot>,
    /// Widget that owns the keyboard; `None` means the canvas itself does.
    active: Option<String>,
    /// Widget whose *frame* owns the keyboard — the shell's DOM focus on
    /// the frame element after a header or handle press. Arrows nudge it
    /// 1px (⇧16), Alt+arrows resize, Delete closes, Escape hands focus
    /// back to the body. A terminal's body press clears it — the xterm
    /// textarea retakes focus just like in the shell.
    frame_focus: Option<String>,
    /// Terminal awaiting the "Close terminal? The running process will be
    /// terminated." answer — the shell's `confirm()` dialog.
    pending_close: Option<String>,
    drag: Option<Drag>,
    /// (cols, rows) last pushed to each PTY — resized only on change.
    term_cells: HashMap<String, (u16, u16)>,
    /// Camera commits are journal writes; drags and wheel zoom would flood
    /// the file, so they are flushed at most this often and on mouse-up.
    camera_dirty_at: Option<Instant>,
    /// Screen position of the right-click spawn menu, when open.
    menu: Option<Pos2>,
    /// Context-menu keyboard cursor — the shell's arrow/Enter nav. Resets
    /// to 0 each time the menu opens.
    menu_sel: usize,
    /// `d` toggles draw mode: empty-canvas drags draw strokes instead of
    /// panning, and the cursor goes crosshair.
    draw_mode: bool,
    /// `e` toggles erase mode — the shell's `tool === 'erase'`. Mutually
    /// exclusive with draw mode, the way `tool` was one value; empty-canvas
    /// presses and drags cut strokes under the pointer.
    erase_mode: bool,
    /// `s` toggles the select tool — the toolbar's Select button: drags on
    /// empty canvas draw a marquee instead of panning. Mutually exclusive
    /// with draw/erase, the way `tool` was one value.
    select_mode: bool,
    /// World-space snap guides while a widget drag is near an alignment —
    /// the shell's snap-guide-x/y accent lines. `.0` is the vertical line's
    /// world x, `.1` the horizontal line's world y; None between drags.
    snap_guides: Option<(Option<f64>, Option<f64>)>,
    /// Screen-space pointer position while an OS file drag hovers the
    /// canvas — `on_drag_move::<ExternalPaths>` keeps it fresh so `on_drop`
    /// knows where the files landed (the shell's `M(clientX, clientY)`).
    drop_point: Option<Pos2>,
    /// The shell's `notice` state — a transient top-center toast for cap
    /// hits, control-route failures and save errors. Rendered while
    /// `Instant` is unexpired.
    notice: Option<(String, Instant)>,
    /// The shell's history ring: `recordHistory` snapshots the canvas into
    /// `undo` before every mutating commit and clears `redo`; Ctrl+Z pops
    /// back through `canvas.restore` — itself a journal entry, so undo
    /// survives restart like every other event.
    undo_stack: Vec<CanvasState>,
    redo_stack: Vec<CanvasState>,
    /// Set while a `canvas.restore` or an arrange batch commits so the
    /// history push stays a single entry per user action.
    suppress_history: bool,
    /// Index into STROKE_COLORS — the shell's `strokeColor` state. `c` in
    /// draw mode cycles it; new strokes take it.
    stroke_color: usize,
    /// World-space points of the stroke currently being drawn; committed
    /// to `canvas.strokes` on mouse-up.
    pending_stroke: Vec<Pos2>,
    /// Widget id mid-title-rename plus the edit buffer — the shell's
    /// double-click-on-title input. Keystrokes route here while it is set.
    renaming: Option<String>,
    rename_buffer: String,
    /// Terminal id whose agent-launch menu is open — WidgetFrame's pencil
    /// button opened a small picker next to the header.
    agent_menu: Option<String>,
    /// Terminal id mid-selection-drag — set on body mousedown when the
    /// program isn't reporting the mouse (or Shift overrode it), tracked on
    /// the root so the drag keeps working outside the widget like xterm's.
    selection_drag: Option<String>,
    /// (widget id, when) a copy-name click last fired — the button shows
    /// the shell's Check glyph for a moment, reverting on a later tick.
    name_copied_at: Option<(String, Instant)>,
    /// The `/` command bar's text buffer while it is open.
    command_bar: Option<String>,
    /// The shell's suggestion-highlight index — ↑/↓ in command mode and
    /// the listbox's `aria-selected` row. Resets when the query changes.
    command_suggestion: usize,
    /// The bar's mode select — `command` runs widget invocations, `message`
    /// writes the line into the picked terminal (the shell's command-mode
    /// dropdown and its `text + "\r"` write).
    command_message: bool,
    /// The message mode's terminal target — the shell's Terminal <select>,
    /// cycled with ↑/↓ while the bar is open.
    command_target: Option<String>,
    /// Initial commands queued for command-bar terminals, flushed on the
    /// refresh tick once each PTY has had a moment to reach its read loop
    /// — the native queueInitialCommand.
    pending_commands: Vec<PendingCommand>,
    /// Two /proc/stat samples a tick apart make a CPU% for the status bar.
    cpu_last: Option<(u64, u64)>,
    cpu_percent: Option<u32>,
    /// Mtime of the journal last folded into `canvas`, so the tick can spot
    /// a CLI writer's commit and refold — the original's `canvas:changed`
    /// push without a watcher.
    journal_mtime: Option<std::time::SystemTime>,
    /// The terminal last told it had focus (DECSET 1004 `\x1b[I`), so widget
    /// focus changes emit the matching `\x1b[O` / `\x1b[I` pair once each.
    focus_sent: Option<String>,
    /// Last terminal-state flush — the 30s autosave behind "close Slate and
    /// your shells come back": scrollback tail + cwd per terminal, restored
    /// on the next spawn.
    last_state_save: Instant,
    /// The viewport size seen at the last render, so code paths with no
    /// Window (the refresh tick, widget adoption) can still spawn-clamp.
    viewport: Vec2,
    /// The shell's `orcspace-agent-select/launched-agent` localStorage keys.
    agent_picks: slate_app::agent_picks::AgentPicks,
    /// The mount-effect guard (`H.current`): each restored terminal
    /// relaunches its recorded agent once per app run.
    agent_relaunched: HashSet<String>,
    /// Launch-failure watch per terminal — the shell's startupProbe: armed
    /// when an agent command is typed, scanning output for the failure
    /// signatures a dying agent leaves for STARTUP_WATCH_MS.
    startup_watch: HashMap<String, StartupWatch>,
    /// The shell's startupNotice state, per terminal — an error banner
    /// pinned to the widget until dismissed or a new launch clears it.
    startup_notice: HashMap<String, String>,
    /// `native-ui.json` — carries `orcspace-arrange-mode` and the
    /// `orcspace-arrange-free-layout` snapshot Free mode replays.
    prefs: slate_app::ui_preferences::UiPreferences,
    /// `settings.json` — `favoriteWidgets` drives the right-click menu.
    settings: slate_app::settings::Settings,
    /// The TitleBar arrange dropdown's open state.
    arrange_menu: bool,
    /// The settings modal (`orcspace:open-settings` → the gear in the
    /// TitleBar/Toolbar) — `views_settings::settings_modal` renders over it.
    settings_open: bool,
    /// The TitleBar git pill — the last background poll's `git status`
    /// read (branch + dirty count), refreshed every `GIT_POLL_EVERY` like
    /// the shell's 8s focused `refreshGit` timer.
    git_chip: crate::views_titlebar::GitChip,
    /// When the chip is next due — a click sets it to now so a press
    /// repolls on the following tick.
    git_next_poll: Instant,
    /// One in-flight porcelain read at a time — a slow repo must never
    /// stack subprocesses behind the tick.
    git_polling: bool,
    counter: u64,
}

/// A `/term <command>` line waiting to be typed into a fresh PTY.
struct PendingCommand {
    terminal_id: String,
    command: String,
    attempts: u8,
    /// True for agent relaunches — once the line lands the terminal's
    /// output is watched for a startup failure.
    watch: bool,
}

/// The shell's STARTUP_WATCH_MS: a launch failure only counts inside this
/// window — ordinary agent output can quote the same phrases.
const STARTUP_WATCH: Duration = Duration::from_secs(30);
/// The shell's STARTUP_TAIL_LIMIT — enough tail to hold a multi-line abort.
const STARTUP_TAIL_LIMIT: usize = 4_000;

struct StartupWatch {
    command: String,
    until: Instant,
    /// Bytes of `TerminalSnapshot.output` already scanned. Scrollback trims
    /// from the front, so a shrunk buffer rewinds the offset to zero.
    scanned: usize,
    tail: String,
}

/// `plainTerminalText` — escape sequences and stray controls out, runs of
/// whitespace collapsed, so marker phrases survive ConPTY-style repaints.
fn plain_terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // OSC: terminated by BEL or ST.
                Some(']') => {
                    let mut escaped = false;
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (escaped && c == '\\') {
                            break;
                        }
                        escaped = c == '\u{1b}';
                    }
                }
                // CSI: parameters and intermediates end at a final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // Everything else is a two-byte ESC sequence.
                Some(_) => {}
                None => {}
            },
            '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}' => {}
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `executableName` — the first bare token of the launch command.
fn executable_name(command: &str) -> String {
    command
        .trim_start()
        .trim_start_matches(['"', '\''])
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_end_matches(['"', '\''])
        .to_owned()
}

/// `startupFailureMessage` — a one-line explanation for the card, or None
/// when the output says nothing about a failed launch. The console-title
/// "shell is idle again" leg of the original probe is ConPTY-specific;
/// Linux shells set no reliable OSC title, so only the marker scan ports.
fn startup_failure_message(output: &str, command: &str) -> Option<String> {
    let text = plain_terminal_text(output).to_lowercase();
    let name = {
        let name = executable_name(command);
        if name.is_empty() {
            "The command".to_owned()
        } else {
            name
        }
    };
    const MEMORY_MARKERS: [&str; 7] = [
        "memory full",
        "memoryexhaustion",
        "memory is exhausted",
        "out of memory",
        "not enough memory",
        "insufficient memory",
        "cannot allocate memory",
    ];
    // Rust's allocator prints the requested byte count — the original's two
    // regexes, `memory allocation of \d+ bytes failed` and
    // `failed to allocate \d+ bytes`.
    let counted = |prefix: &str, suffix: &str| -> bool {
        text.match_indices(prefix).any(|(at, _)| {
            let rest = &text[at + prefix.len()..];
            let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            digits > 0 && rest[digits..].starts_with(suffix)
        })
    };
    if MEMORY_MARKERS.iter().any(|marker| text.contains(marker))
        || counted("memory allocation of ", " bytes failed")
        || counted("failed to allocate ", " bytes")
    {
        return Some(format!(
            "{name} could not start: the system ran out of memory. Close other sessions or free memory, then launch it again."
        ));
    }
    const MISSING_MARKERS: [&str; 5] = [
        "is not recognized as an internal or external command",
        "is not recognized as the name of a cmdlet",
        "не является внутренней или внешней командой",
        "command not found",
        "commandnotfoundexception",
    ];
    if MISSING_MARKERS.iter().any(|marker| text.contains(marker)) {
        return Some(format!(
            "{name} could not start: the command was not found on PATH."
        ));
    }
    // Checked last: an out-of-memory abort also prints a crash banner, and
    // "ran out of memory" is the more useful of the two.
    const CRASH_MARKERS: [&str; 7] = [
        "bun has crashed",
        "panic(thread",
        "illegal instruction at address",
        "segmentation fault",
        "access violation",
        "fatal error in",
        "core dumped",
    ];
    if CRASH_MARKERS.iter().any(|marker| text.contains(marker)) {
        return Some(format!(
            "{name} crashed on startup — its own runtime aborted, not Slate. Relaunch it; if it keeps crashing, update {name}."
        ));
    }
    None
}

/// lib/commandInput.ts WIDGET_ALIASES: the word after a `/ . @` prefix (or
/// standing alone, since the shell's `any` prefix mode also parsed bare
/// names) resolves to a widget kind.
fn widget_alias(name: &str) -> Option<&'static str> {
    Some(match name {
        "terminal" | "term" | "sh" | "shell" | "cmd" => "terminal",
        "files" | "file" => "files",
        "planner" | "plan" | "tasks" | "todo" => "planner",
        "orchestration" | "orc" | "orch" | "agents" | "workers" => "orchestration",
        "browser" | "web" => "browser",
        _ => return None,
    })
}

/// parseWidgetInvocation with prefix `any`: first word is the alias (an
/// optional `/`, `.` or `@` prefix is stripped), the rest of the line is
/// the widget's initial command. Returns `(kind, initialCommand)`.
fn parse_widget_invocation(input: &str, prefix: &str) -> Option<(&'static str, String)> {
    let value = input.trim();
    if value.is_empty() {
        return None;
    }
    let first = value.split_whitespace().next()?;
    let sigil = first
        .chars()
        .next()
        .filter(|c| matches!(c, '/' | '.' | '@'));
    // `prefix !== 'any'` requires the invocation to open with that exact
    // sigil — `/` typed while commandPrefix is `.` is just a message.
    if prefix != "any"
        && sigil.map(|c| c.to_string()).as_deref() != Some(prefix)
    {
        return None;
    }
    let bare = match sigil {
        Some(_) => &first[1..],
        _ => first,
    };
    let kind = widget_alias(&bare.to_lowercase())?;
    Some((kind, value[first.len()..].trim().to_owned()))
}

/// widgetCatalog.ts — (kind, label, hint). The right-click menu lists the
/// catalog in `favoriteWidgets` order, not this declaration order; kinds
/// the native build can't render are filtered out at list time.
const WIDGET_CATALOG: [(&str, &str, &str); 14] = [
    ("terminal", "Terminal", "Shell in the current workspace"),
    ("files", "Files", "Browse workspace files"),
    ("sys-monitor", "System Monitor", "CPU, RAM and processes"),
    ("timer", "Timer", "Countdown or stopwatch"),
    ("planner", "Planner", "Daily agenda and checklist"),
    (
        "orchestration",
        "Orchestration",
        "The agent fleet: tasks, workers and their questions",
    ),
    ("browser", "Browser", "Embedded web page"),
    (
        "image",
        "Image",
        "Pinned image from the clipboard or a file",
    ),
    ("links", "Links", "Saved links"),
    (
        "music-player",
        "Music Player",
        "Stream YouTube, Yandex Music, Spotify or MP3 links",
    ),
    ("chat", "AI Chat", "Chat with an authenticated model"),
    ("notes", "Notes", "Tagged notes with a color per category"),
    ("calendar", "Calendar", "Month view of your planner tasks"),
    (
        "kanban",
        "Kanban",
        "Todo / Doing / Done board for your planner tasks",
    ),
];

/// Kinds `widget_body` can actually render — `music-player` and `chat`
/// stay in the catalog (favorites may name them) but never spawn.
fn renderable(kind: &str) -> bool {
    !matches!(kind, "music-player" | "chat")
}

/// The Toolbar's `W` — catalog rows filtered by the buffer's first token.
/// Only a lone first token suggests (the original required
/// `!N.trim().includes(" ")`); a `/ . @` sigil is stripped for the match,
/// and a non-`any` `commandPrefix` gates to that sigil.
fn toolbar_suggestions(buffer: &str, command_prefix: &str) -> Vec<(String, String, String)> {
    let trimmed = buffer.trim();
    if trimmed.contains(' ') {
        return Vec::new();
    }
    let first = trimmed.split_whitespace().next().unwrap_or("");
    if first.is_empty() {
        return Vec::new();
    }
    let sigil = first.chars().next().filter(|c| "/.@".contains(*c));
    if command_prefix != "any" && sigil.map(|c| c.to_string()).as_deref() != Some(command_prefix) {
        return Vec::new();
    }
    let query = match sigil {
        Some(_) => &first[1..],
        None => first,
    };
    let squashed = query.replace('-', "");
    WIDGET_CATALOG
        .iter()
        .filter(|(kind, _, _)| renderable(kind))
        .filter(|(kind, _, _)| {
            kind.starts_with(query) || kind.replace('-', "").starts_with(squashed.as_str())
        })
        .map(|&(kind, label, hint)| (kind.to_owned(), label.to_owned(), hint.to_owned()))
        .collect()
}

/// WidgetFrame.tsx AGENTS: (label, command) — the launch picker types the
/// command into the terminal and presses Enter. Icons don't port; the name
/// column carries the picker. The first column is the shell's agent id —
/// persisted as `orcspace-launched-agent:<id>` so the pick re-launches.
const AGENT_LAUNCHERS: [(&str, &str, &str); 7] = [
    ("antigravity", "Antigravity", "agy"),
    ("claude", "Claude", "claude"),
    ("codex", "Codex", "codex"),
    ("opencode", "OpenCode", "opencode"),
    ("grok", "Grok Build", "grok"),
    ("kimi", "Kimi Code", "kimi"),
    ("cursor", "Cursor Agent", "cursor-agent"),
];

const CAMERA_COMMIT_EVERY: Duration = Duration::from_millis(500);

fn journal_path() -> std::path::PathBuf {
    slate_app::ipc::user_data_dir().join("command-journal.ndjson")
}

fn load_orchestration_store() -> slate_app::orchestration::OrchestrationStore {
    let path = slate_app::ipc::user_data_dir().join("orchestration.json");
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) if value.is_object() => {
                slate_app::orchestration::OrchestrationStore::load(&value)
            }
            _ => slate_app::orchestration::OrchestrationStore::new(),
        },
        Err(_) => slate_app::orchestration::OrchestrationStore::new(),
    }
}

impl Focusable for CanvasView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn pos(p: Point<Pixels>) -> Pos2 {
    Pos2::new(f32::from(p.x), f32::from(p.y))
}

impl CanvasView {
    pub fn new(manager: TerminalManager, cx: &mut Context<Self>) -> Self {
        // Canvas state is event-sourced: replay the journal, then bring the
        // PTYs the saved terminal widgets point at back to life.
        let journal = JournalLog::open(journal_path())
            .map_err(|error| eprintln!("canvas journal: {error}"))
            .ok();
        // The workspace dir seeds both the snapshot slot and the journal
        // tail filter, so resolve it before the fold.
        let workspace_dir = slate_app::workspace::current();
        let canvas = journal
            .as_ref()
            .map(|log| slate_app::canvas_store::load_with_tail(workspace_dir.as_deref(), log).0)
            .unwrap_or_default();

        let agent_picks = slate_app::agent_picks::AgentPicks::load();
        let settings = slate_app::settings::Settings::load();
        // `targetTerminalId` — the command bar's remembered target.
        let command_target = settings
            .extra("targetTerminalId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut spawned = 0usize;
        let mut relaunch: Vec<PendingCommand> = Vec::new();
        for widget in canvas.widgets.values() {
            if is_terminal_kind(widget.kind.as_deref()) {
                let (cols, rows) = terminal_cells(widget.w, widget.h, canvas.camera.zoom);
                if manager
                    .spawn_with_options(
                        widget.id.clone(),
                        cols,
                        rows,
                        None,
                        slate_app::platform::shell_command(),
                        HashMap::new(),
                    )
                    .is_ok()
                {
                    spawned += 1;
                    if let Some(agent_id) = agent_picks.launched.get(&widget.id) {
                        if let Some((_, _, command)) = AGENT_LAUNCHERS
                            .iter()
                            .find(|(id, _, _)| *id == agent_id.as_str())
                        {
                            relaunch.push(PendingCommand {
                                terminal_id: widget.id.clone(),
                                command: (*command).to_owned(),
                                attempts: 0,
                                watch: true,
                            });
                        }
                    }
                }
            }
        }

        let mut view = Self {
            focus: cx.focus_handle(),
            manager,
            journal,
            canvas,
            workspace_dir,
            snapshots: Vec::new(),
            active: None,
            frame_focus: None,
            pending_close: None,
            drag: None,
            selection_drag: None,
            term_cells: HashMap::new(),
            camera_dirty_at: None,
            menu: None,
            menu_sel: 0,
            draw_mode: false,
            erase_mode: false,
            select_mode: false,
            snap_guides: None,
            drop_point: None,
            notice: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            suppress_history: false,
            stroke_color: 0,
            pending_stroke: Vec::new(),
            renaming: None,
            rename_buffer: String::new(),
            agent_menu: None,
            name_copied_at: None,
            command_bar: None,
            command_suggestion: 0,
            command_message: false,
            command_target,
            pending_commands: Vec::new(),
            journal_mtime: None,
            focus_sent: None,
            last_state_save: Instant::now(),
            cpu_last: None,
            cpu_percent: None,
            viewport: Vec2::new(0.0, 0.0),
            agent_picks,
            agent_relaunched: HashSet::new(),
            startup_watch: HashMap::new(),
            startup_notice: HashMap::new(),
            prefs: slate_app::ui_preferences::UiPreferences::load_or_default(),
            settings,
            arrange_menu: false,
            settings_open: false,
            // The shell's mount-time `refreshGit()` — due immediately.
            git_chip: Default::default(),
            git_next_poll: Instant::now(),
            git_polling: false,
            counter: 0,
        };
        view.snapshots = view.manager.snapshots();
        for pending in relaunch {
            view.agent_relaunched.insert(pending.terminal_id.clone());
            view.pending_commands.push(pending);
        }
        if spawned == 0 && view.canvas.widgets.is_empty() {
            // First run: the Electron app opened empty with a hint overlay;
            // a live terminal is a better first screen and still one key (T)
            // away from more.
            let id = view.fresh_id("terminal");
            view.create_widget_at(&id, "terminal", "Terminal", Pos2::new(80.0, 80.0));
            let _ = view.manager.spawn(id.clone());
            view.active = Some(id);
        }

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let alive = this
                    .update(cx, |view, cx| {
                        view.snapshots = view.manager.snapshots();
                        for event in view.manager.drain_events() {
                            // The shell closed the widget when its process
                            // exited (`onProcessExit ?? onClose`) after writing
                            // the dim marker — same here.
                            if let crate::engine::TerminalEvent::Exited { ref id, code } = event {
                                view.manager.mark_exited(id, code);
                                view.close_widget(id, cx);
                                continue;
                            }
                            // OSC 52: an app inside a terminal wrote to the
                            // clipboard — hand it to the real X11 selection.
                            if let crate::engine::TerminalEvent::Clipboard { payload, .. } = event {
                                use base64::Engine as _;
                                // `?` asks the terminal to *report* the
                                // clipboard — don't clobber the selection.
                                if payload != "?" {
                                    if let Ok(bytes) =
                                        base64::engine::general_purpose::STANDARD.decode(&payload)
                                    {
                                        if let Ok(text) = String::from_utf8(bytes) {
                                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                                text,
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        // `slate canvas focus` posted through the socket lands
                        // here: jump to the widget or the given camera state.
                        if let Some(request) = view.manager.take_camera_request() {
                            if let Some(zoom) = request.zoom {
                                view.canvas.camera.zoom =
                                    zoom.clamp(canvas::MIN_ZOOM as f64, canvas::MAX_ZOOM as f64);
                            }
                            let zoom = view.canvas.camera.zoom;
                            if let Some(id) = request.widget_id.as_deref() {
                                if let Some(widget) = view.canvas.widgets.get(id) {
                                    let cx_w = widget.x + widget.w / 2.0;
                                    let cy_w = widget.y + widget.h / 2.0;
                                    view.canvas.camera.x =
                                        view.viewport.x as f64 / 2.0 - cx_w * zoom;
                                    view.canvas.camera.y =
                                        view.viewport.y as f64 / 2.0 - cy_w * zoom;
                                    view.active = Some(widget.id.clone());
                                }
                            }
                            if let Some(x) = request.x {
                                view.canvas.camera.x = x;
                            }
                            if let Some(y) = request.y {
                                view.canvas.camera.y = y;
                            }
                            view.commit_camera();
                        }
                        // Second-instance raise: a `slate` invocation asked
                        // the running window to come forward. Same mailbox
                        // shape as take_camera_request — a bool slot.
                        if view.manager.take_raise_request() {
                            cx.activate(true);
                            for handle in cx.windows() {
                                let _ = handle.update(cx, |_, window, _| {
                                    window.activate_window();
                                });
                            }
                        }
                        // DECSET 1004: tell a terminal when widget focus
                        // actually moved, not per click — the original's
                        // xterm got focusin/focusout from the DOM.
                        let focus_now = view
                            .active
                            .clone()
                            .filter(|id| is_terminal_kind(view.widget_kind(id).as_deref()));
                        if focus_now != view.focus_sent {
                            if let Some(old) = view.focus_sent.take() {
                                let _ = view.manager.focus_input(&old, false);
                            }
                            if let Some(new) = &focus_now {
                                let _ = view.manager.focus_input(new, true);
                            }
                            view.focus_sent = focus_now;
                        }
                        // The CLI is a second journal writer (`canvas move`,
                        // `plan create`): when the file changes under us,
                        // refold so the canvas matches. Skipped mid-drag — the
                        // uncommitted pointer position would get stomped.
                        if view.drag.is_none() {
                            let mtime = std::fs::metadata(journal_path())
                                .and_then(|m| m.modified())
                                .ok();
                            if mtime.is_some() && mtime != view.journal_mtime {
                                let first = view.journal_mtime.is_none();
                                view.journal_mtime = mtime;
                                if !first {
                                    if let Ok(log) = JournalLog::open(journal_path()) {
                                        let prior: HashSet<String> =
                                            view.canvas.widgets.keys().cloned().collect();
                                        view.canvas = slate_app::canvas_store::load_with_tail(
                                            view.workspace_dir.as_deref(),
                                            &log,
                                        )
                                        .0;
                                        view.journal = Some(log);
                                        // Widget-mount spawn: a terminal
                                        // widget that just appeared via the
                                        // journal gets a PTY (the shell's
                                        // xterm mount did terminal.create).
                                        // Only NEW ids — an existing widget
                                        // losing its snapshot is a dispose,
                                        // and the stale check below closes it.
                                        let newcomers: Vec<(String, f64, f64)> = view
                                            .canvas
                                            .widgets
                                            .values()
                                            .filter(|w| {
                                                !prior.contains(&w.id)
                                                    && is_terminal_kind(w.kind.as_deref())
                                                    && view.manager.snapshot(&w.id).is_err()
                                            })
                                            .map(|w| (w.id.clone(), w.w, w.h))
                                            .collect();
                                        for (id, w, h) in newcomers {
                                            let (cols, rows) =
                                                terminal_cells(w, h, view.canvas.camera.zoom);
                                            if view
                                                .manager
                                                .spawn_with_options(
                                                    id.clone(),
                                                    cols,
                                                    rows,
                                                    None,
                                                    slate_app::platform::shell_command(),
                                                    HashMap::new(),
                                                )
                                                .is_ok()
                                            {
                                                view.relaunch_agent(&id);
                                            }
                                        }
                                        // A focused widget can vanish in the
                                        // refold (`slate canvas remove`) — a
                                        // stale id would keep swallowing keys
                                        // into a dead terminal.
                                        if let Some(active) = &view.active {
                                            if !view.canvas.widgets.contains_key(active) {
                                                view.active = None;
                                            }
                                        }
                                        if let Some(renaming) = &view.renaming {
                                            if !view.canvas.widgets.contains_key(renaming) {
                                                view.renaming = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Autosave: a crash loses at most ~30s of scrollback;
                        // a clean quit also flushes once via on_window_closed.
                        if view.last_state_save.elapsed() >= Duration::from_secs(30) {
                            view.last_state_save = Instant::now();
                            view.manager.save_terminal_states();
                            // Same cadence for the canvas store — it only
                            // writes once 50 journal events have landed since
                            // the last snapshot.
                            slate_app::canvas_store::maybe_snapshot(
                                view.workspace_dir.as_deref(),
                                &view.canvas,
                                view.journal.as_ref().map(|j| j.sequence()).unwrap_or(0),
                            );
                        }
                        // Terminals spawned over the socket (`slate terminal
                        // new`, a worker dispatch) have no widget yet — adopt
                        // each one into the canvas so nothing runs invisibly.
                        let orphans: Vec<String> = view
                            .snapshots
                            .iter()
                            .map(|s| s.id.clone())
                            .filter(|id| !view.canvas.widgets.contains_key(id))
                            .collect();
                        for id in orphans {
                            let n = view.canvas.widgets.len() as f32;
                            let title = view
                                .manager
                                .name(&id)
                                .unwrap_or_else(|| "Terminal".to_owned());
                            view.create_widget_at(
                                &id,
                                "terminal",
                                &title,
                                Pos2::new(80.0 + n * 40.0, 80.0 + n * 40.0),
                            );
                        }
                        // A terminal disposed over the socket leaves the
                        // snapshot list entirely; its widget goes with it.
                        let live: std::collections::HashSet<&str> =
                            view.snapshots.iter().map(|s| s.id.as_str()).collect();
                        let stale: Vec<String> = view
                            .canvas
                            .widgets
                            .values()
                            .filter(|w| {
                                is_terminal_kind(w.kind.as_deref()) && !live.contains(w.id.as_str())
                            })
                            .map(|w| w.id.clone())
                            .collect();
                        for id in stale {
                            view.close_widget(&id, cx);
                        }
                        // Renames land on the widget title and the journal —
                        // the names map is in-memory, so the title is the
                        // part that survives a restart.
                        let renames: Vec<(String, String)> = view
                            .canvas
                            .widgets
                            .values()
                            .filter(|w| is_terminal_kind(w.kind.as_deref()))
                            .filter_map(|w| {
                                view.manager
                                    .name(&w.id)
                                    .filter(|name| *name != w.title)
                                    .map(|name| (w.id.clone(), name))
                            })
                            .collect();
                        for (id, name) in renames {
                            if let Some(widget) = view.canvas.widgets.get_mut(&id) {
                                widget.title = name;
                                let widget = widget.clone();
                                view.commit_widget(&widget);
                            }
                        }
                        // Command-bar initial commands, one tick after spawn:
                        // the shell has reached its read loop by now, so the
                        // line is typed rather than eaten by init. A dead
                        // terminal drops its command; anything else gets a
                        // couple of seconds of retries, like the original's
                        // delivery-is-what-clears queue.
                        let mut deferred: Vec<PendingCommand> = Vec::new();
                        for pending in std::mem::take(&mut view.pending_commands) {
                            match view.manager.write_text(
                                &pending.terminal_id,
                                &pending.command,
                                true,
                            ) {
                                Ok(_) => {
                                    if pending.watch {
                                        view.arm_startup_watch(
                                            &pending.terminal_id,
                                            &pending.command,
                                        );
                                    }
                                }
                                Err(error) => {
                                    if pending.attempts < 12
                                        && view.manager.snapshot(&pending.terminal_id).is_ok()
                                    {
                                        deferred.push(PendingCommand {
                                            attempts: pending.attempts + 1,
                                            ..pending
                                        });
                                    } else {
                                        eprintln!(
                                            "initial command for {} dropped: {error}",
                                            pending.terminal_id
                                        );
                                    }
                                }
                            }
                        }
                        view.pending_commands = deferred;
                        view.poll_startup_watches();
                        // Status bar CPU%: two /proc/stat samples a tick
                        // apart — cheap enough to keep polling.
                        if let Some((idle, total)) = proc_stat_sample() {
                            if let Some((prev_idle, prev_total)) = view.cpu_last {
                                let di = idle.saturating_sub(prev_idle) as f64;
                                let dt = total.saturating_sub(prev_total) as f64;
                                if dt > 0.0 {
                                    view.cpu_percent =
                                        Some(((1.0 - di / dt) * 100.0).round().clamp(0.0, 100.0)
                                            as u32);
                                }
                            }
                            view.cpu_last = Some((idle, total));
                        }
                        // The original repaired out-of-range widgets on every
                        // widgets change (App.tsx): non-maximizable kinds
                        // cannot stay maximized, and a persisted size outside
                        // the kind's limits is clamped. Converges after one
                        // pass, so an in-range canvas writes nothing.
                        let mut repairs: Vec<Widget> = Vec::new();
                        for widget in view.canvas.widgets.values() {
                            let (w, h) = canvas::clamp_widget_size(
                                widget.kind.as_deref(),
                                widget.w,
                                widget.h,
                            );
                            let unmaximize =
                                canvas::non_maximizable(widget.kind.as_deref()) && widget.maximized;
                            if w != widget.w || h != widget.h || unmaximize {
                                let mut fixed = widget.clone();
                                fixed.w = w;
                                fixed.h = h;
                                fixed.maximized = fixed.maximized && !unmaximize;
                                repairs.push(fixed);
                            }
                        }
                        for fixed in repairs {
                            view.canvas.widgets.insert(fixed.id.clone(), fixed.clone());
                            view.commit_widget(&fixed);
                        }
                        // A camera gesture that stopped moving still needs its
                        // resting position written through to the journal.
                        if let Some(at) = view.camera_dirty_at {
                            if at.elapsed() >= CAMERA_COMMIT_EVERY {
                                view.camera_dirty_at = None;
                                view.commit_camera();
                            }
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
                // The TitleBar git chip — the shell polled `git.status()`
                // every 8s focused / 30s hidden; here it's one lazy read
                // per `GIT_POLL_EVERY`, run on the background executor so
                // a big repo's porcelain walk never stalls a frame.
                let poll = this.update(cx, |view, _cx| {
                    if view.git_polling || Instant::now() < view.git_next_poll {
                        None
                    } else {
                        view.git_next_poll =
                            Instant::now() + crate::views_titlebar::GIT_POLL_EVERY;
                        match view.workspace_dir.clone() {
                            Some(dir) => {
                                view.git_polling = true;
                                Some(dir)
                            }
                            None => None,
                        }
                    }
                });
                let Ok(poll) = poll else { break };
                if let Some(dir) = poll {
                    let chip = cx
                        .background_executor()
                        .spawn(async move {
                            crate::views_titlebar::read_git_chip(std::path::Path::new(&dir))
                        })
                        .await;
                    let alive = this
                        .update(cx, |view, cx| {
                            view.git_chip = chip;
                            view.git_polling = false;
                            cx.notify();
                        })
                        .is_ok();
                    if !alive {
                        break;
                    }
                }
            }
        })
        .detach();
        view
    }

    fn fresh_id(&mut self, kind: &str) -> String {
        loop {
            self.counter += 1;
            let id = format!("{kind}-{}", self.counter);
            if !self.canvas.widgets.contains_key(&id) && self.manager.snapshot(&id).is_err() {
                return id;
            }
        }
    }

    /// recordHistory — snapshot the canvas before a mutating commit. The
    /// stacks are in-memory like the original's; a journal refold rebuilds
    /// the canvas but keeps the rings, so undo crosses file reloads too.
    fn record_history(&mut self) {
        const HISTORY_LIMIT: usize = 100;
        self.undo_stack.push(self.canvas.clone());
        if self.undo_stack.len() > HISTORY_LIMIT {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    fn commit(&mut self, entry_type: &str, target: &str, payload: Value) {
        self.commit_as("user", entry_type, target, payload);
    }

    /// `updateWidgets(updates, actor)` — most commits are the `"user"`
    /// actor; the arrange pass journals `"arrange"` like the renderer did.
    fn commit_as(&mut self, actor: &str, entry_type: &str, target: &str, payload: Value) {
        if !self.suppress_history
            && (entry_type.starts_with("widget.") || entry_type.starts_with("canvas."))
        {
            self.record_history();
        }
        if let Some(journal) = &mut self.journal {
            if let Err(error) = journal.commit(actor, entry_type, target, payload) {
                self.notice(format!("journal {entry_type}: {error}"));
            }
        }
    }

    /// The shell's mount effect: a terminal with a recorded
    /// `orcspace-launched-agent` key re-types that agent's command once per
    /// app run, through the same deferred-write queue the command bar uses.
    fn relaunch_agent(&mut self, id: &str) {
        if self.agent_relaunched.contains(id) {
            return;
        }
        let Some(agent_id) = self.agent_picks.launched.get(id).cloned() else {
            return;
        };
        if let Some((_, _, command)) = AGENT_LAUNCHERS
            .iter()
            .find(|(aid, _, _)| *aid == agent_id.as_str())
        {
            self.agent_relaunched.insert(id.to_owned());
            self.pending_commands.push(PendingCommand {
                terminal_id: id.to_owned(),
                command: (*command).to_owned(),
                attempts: 0,
                watch: true,
            });
        }
    }

    /// Arming the launch-failure watch — the shell's `createStartupProbe`
    /// call in `launchAgent`. A new launch also clears a stale banner.
    fn arm_startup_watch(&mut self, id: &str, command: &str) {
        self.startup_notice.remove(id);
        self.startup_watch.insert(
            id.to_owned(),
            StartupWatch {
                command: command.to_owned(),
                until: Instant::now() + STARTUP_WATCH,
                scanned: 0,
                tail: String::new(),
            },
        );
    }

    /// One pass over the armed watches, fed from the snapshots the tick
    /// already refreshed — `watchStartupOutput` in the shell. A failure
    /// verdict pins the banner; expiry or a dead PTY just drops the watch.
    fn poll_startup_watches(&mut self) {
        if self.startup_watch.is_empty() {
            return;
        }
        let now = Instant::now();
        self.startup_watch.retain(|_, watch| watch.until > now);
        let ids: Vec<String> = self.startup_watch.keys().cloned().collect();
        for id in ids {
            let scanned = self
                .startup_watch
                .get(&id)
                .map(|watch| watch.scanned)
                .unwrap_or(0);
            let Ok(snapshot) = self.manager.snapshot(&id) else {
                self.startup_watch.remove(&id);
                continue;
            };
            let output = snapshot.output;
            let Some(watch) = self.startup_watch.get_mut(&id) else {
                continue;
            };
            let mut start = if output.len() < scanned { 0 } else { scanned };
            while start < output.len() && !output.is_char_boundary(start) {
                start += 1;
            }
            watch.scanned = output.len();
            watch.tail.push_str(&output[start..]);
            // JS `slice(-4000)` counts UTF-16 units; bytes at a char
            // boundary is close enough for a marker scan.
            let over = watch.tail.len().saturating_sub(STARTUP_TAIL_LIMIT);
            if over > 0 {
                let mut cut = over;
                while !watch.tail.is_char_boundary(cut) {
                    cut += 1;
                }
                watch.tail.drain(..cut);
            }
            if let Some(message) = startup_failure_message(&watch.tail, &watch.command) {
                self.startup_notice.insert(id.clone(), message);
                self.startup_watch.remove(&id);
            }
        }
    }

    /// The TitleBar arrange pick — `orcspace:canvas-arrange`. Non-free
    /// modes snapshot every widget once (`orcspace-arrange-free-layout`),
    /// tile them into the visible area and journal each move; Free replays
    /// the snapshot once and clears it.
    fn apply_arrange(&mut self, mode: slate_app::arrange::ArrangeMode, cx: &mut Context<Self>) {
        use slate_app::arrange::ArrangeMode;
        self.prefs.arrange_mode = mode;
        // One arrange is one history entry — the batch suppresses per-widget
        // pushes after this single snapshot.
        self.record_history();
        self.suppress_history = true;
        if mode == ArrangeMode::Free {
            let saved = std::mem::take(&mut self.prefs.arrange_free_layout);
            for (id, rect) in saved {
                let Some(widget) = self.canvas.widgets.get_mut(&id) else {
                    continue;
                };
                widget.x = rect.x;
                widget.y = rect.y;
                widget.w = rect.w;
                widget.h = rect.h;
                widget.maximized = rect.maximized;
                let widget = widget.clone();
                self.commit_widget_as("arrange", &widget);
            }
        } else {
            if self.prefs.arrange_free_layout.is_empty() {
                self.prefs.arrange_free_layout = self
                    .canvas
                    .widgets
                    .values()
                    .map(|w| {
                        (
                            w.id.clone(),
                            slate_app::ui_preferences::FreeRect {
                                x: w.x,
                                y: w.y,
                                w: w.w,
                                h: w.h,
                                maximized: w.maximized,
                            },
                        )
                    })
                    .collect();
            }
            for (id, x, y, w, h) in slate_app::arrange::layout(
                mode,
                &self.canvas,
                self.viewport,
                self.active.as_deref(),
            ) {
                let Some(widget) = self.canvas.widgets.get_mut(&id) else {
                    continue;
                };
                widget.x = x;
                widget.y = y;
                widget.w = w;
                widget.h = h;
                widget.maximized = false;
                let widget = widget.clone();
                self.commit_widget_as("arrange", &widget);
            }
        }
        self.suppress_history = false;
        if let Err(error) = self
            .prefs
            .save(&slate_app::ui_preferences::UiPreferences::path())
        {
            self.notice(format!("arrange: {error}"));
        }
        cx.notify();
    }

    /// Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y — the shell's `orcspace:canvas-undo`
    /// and `-redo` events. The popped state journals as `canvas.restore` so
    /// the fold replays the undo exactly like it replays everything else.
    fn undo(&mut self, redo: bool) {
        let (from, to) = if redo {
            (&mut self.redo_stack, &mut self.undo_stack)
        } else {
            (&mut self.undo_stack, &mut self.redo_stack)
        };
        let Some(restored) = from.pop() else { return };
        to.push(self.canvas.clone());
        let mut payload = slate_app::canvas_store::canvas_to_value(&restored, 0);
        // canvas.restore ignores snapshotSeq — it serializes with the
        // snapshot so the same shape serves both writers.
        payload.as_object_mut().map(|map| map.remove("snapshotSeq"));
        self.suppress_history = true;
        self.commit("canvas.restore", "canvas", payload);
        self.suppress_history = false;
        self.canvas = restored;
    }

    /// Buttons in the TitleBar (`views_titlebar::titlebar_pane`) — window
    /// controls ride gpui's Window; the rest flip view state.
    pub(crate) fn titlebar_command(
        &mut self,
        action: &str,
        payload: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            "settings" => self.settings_open = !self.settings_open,
            "sidebar" => {
                self.prefs.sidebar_open = !self.prefs.sidebar_open;
                let _ = self
                    .prefs
                    .save(&slate_app::ui_preferences::UiPreferences::path());
            }
            // `orcspace:canvas-undo`/`-redo` — the history pill's arrows
            // pop the same rings Ctrl+Z/Ctrl+Shift+Z do.
            "undo" => self.undo(false),
            "redo" => self.undo(true),
            // The WorkView tablist: Canvas is the only view this build
            // renders, so the other tabs say so instead of switching.
            "view" => {
                if let Some(view) = payload.get("view").and_then(Value::as_str) {
                    if view != "canvas" {
                        self.notice(format!("{view} view isn't in this build — canvas only"));
                    }
                }
            }
            // `orcspace-flip-terminals` — localStorage in the shell, the
            // persisted `terminalsFlipped` pref here; widget bodies draw
            // the flip card while it's on.
            "flip" => {
                self.prefs.terminals_flipped = !self.prefs.terminals_flipped;
                let _ = self
                    .prefs
                    .save(&slate_app::ui_preferences::UiPreferences::path());
            }
            // The git pill has no checkout panel in this build — a click
            // repolls on the next tick and flashes the summary the
            // original's tooltip carried.
            "git" => {
                self.git_next_poll = Instant::now();
                let chip = &self.git_chip;
                let text = if !chip.repo {
                    "Git — not a repository".to_owned()
                } else if chip.dirty == 0 {
                    format!("{} — working tree clean", chip.branch)
                } else {
                    format!(
                        "{} — {} changed file{}",
                        chip.branch,
                        chip.dirty,
                        if chip.dirty == 1 { "" } else { "s" }
                    )
                };
                self.notice(text);
            }
            "minimize" => window.minimize_window(),
            "maximize" => window.zoom_window(),
            "close" => cx.quit(),
            _ => {}
        }
        cx.notify();
    }

    /// Settings modal rows (`views_settings::settings_modal`) — writes
    /// through to the persisted stores so a restart keeps the pick.
    pub(crate) fn settings_command(
        &mut self,
        action: &str,
        payload: Value,
        cx: &mut Context<Self>,
    ) {
        match action {
            "close" => self.settings_open = false,
            // The settings context's pickBackground/clearBackground — the
            // pick stores a `data:image/…` URL under `extra["background"]`,
            // exactly what the Electron settings.json carried.
            "pick_background" | "background_pick" => {
                let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("Wallpaper image".into()),
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = picked.await {
                        if let Some(path) = paths.first() {
                            let mime = match path
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .unwrap_or("")
                                .to_ascii_lowercase()
                                .as_str()
                            {
                                "jpg" | "jpeg" => "image/jpeg",
                                "gif" => "image/gif",
                                "webp" => "image/webp",
                                "bmp" => "image/bmp",
                                _ => "image/png",
                            };
                            if let Ok(bytes) = std::fs::read(path) {
                                use base64::Engine as _;
                                let url = format!(
                                    "data:{mime};base64,{}",
                                    base64::engine::general_purpose::STANDARD.encode(bytes)
                                );
                                let _ = this.update(cx, |this, cx| {
                                    this.settings.set_extra("background", Value::from(url));
                                    this.settings.save();
                                    cx.notify();
                                });
                            }
                        }
                    }
                })
                .detach();
            }
            "background_clear" => {
                self.settings.set_extra("background", Value::Null);
                self.settings.save();
            }
            "toggle_fav" => {
                if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
                    let favs = &mut self.settings.favorite_widgets;
                    if let Some(at) = favs.iter().position(|k| k == kind) {
                        favs.remove(at);
                    } else {
                        favs.push(kind.to_owned());
                    }
                    self.settings.save();
                }
            }
            "toggle_pref" | "set_pref" => {
                if let Some(key) = payload.get("key").and_then(Value::as_str) {
                    let value = payload.get("value").cloned();
                    let want = value.as_ref().and_then(Value::as_bool).unwrap_or(true);
                    match key {
                        "sidebar_open" => {
                            self.prefs.sidebar_open = match &value {
                                Some(_) => want,
                                None => !self.prefs.sidebar_open,
                            };
                            let _ = self
                                .prefs
                                .save(&slate_app::ui_preferences::UiPreferences::path());
                        }
                        "auto_approve_permissions" => {
                            self.settings.auto_approve_permissions = match &value {
                                Some(_) => want,
                                None => !self.settings.auto_approve_permissions,
                            };
                            self.settings.save();
                        }
                        "auto_names" => {
                            self.prefs.auto_names = match &value {
                                Some(_) => want,
                                None => !self.prefs.auto_names,
                            };
                            let _ = self
                                .prefs
                                .save(&slate_app::ui_preferences::UiPreferences::path());
                        }
                        "arrange_mode" => {
                            if let Some(label) = value.as_ref().and_then(Value::as_str) {
                                if let Some(mode) = slate_app::arrange::ArrangeMode::ALL
                                    .iter()
                                    .find(|m| m.label().eq_ignore_ascii_case(label))
                                {
                                    let mode = *mode;
                                    if mode != self.prefs.arrange_mode {
                                        self.apply_arrange(mode, cx);
                                    }
                                }
                            }
                        }
                        "image_insert_shortcut" => {
                            if let Some(shortcut) = value.as_ref().and_then(Value::as_str) {
                                if self.settings.set_image_insert_shortcut(shortcut) {
                                    self.settings.save();
                                }
                            }
                        }
                        "background_dim" => {
                            if let Some(v) = value.as_ref().and_then(Value::as_u64) {
                                if self.settings.set_background_dim(v.min(100)) {
                                    self.settings.save();
                                }
                            }
                        }
                        "background_blur" => {
                            if let Some(v) = value.as_ref().and_then(Value::as_u64) {
                                if self.settings.set_background_blur(v.min(200)) {
                                    self.settings.save();
                                }
                            }
                        }
                        "background" => {
                            // Set via pick_background; null here = Remove.
                            if value == Some(Value::Null) {
                                self.settings.set_extra("background", Value::Null);
                                self.settings.save();
                            }
                        }
                        _ => {
                            // Unmanaged keys ride `extra` verbatim.
                            if let Some(v) = value {
                                if self.settings.set_extra(key, v) {
                                    self.settings.save();
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Bottom-toolbar buttons (`views_toolbar::toolbar_pane`) — every control
    /// the Electron Toolbar exposed, dispatching to the same code its keys do.
    pub(crate) fn toolbar_command(
        &mut self,
        action: &str,
        _payload: Value,
        cx: &mut Context<Self>,
    ) {
        if let Some(tool) = action.strip_prefix("tool:") {
            // One tool at a time, the shell's single `tool` state; picking
            // the active tool again turns it off.
            let toggling = match tool {
                "draw" => self.draw_mode,
                "erase" => self.erase_mode,
                _ => self.select_mode,
            };
            self.draw_mode = tool == "draw" && !toggling;
            self.erase_mode = tool == "erase" && !toggling;
            self.select_mode = tool == "select" && !toggling;
            cx.notify();
            return;
        }
        if let Some(n) = action
            .strip_prefix("stroke:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            self.stroke_color = n.min(STROKE_COLORS.len() - 1);
            cx.notify();
            return;
        }
        if let Some(id) = action.strip_prefix("target:") {
            self.command_target = Some(id.to_owned());
            // The shell's `updateSettings({ targetTerminalId })` — the pick
            // survives restarts through the unmanaged-key passthrough.
            self.settings.set_extra("targetTerminalId", json!(id));
            self.settings.save();
            cx.notify();
            return;
        }
        if let Some(kind) = action.strip_prefix("suggest:") {
            // The suggestions popover's click = "adopt this kind into the
            // input" — the original filled `/<kind> ` and kept focus.
            self.command_bar = Some(format!("/{kind} "));
            cx.notify();
            return;
        }
        if let Some(mode) = action.strip_prefix("mode:") {
            self.command_message = mode == "message";
            // Mode change resets the suggestion highlight — the shell's
            // `setSuggestionIndex(0)` effect on `[suggestionQuery, mode, …]`.
            self.command_suggestion = 0;
            if self.command_bar.is_none() {
                self.command_bar = Some(String::new());
            }
            cx.notify();
            return;
        }
        let center = Pos2::new(self.viewport.x / 2.0, self.viewport.y / 2.0);
        match action {
            "clear_strokes" => {
                self.canvas.strokes.clear();
                self.commit_strokes();
            }
            "zoom_in" => zoom_at(&mut self.canvas.camera, Pos2::ZERO, center, 1.2),
            "zoom_out" => zoom_at(&mut self.canvas.camera, Pos2::ZERO, center, 1.0 / 1.2),
            "zoom_reset" => self.canvas.camera.zoom = 1.0,
            "fit" => self.fit(self.viewport, cx),
            "undo" => self.undo(false),
            "redo" => self.undo(true),
            "settings" => self.settings_open = !self.settings_open,
            "focus_input" => {
                if self.command_bar.is_none() {
                    self.command_bar = Some(String::new());
                }
            }
            "workspace:pick" => {
                // Async folder pick; the workspace adopts the choice.
                let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Workspace folder".into()),
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = picked.await {
                        if let Some(dir) = paths.first() {
                            let _ = this.update(cx, |this, cx| {
                                let dir = dir.to_string_lossy().into_owned();
                                let mut state = slate_app::workspace::WorkspaceState::load();
                                state.set_workspace_dir(Some(dir));
                                state.save();
                                // The workspace swap reloads the canvas —
                                // its snapshot + journal tail are per-dir.
                                let journal_path =
                                    slate_app::ipc::user_data_dir().join("command-journal.ndjson");
                                if let Ok(log) =
                                    slate_app::journal_log::JournalLog::open(&journal_path)
                                {
                                    this.canvas = slate_app::canvas_store::load_with_tail(
                                        slate_app::workspace::current().as_deref(),
                                        &log,
                                    )
                                    .0;
                                }
                                cx.notify();
                            });
                        }
                    }
                })
                .detach();
            }
            _ => {}
        }
        cx.notify();
    }

    /// Sidebar rows (`views_sidebar::sidebar_pane`) — focus raises a widget
    /// and centers the camera on it; the rest reuse the existing paths.
    pub(crate) fn sidebar_command(&mut self, action: &str, payload: Value, cx: &mut Context<Self>) {
        match action {
            "toggle" => {
                self.prefs.sidebar_open = !self.prefs.sidebar_open;
                let _ = self
                    .prefs
                    .save(&slate_app::ui_preferences::UiPreferences::path());
            }
            "focus" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    self.raise(id);
                    self.active = Some(id.to_owned());
                    if let Some(widget) = self.canvas.widgets.get(id).cloned() {
                        // Camera centers the widget at 1× like a shell click.
                        let zoom = self.canvas.camera.zoom.max(0.2);
                        self.canvas.camera.x =
                            widget.x + widget.w / 2.0 - self.viewport.x as f64 / (2.0 * zoom);
                        self.canvas.camera.y =
                            widget.y + widget.h / 2.0 - self.viewport.y as f64 / (2.0 * zoom);
                    }
                }
            }
            "close" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    // Terminals confirm like the header ×; other kinds close.
                    if is_terminal_kind(self.widget_kind(id).as_deref()) {
                        self.pending_close = Some(id.to_owned());
                    } else {
                        self.close_widget(id, cx);
                    }
                }
            }
            "spawn" => {
                if let Some(kind) = payload.get("kind").and_then(Value::as_str) {
                    let center = Pos2::new(self.viewport.x / 2.0, self.viewport.y / 2.0);
                    self.spawn_menu_widget(kind, center, cx);
                }
            }
            "plan_toggle" => {
                // Forward to the planner pane's op — the widget id is just a
                // dispatch handle; the item id + `done` do the work.
                let planner = self
                    .canvas
                    .widgets
                    .values()
                    .find(|w| w.kind.as_deref() == Some("planner"))
                    .map(|w| w.id.clone());
                if let Some(widget_id) = planner {
                    self.widget_command(
                        &widget_id,
                        json!({ "op": "plan_toggle", "id": payload.get("id"), "done": payload.get("done") }),
                        cx,
                    );
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn commit_widget(&mut self, widget: &Widget) {
        self.commit_widget_as("user", widget);
    }

    /// `updateWidgets(updates, actor)` — the arrange pass journals with the
    /// `"arrange"` actor like the renderer did.
    fn commit_widget_as(&mut self, actor: &str, widget: &Widget) {
        let payload = json!({
            "title": widget.title,
            "x": widget.x, "y": widget.y, "w": widget.w, "h": widget.h,
            "z": widget.z, "maximized": widget.maximized,
        });
        self.commit_as(
            actor,
            "widget.update",
            &format!("widget:{}", widget.id),
            payload,
        );
    }

    /// `canvas.strokes` replaces the whole strokes array — that is how the
    /// projection applies the event — so a clear writes `[]` and an added
    /// stroke writes every stroke including it.
    fn commit_strokes(&mut self) {
        let strokes: Vec<Value> = self
            .canvas
            .strokes
            .iter()
            .map(|stroke| {
                json!({
                    "id": stroke.id,
                    "color": stroke.color,
                    "points": stroke
                        .points
                        .iter()
                        .map(|point| json!({"x": point.x, "y": point.y}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        self.commit("canvas.strokes", "canvas", json!({ "strokes": strokes }));
    }

    /// useCanvas.ts `eraseAt`: a press/drag position in erase mode cuts the
    /// strokes under it. Points within the world radius die; a segment whose
    /// interior crosses the footprint kills both its ends; each surviving run
    /// of ≥2 points keeps the stroke's color under a fresh id — the original
    /// splits strokes rather than deleting them whole. Single-point "dots"
    /// survive unless directly hit; empty strokes drop. Returns whether the
    /// strokes array changed (the caller commits once on release).
    fn erase_at(&mut self, world: Pos2) -> bool {
        let zoom = self.canvas.camera.zoom.max(0.1) as f32;
        let radius = ERASE_RADIUS_PX / zoom;
        let near = |p: &projection::Point| {
            let dx = p.x - world.x as f64;
            let dy = p.y - world.y as f64;
            dx * dx + dy * dy <= (radius as f64) * (radius as f64)
        };
        let crosses = |a: &projection::Point, b: &projection::Point| {
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let len_sq = dx * dx + dy * dy;
            let t = if len_sq == 0.0 {
                0.0
            } else {
                (((world.x as f64 - a.x) * dx + (world.y as f64 - a.y) * dy) / len_sq)
                    .clamp(0.0, 1.0)
            };
            let (px, py) = (
                world.x as f64 - (a.x + t * dx),
                world.y as f64 - (a.y + t * dy),
            );
            px * px + py * py <= (radius as f64) * (radius as f64)
        };

        let mut changed = false;
        let mut next: Vec<projection::Stroke> = Vec::new();
        for stroke in std::mem::take(&mut self.canvas.strokes) {
            let pts = stroke.points;
            // A zero-point stroke carries nothing — the original dropped it
            // on any erase; a one-point "dot" survives unless directly hit.
            if pts.is_empty() {
                changed = true;
                continue;
            }
            if pts.len() == 1 {
                if near(&pts[0]) {
                    changed = true;
                } else {
                    next.push(projection::Stroke {
                        points: pts,
                        ..stroke
                    });
                }
                continue;
            }
            // Bounds reject: strokes whose box misses the footprint are kept
            // untouched, skipping the per-point pass.
            let (mut min_x, mut min_y, mut max_x, mut max_y) =
                (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
            for p in &pts {
                min_x = min_x.min(p.x);
                min_y = min_y.min(p.y);
                max_x = max_x.max(p.x);
                max_y = max_y.max(p.y);
            }
            let r = radius as f64;
            if max_x < world.x as f64 - r
                || min_x > world.x as f64 + r
                || max_y < world.y as f64 - r
                || min_y > world.y as f64 + r
            {
                next.push(projection::Stroke {
                    points: pts,
                    ..stroke
                });
                continue;
            }
            let mut dead = vec![false; pts.len()];
            let mut any_dead = false;
            for i in 0..pts.len() {
                if near(&pts[i]) {
                    dead[i] = true;
                    any_dead = true;
                } else if i > 0 && !dead[i - 1] && crosses(&pts[i - 1], &pts[i]) {
                    dead[i] = true;
                    dead[i - 1] = true;
                    any_dead = true;
                }
            }
            if !any_dead {
                next.push(projection::Stroke {
                    points: pts,
                    ..stroke
                });
                continue;
            }
            changed = true;
            let mut run: Vec<projection::Point> = Vec::new();
            for (i, p) in pts.into_iter().enumerate() {
                if dead[i] {
                    if run.len() > 1 {
                        next.push(projection::Stroke {
                            id: format!("stroke-{}", uuid::Uuid::new_v4().simple()),
                            points: std::mem::take(&mut run),
                            color: stroke.color.clone(),
                        });
                    } else {
                        run.clear();
                    }
                } else {
                    run.push(p);
                }
            }
            if run.len() > 1 {
                next.push(projection::Stroke {
                    id: format!("stroke-{}", uuid::Uuid::new_v4().simple()),
                    points: run,
                    color: stroke.color.clone(),
                });
            }
        }
        self.canvas.strokes = next;
        changed
    }

    /// Double-clicking a widget title opens the shell's rename input. The
    /// buffer seeds from the current title; Enter or a click elsewhere
    /// commits, Escape cancels.
    fn begin_rename(&mut self, id: &str) {
        if let Some(widget) = self.canvas.widgets.get(id) {
            self.rename_buffer = widget.title.clone();
            self.renaming = Some(id.to_owned());
        }
    }

    /// The Electron onRename: `value.trim() || widget.title` — an empty
    /// buffer keeps the old title rather than writing nothing. Terminals
    /// rename through the manager (`slate rename`'s names map) so the CLI
    /// sees the same name; the tick then commits it. Other kinds patch the
    /// journaled widget directly.
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming.take() else {
            return;
        };
        let title = self.rename_buffer.trim().to_owned();
        self.rename_buffer.clear();
        if title.is_empty() {
            cx.notify();
            return;
        }
        let is_terminal = self
            .canvas
            .widgets
            .get(&id)
            .is_some_and(|w| is_terminal_kind(w.kind.as_deref()));
        if is_terminal {
            // The tick's rename sync picks the name up, writes the title and
            // commits — the same path `slate rename` takes.
            self.manager.set_name(&id, &title);
        } else if let Some(widget) = self.canvas.widgets.get_mut(&id) {
            if widget.title != title {
                widget.title = title.clone();
                self.commit(
                    "widget.update",
                    &format!("widget:{id}"),
                    json!({ "title": title }),
                );
            }
        }
        cx.notify();
    }

    fn cancel_rename(&mut self) {
        self.renaming = None;
        self.rename_buffer.clear();
    }

    fn commit_camera(&mut self) {
        let camera = &self.canvas.camera;
        self.commit(
            "canvas.camera",
            "canvas",
            json!({"x": camera.x, "y": camera.y, "zoom": camera.zoom}),
        );
    }

    /// The shell's `defaultSize(kind, vw, vh, zoom)`: the catalog size
    /// shrinks to the visible world (64px/192px chrome reserves), clamped at
    /// the 280×160 minimum — small windows never spawn oversized widgets.
    fn spawn_size(&self, kind: &str) -> (f64, f64) {
        let (dw, dh) = default_size(kind);
        if self.viewport.x <= 0.0 {
            return (dw, dh);
        }
        let zoom = self.canvas.camera.zoom.max(0.01) as f32;
        let c = (self.viewport.x - 64.0) / zoom;
        let i = (self.viewport.y - 192.0) / zoom;
        (
            (dw as f32).min(c.floor()).max(280.0) as f64,
            (dh as f32).min(i.floor()).max(160.0) as f64,
        )
    }

    /// Add a widget at a world position (top-left corner). The point is
    /// clamped into the visible world — the original's clampToVisibleWorld —
    /// so a fresh widget never opens fully off-screen or under the title
    /// band. Once a viewport is known the clamp always applies.
    fn create_widget_at(&mut self, id: &str, kind: &str, title: &str, at: Pos2) {
        if self.canvas.widgets.len() >= projection::MAX_WIDGETS {
            // addWidget returned false → the shell's "Canvas is full" notice.
            self.notice("Canvas is full — close a widget before adding another.");
            return;
        }
        // `addWidget` anchored spawns at `point - 16` in world units.
        let at = Pos2::new(at.x - 16.0, at.y - 16.0);
        let (w, h) = self.spawn_size(kind);
        let at = if self.viewport.x > 0.0 {
            canvas::clamp_to_visible_world(&self.canvas.camera, self.viewport, at, w, h)
        } else {
            at
        };
        let z = self
            .canvas
            .widgets
            .values()
            .map(|widget| widget.z)
            .fold(0.0, f64::max)
            + 1.0;
        let widget = Widget {
            id: id.to_owned(),
            title: title.to_owned(),
            kind: Some(kind.to_owned()),
            x: at.x as f64,
            y: at.y as f64,
            w,
            h,
            z,
            maximized: false,
            image_path: None,
            image_name: None,
            state: None,
            version: 1.0,
            updated_at: 0.0,
        };
        self.commit(
            "widget.create",
            &format!("widget:{id}"),
            json!({
                "id": widget.id, "title": widget.title, "kind": widget.kind,
                "x": widget.x, "y": widget.y, "w": widget.w, "h": widget.h,
                "z": widget.z, "maximized": widget.maximized,
            }),
        );
        self.canvas.widgets.insert(id.to_owned(), widget);
    }

    /// The shell's canvas `onDrop`: files land at the pointer's world point
    /// in a 28px cascade. Images import into the media store and spawn an
    /// `image` widget; other kinds opened a browser widget in the shell —
    /// this build can't render those, so they get an honest notice instead.
    fn handle_file_drop(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let point = self
            .drop_point
            .take()
            .unwrap_or_else(|| Pos2::new(self.viewport.x / 2.0, self.viewport.y / 2.0));
        let view_now = canvas::View::new(&self.canvas.camera, Pos2::ZERO);
        let drop = view_now.to_world(point);
        let media_dir = slate_app::ipc::user_data_dir().join("media");
        let mut spawned = 0usize;
        for source in paths.paths() {
            if self.canvas.widgets.len() >= projection::MAX_WIDGETS {
                self.notice("Canvas is full — close a widget before adding another.");
                break;
            }
            let name = source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_owned());
            match slate_app::attachments::import_image(source, &media_dir) {
                Ok(imported) => {
                    // `addWidget` anchors spawns at `point - 16`.
                    let at = Pos2::new(
                        drop.x + spawned as f32 * 28.0 - 16.0,
                        drop.y + spawned as f32 * 28.0 - 16.0,
                    );
                    let id = self.fresh_id("image");
                    let (w, h) = self.spawn_size("image");
                    let z = self
                        .canvas
                        .widgets
                        .values()
                        .map(|widget| widget.z)
                        .fold(0.0, f64::max)
                        + 1.0;
                    let image_path = imported.to_string_lossy().into_owned();
                    self.commit(
                        "widget.create",
                        &format!("widget:{id}"),
                        json!({
                            "id": id, "title": name, "kind": "image",
                            "x": at.x as f64, "y": at.y as f64,
                            "w": w, "h": h, "z": z, "maximized": false,
                            "imagePath": image_path, "imageName": name,
                        }),
                    );
                    self.canvas.widgets.insert(
                        id.clone(),
                        Widget {
                            id,
                            title: name.clone(),
                            kind: Some("image".to_owned()),
                            x: at.x as f64,
                            y: at.y as f64,
                            w,
                            h,
                            z,
                            maximized: false,
                            image_path: Some(image_path),
                            image_name: Some(name),
                            state: None,
                            version: 1.0,
                            updated_at: 0.0,
                        },
                    );
                    spawned += 1;
                }
                Err(_) => self.notice(format!(
                    "can't open \"{name}\" — only images can be dropped in this build"
                )),
            }
        }
        cx.notify();
    }

    /// The shell's `matchesShortcut(u, combo)` — "Mod+Shift+I" style combos
    /// from `imageInsertShortcut`. `Mod` is the platform modifier (Ctrl on
    /// Linux/Windows, Cmd on macOS); CTRL/ALT/SHIFT/META are literal.
    fn shortcut_matches(keystroke: &Keystroke, combo: &str) -> bool {
        let mut want_mod = false;
        let mut want_ctrl = false;
        let mut want_alt = false;
        let mut want_shift = false;
        let mut want_meta = false;
        let mut key = None;
        for token in combo.split('+') {
            match token.trim().to_ascii_uppercase().as_str() {
                "MOD" => want_mod = true,
                "CTRL" | "CONTROL" => want_ctrl = true,
                "ALT" | "OPTION" => want_alt = true,
                "SHIFT" => want_shift = true,
                "META" | "CMD" | "COMMAND" => want_meta = true,
                other if !other.is_empty() => key = Some(other.to_owned()),
                _ => {}
            }
        }
        let mods = &keystroke.modifiers;
        // Mod folds into the platform modifier — on Linux/Windows that IS
        // Ctrl (same physical key), so Mod and Ctrl in a combo can't be
        // checked independently.
        let (want_control, want_platform) = if cfg!(target_os = "macos") {
            (want_ctrl, want_mod || want_meta)
        } else {
            (want_mod || want_ctrl, want_meta)
        };
        let Some(key) = key else { return false };
        want_control == mods.control
            && want_platform == mods.platform
            && want_alt == mods.alt
            && want_shift == mods.shift
            && keystroke.key.eq_ignore_ascii_case(&key)
    }

    /// The shell's image-widget hotkey handler (`Mod+Shift+I` by default,
    /// `imageInsertShortcut` in settings): the clipboard's image lands as an
    /// `image` widget centred in the viewport — `saveClipboard` then
    /// `addWidget("image", rect, name, {imagePath, imageName})`. The
    /// shortcut fired even over a focused terminal (xterm was excluded from
    /// the input-context guard), so this runs ahead of the terminal branch.
    fn image_insert(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            self.notice("No image found in the clipboard.");
            return;
        };
        let image = item.entries.iter().find_map(|entry| match entry {
            ClipboardEntry::Image(image) => Some(image),
            _ => None,
        });
        let Some(image) = image else {
            self.notice("No image found in the clipboard.");
            return;
        };
        let media_dir = slate_app::ipc::user_data_dir().join("media");
        if let Err(error) = std::fs::create_dir_all(&media_dir) {
            self.notice(format!("Failed to save clipboard image: {error}"));
            return;
        }
        let extension = image.format.extension();
        let path = media_dir.join(format!(
            "pasted-{}.{}",
            uuid::Uuid::new_v4().simple(),
            extension
        ));
        if let Err(error) = std::fs::write(&path, &image.bytes) {
            self.notice(format!("Failed to save clipboard image: {error}"));
            return;
        }
        if self.canvas.widgets.len() >= projection::MAX_WIDGETS {
            self.notice("Canvas is full — close a widget before adding another.");
            return;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Image".to_owned());
        let (w, h) = self.spawn_size("image");
        // The shell centred it on the viewport's world point.
        let view_now = canvas::View::new(&self.canvas.camera, Pos2::ZERO);
        let center = view_now.to_world(Pos2::new(self.viewport.x / 2.0, self.viewport.y / 2.0));
        // `addWidget` anchors spawns at `point - 16`.
        let at = Pos2::new(
            center.x - (w / 2.0) as f32 - 16.0,
            center.y - (h / 2.0) as f32 - 16.0,
        );
        let z = self
            .canvas
            .widgets
            .values()
            .map(|widget| widget.z)
            .fold(0.0, f64::max)
            + 1.0;
        let id = self.fresh_id("image");
        let image_path = path.to_string_lossy().into_owned();
        self.commit(
            "widget.create",
            &format!("widget:{id}"),
            json!({
                "id": id, "title": name, "kind": "image",
                "x": at.x as f64, "y": at.y as f64,
                "w": w, "h": h, "z": z, "maximized": false,
                "imagePath": image_path, "imageName": name,
            }),
        );
        self.canvas.widgets.insert(
            id.clone(),
            Widget {
                id,
                title: name.clone(),
                kind: Some("image".to_owned()),
                x: at.x as f64,
                y: at.y as f64,
                w,
                h,
                z,
                maximized: false,
                image_path: Some(image_path),
                image_name: Some(name),
                state: None,
                version: 1.0,
                updated_at: 0.0,
            },
        );
        cx.notify();
    }

    /// Spawn centred in the current viewport, like the Electron app's T/N.
    fn spawn_terminal(&mut self, viewport: Vec2, cx: &mut Context<Self>) {
        let view = self.view();
        let center = view.to_world(Pos2::new(viewport.x / 2.0, viewport.y / 2.0));
        let (w, h) = default_size("terminal");
        let at = Pos2::new(center.x - (w / 2.0) as f32, center.y - (h / 2.0) as f32);
        self.spawn_terminal_at(at, cx);
    }

    fn spawn_terminal_at(&mut self, at: Pos2, cx: &mut Context<Self>) -> String {
        let id = self.fresh_id("terminal");
        // pickTerminalName: a random start into the fixed pool, skipping any
        // name an existing terminal already shows (titles and CLI-assigned
        // names both count — `slate tell` resolves by either).
        let taken: HashSet<String> = self
            .canvas
            .widgets
            .values()
            .filter(|w| is_terminal_kind(w.kind.as_deref()))
            .map(|w| w.title.to_lowercase())
            .chain(self.manager.names().values().map(|n| n.to_lowercase()))
            .collect();
        // `pickTerminalName({favorites, taken})` — settings favorites win
        // over the default pool.
        let title = slate_app::terminal_names::pick_with_favorites(
            &self.settings.favorite_terminal_names,
            &taken,
        )
        .unwrap_or_else(|| "Terminal".to_owned());
        self.create_widget_at(&id, "terminal", &title, at);
        self.manager.set_name(&id, &title);
        let (w, h) = default_size("terminal");
        let (cols, rows) = terminal_cells(w, h, self.canvas.camera.zoom);
        let _ = self.manager.spawn_with_options(
            id.clone(),
            cols,
            rows,
            None,
            slate_app::platform::shell_command(),
            HashMap::new(),
        );
        self.active = Some(id.clone());
        self.frame_focus = None;
        cx.notify();
        id
    }

    /// The right-click catalog: `favoriteWidgets` order, unrenderable kinds
    /// dropped — the shell's ContextMenu filtered the same list.
    fn menu_items(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        let mut items: Vec<(&'static str, &'static str, &'static str)> = Vec::new();
        for kind in self.settings.favorite_widgets() {
            if let Some(entry) = WIDGET_CATALOG.iter().find(|(k, _, _)| k == kind) {
                if renderable(entry.0) {
                    items.push(*entry);
                }
            }
        }
        items
    }

    /// Right-click menu pick: any catalog kind, created where the menu was
    /// opened. Terminals also get a PTY; the rest are content widgets.
    fn spawn_menu_widget(&mut self, kind: &str, at_screen: Pos2, cx: &mut Context<Self>) {
        let at = self.view().to_world(at_screen);
        if kind == "terminal" {
            self.spawn_terminal_at(at, cx);
            return;
        }
        let id = self.fresh_id(kind);
        self.create_widget_at(&id, kind, &kind_title(kind), at);
        self.active = Some(id.clone());
        // A fresh pane's frame holds focus — arrows nudge it immediately,
        // same as after a header press in the shell.
        self.frame_focus = Some(id);
        cx.notify();
    }

    /// The command bar's Enter: the resolved kind opens at viewport centre,
    /// and for a terminal the rest of the line is queued as the initial
    /// command — the Electron shell's createWidgetFromCommand.
    fn create_widget_from_command(
        &mut self,
        kind: &str,
        initial: &str,
        viewport: Vec2,
        cx: &mut Context<Self>,
    ) {
        let center = self
            .view()
            .to_world(Pos2::new(viewport.x / 2.0, viewport.y / 2.0));
        let (w, h) = default_size(kind);
        let at = Pos2::new(center.x - (w / 2.0) as f32, center.y - (h / 2.0) as f32);
        if kind == "terminal" {
            let id = self.spawn_terminal_at(at, cx);
            let command = initial.trim();
            if !command.is_empty() {
                self.pending_commands.push(PendingCommand {
                    terminal_id: id,
                    command: command.to_owned(),
                    attempts: 0,
                    watch: false,
                });
            }
            return;
        }
        let id = self.fresh_id(kind);
        self.create_widget_at(&id, kind, &kind_title(kind), at);
        self.active = Some(id);
        cx.notify();
    }

    /// The header's maximize/restore button — `canvas.toggleMaximize`.
    /// Maximized state lives on the widget itself, so it survives journal
    /// replay exactly like the Electron version.
    fn toggle_maximize(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(widget) = self.canvas.widgets.get_mut(id) else {
            return;
        };
        if !widget.maximized && canvas::non_maximizable(widget.kind.as_deref()) {
            return;
        }
        widget.maximized = !widget.maximized;
        let widget = widget.clone();
        self.commit_widget(&widget);
        self.active = Some(id.to_owned());
        cx.notify();
    }

    fn close_widget(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(widget) = self.canvas.widgets.get(id) {
            if is_terminal_kind(widget.kind.as_deref()) {
                let _ = self.manager.dispose(id);
            }
        }
        self.canvas.widgets.shift_remove(id);
        self.canvas
            .connections
            .retain(|c| c.from != id && c.to != id);
        self.commit("widget.remove", &format!("widget:{id}"), json!({}));
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        if self.frame_focus.as_deref() == Some(id) {
            self.frame_focus = None;
        }
        if self.pending_close.as_deref() == Some(id) {
            self.pending_close = None;
        }
        self.term_cells.remove(id);
        self.startup_watch.remove(id);
        self.startup_notice.remove(id);
        // `vn` — widget dispose dropped the localStorage agent keys too.
        if self.agent_picks.launched.contains_key(id) || self.agent_picks.select.contains_key(id) {
            self.agent_picks.clear(id);
            if let Err(error) = self.agent_picks.save() {
                self.notice(format!("agent pick: {error}"));
            }
        }
        cx.notify();
    }

    fn view(&self) -> canvas::View {
        canvas::View::new(&self.canvas.camera, Pos2::ZERO)
    }

    /// The command bar's `activeTerminalId`: the configured target while
    /// that terminal still lives, else the topmost one — the shell's
    /// `terminals[0]?.id` fallback on its z-descending list.
    fn command_target_terminal(&self) -> Option<String> {
        if let Some(target) = self
            .command_target
            .as_deref()
            .filter(|id| self.snapshots.iter().any(|s| &s.id == *id))
        {
            return Some(target.to_owned());
        }
        self.canvas
            .widgets
            .values()
            .filter(|w| is_terminal_kind(w.kind.as_deref()))
            .filter(|w| self.snapshots.iter().any(|s| s.id == w.id))
            .max_by(|a, b| a.z.total_cmp(&b.z))
            .map(|w| w.id.clone())
    }

    /// The suggestion listbox's contents — the shell gated it to command
    /// mode (`mode !== 'command'` returned `[]`).
    fn command_suggestions(&self) -> Vec<(String, String, String)> {
        if self.command_message {
            return Vec::new();
        }
        toolbar_suggestions(
            self.command_bar.as_deref().unwrap_or_default(),
            self.settings.command_prefix(),
        )
    }

    /// `chooseSuggestion` — fill the input with `prefix + kind + " "` and
    /// keep it open for the rest of the line.
    fn adopt_suggestion(&mut self) {
        let suggestions = self.command_suggestions();
        let Some((kind, _, _)) = suggestions
            .get(self.command_suggestion.min(suggestions.len().saturating_sub(1)))
        else {
            return;
        };
        let kind = kind.clone();
        let prefix = self
            .command_bar
            .as_deref()
            .and_then(|b| b.trim_start().chars().next())
            .filter(|c| matches!(c, '/' | '.' | '@'))
            .map(|c| c.to_string())
            .unwrap_or_else(|| {
                // `typedPrefix || (commandPrefix === 'any' ? '/' : p)`.
                let prefix = self.settings.command_prefix();
                if prefix == "any" {
                    "/".to_owned()
                } else {
                    prefix.to_owned()
                }
            });
        self.command_bar = Some(format!("{prefix}{kind} "));
        self.command_suggestion = 0;
    }

    /// Transient toast — the shell's `notice` state: cap hits, control-route
    /// failures, save errors. Auto-clears after four seconds of display.
    fn notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now() + Duration::from_secs(4)));
    }

    /// A pane's mutation against the control server — the same routes the
    /// `slate` CLI calls. The engine stays the single writer; a direct file
    /// write from here would be clobbered by the next routed persist.
    fn control_post(&mut self, method: &str, path: &str, body: Option<Value>) {
        if let Err(error) = slate_app::ipc::control_request(method, path, body.as_ref()) {
            self.notice(error);
        }
    }

    /// Pane-level actions arrive here as `{ "op": … }` JSON from the views.
    /// `set` merges a patch into the widget's journaled `state` object — that
    /// is how calendar month cursors and collapsed columns persist across
    /// restarts. Unknown ops are ignored so panes degrade gracefully.
    pub(crate) fn widget_command(&mut self, id: &str, action: Value, cx: &mut Context<Self>) {
        match action.get("op").and_then(Value::as_str) {
            Some("set") => {
                if let Some(widget) = self.canvas.widgets.get_mut(id) {
                    let mut state = widget.state.take().unwrap_or_else(|| json!({}));
                    if let (Some(dst), Some(src)) = (
                        state.as_object_mut(),
                        action.get("state").and_then(Value::as_object),
                    ) {
                        for (key, value) in src {
                            dst.insert(key.clone(), value.clone());
                        }
                    }
                    widget.state = Some(state.clone());
                    self.commit(
                        "widget.update",
                        &format!("widget:{id}"),
                        json!({ "state": state }),
                    );
                }
            }
            // Planner pane row click — same journal entries `slate plan
            // toggle`/`update` write, so the planner document refolds the
            // same way whichever surface did it.
            Some("plan_toggle") => {
                if let (Some(item), Some(done)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("done").and_then(Value::as_bool),
                ) {
                    self.commit(
                        "plan.toggle",
                        &format!("plan:{item}"),
                        json!({ "done": done }),
                    );
                }
            }
            Some("plan_move") => {
                if let (Some(item), Some(status)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("status").and_then(Value::as_str),
                ) {
                    // Planner scopes derive from `day` — the pane resolves
                    // the concrete date (JSON null = inbox) and it must be
                    // journaled verbatim, since absent ≠ clear in the fold.
                    let mut payload = json!({ "status": status });
                    if let Some(day) = action.get("day") {
                        payload["day"] = day.clone();
                    }
                    self.commit("plan.update", &format!("plan:{item}"), payload);
                }
            }
            Some("plan_create") => {
                // Same payload keys `slate plan create` writes — title plus
                // the optional metadata, nothing empty.
                let mut payload = action.clone();
                payload.as_object_mut().map(|map| map.remove("op"));
                self.commit("plan.create", "plan:new", payload);
            }
            Some("plan_update") => {
                if let Some(item) = action.get("id").and_then(Value::as_str) {
                    let patch = action.get("patch").cloned().unwrap_or_else(|| json!({}));
                    self.commit("plan.update", &format!("plan:{item}"), patch);
                }
            }
            Some("plan_delete") => {
                if let Some(item) = action.get("id").and_then(Value::as_str) {
                    self.commit("plan.delete", &format!("plan:{item}"), json!({}));
                }
            }
            // Orchestration pane ops — every one maps to the route the CLI
            // verb of the same name calls, so the pane and `slate` can never
            // disagree about the store.
            Some("orc_ask_reply") => {
                if let (Some(ask_id), Some(body)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("body").and_then(Value::as_str),
                ) {
                    let to = slate_app::ipc::control_request(
                        "GET",
                        &format!("/orchestration/messages/{ask_id}"),
                        None,
                    )
                    .ok()
                    .and_then(|value| {
                        value
                            .get("message")?
                            .get("from")?
                            .as_str()
                            .map(str::to_owned)
                    });
                    let mut payload = json!({
                        "type": "reply",
                        "replyTo": ask_id,
                        "body": body,
                        "subject": "reply",
                    });
                    if let Some(to) = to {
                        payload["to"] = json!(to);
                    }
                    self.control_post("POST", "/orchestration/messages", Some(payload));
                }
            }
            Some("orc_allow") | Some("orc_deny") => {
                let op = action.get("op").and_then(Value::as_str).unwrap_or("");
                if let Some(ask_id) = action.get("id").and_then(Value::as_str) {
                    let note = action
                        .get("note")
                        .or_else(|| action.get("reason"))
                        .and_then(Value::as_str)
                        .filter(|note| !note.is_empty());
                    let (verb, subject) = if op == "orc_allow" {
                        ("allow", "permission_granted")
                    } else {
                        ("deny", "permission_denied")
                    };
                    let body = match note {
                        Some(note) => format!("{verb}: {note}"),
                        None => verb.to_owned(),
                    };
                    let to = slate_app::ipc::control_request(
                        "GET",
                        &format!("/orchestration/messages/{ask_id}"),
                        None,
                    )
                    .ok()
                    .and_then(|value| {
                        value
                            .get("message")?
                            .get("from")?
                            .as_str()
                            .map(str::to_owned)
                    });
                    let mut payload = json!({
                        "type": "reply",
                        "replyTo": ask_id,
                        "body": body,
                        "subject": subject,
                    });
                    if let Some(to) = to {
                        payload["to"] = json!(to);
                    }
                    self.control_post("POST", "/orchestration/messages", Some(payload));
                }
            }
            Some("orc_gate") => {
                if let (Some(gate), Some(resolution)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("resolution").and_then(Value::as_str),
                ) {
                    self.control_post(
                        "POST",
                        &format!("/orchestration/gates/{gate}/resolve"),
                        Some(json!({ "resolution": resolution })),
                    );
                }
            }
            Some("orc_ack") => {
                if let Some(message) = action.get("id").and_then(Value::as_str) {
                    self.control_post(
                        "POST",
                        &format!("/orchestration/messages/{message}/ack"),
                        Some(json!({})),
                    );
                }
            }
            Some("orc_release") | Some("orc_retain") => {
                let op = action.get("op").and_then(Value::as_str).unwrap_or("");
                if let Some(dispatch) = action.get("id").and_then(Value::as_str) {
                    let state = if op == "orc_retain" {
                        "retained"
                    } else {
                        "released"
                    };
                    self.control_post(
                        "POST",
                        &format!("/orchestration/dispatches/{dispatch}/account"),
                        Some(json!({ "state": state })),
                    );
                }
            }
            // Files pane ops — pure filesystem work the pane itself can't
            // own; confirmed twice on the pane side before this runs.
            Some("file_delete") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    let path = std::path::Path::new(path);
                    let result = if path.is_dir() {
                        std::fs::remove_dir_all(path)
                    } else {
                        std::fs::remove_file(path)
                    };
                    if let Err(error) = result {
                        self.notice(format!("delete {}: {error}", path.display()));
                    }
                }
            }
            Some("file_new") => {
                if let (Some(dir), Some(kind)) = (
                    action.get("dir").and_then(Value::as_str),
                    action.get("kind").and_then(Value::as_str),
                ) {
                    // `untitled`, `untitled-1`, … — the shell's placeholder
                    // names; rename happens after the row exists.
                    let dir = std::path::Path::new(dir);
                    let mut candidate = dir.join("untitled");
                    for index in 1usize.. {
                        if !candidate.exists() {
                            break;
                        }
                        candidate = dir.join(format!("untitled-{index}"));
                    }
                    let result = if kind == "dir" {
                        std::fs::create_dir(&candidate)
                    } else {
                        std::fs::File::create(&candidate).map(|_| ())
                    };
                    if let Err(error) = result {
                        self.notice(format!("create {}: {error}", candidate.display()));
                    }
                }
            }
            Some("file_open") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
                }
            }
            Some("file_reveal") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    let path = std::path::Path::new(path);
                    let dir = if path.is_dir() {
                        path
                    } else {
                        path.parent().unwrap_or(path)
                    };
                    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
                }
            }
            Some("file_copy_path") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.to_owned()));
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Report one pointer event to a terminal that armed mouse tracking —
    /// the screen keeps the mode state, so a terminal that never asked for
    /// it simply produces no bytes.
    fn terminal_mouse(
        &mut self,
        id: &str,
        kind: MouseKind,
        rect: canvas::Rect,
        zoom: f32,
        pointer: Pos2,
        modifiers: gpui::Modifiers,
    ) {
        let reporting = self
            .snapshots
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.mouse_reporting)
            .unwrap_or(false);
        if !reporting {
            return;
        }
        let (col, row) = terminal_cell(rect, zoom, pointer);
        let mods = KeyMods {
            shift: modifiers.shift,
            alt: modifiers.alt,
            ctrl: modifiers.control,
            mac_cmd: modifiers.platform,
        };
        let _ = self.manager.mouse_input(id, kind, col, row, mods);
    }

    /// Keep the cached snapshot's highlight in sync mid-drag — waiting for
    /// the 250ms snapshot tick would make the selection trail the pointer.
    fn refresh_selection(&mut self, id: &str) {
        if let Some(snap) = self.snapshots.iter_mut().find(|s| s.id == id) {
            snap.selection = self.manager.selection_ranges(id);
        }
    }

    fn raise(&mut self, id: &str) {
        let top = self
            .canvas
            .widgets
            .values()
            .map(|widget| widget.z)
            .fold(0.0, f64::max);
        let updated = {
            let Some(widget) = self.canvas.widgets.get_mut(id) else {
                return;
            };
            if widget.z < top {
                widget.z = top + 1.0;
                Some(widget.clone())
            } else {
                None
            }
        };
        // bringToFront journaled the new z on press — a click that raises
        // without dragging still has to reach the store.
        if let Some(widget) = updated {
            self.commit_widget(&widget);
        }
    }

    /// Fit every widget into the viewport (the original's F key). The shell's
    /// fitCameraToRect skipped maximized widgets — their stored rect is the
    /// pre-maximize frame — and never zoomed in: `min(1, …)`.
    fn fit(&mut self, viewport: Vec2, cx: &mut Context<Self>) {
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        let mut any = false;
        for widget in self.canvas.widgets.values() {
            if widget.maximized {
                continue;
            }
            any = true;
            min_x = min_x.min(widget.x);
            min_y = min_y.min(widget.y);
            max_x = max_x.max(widget.x + widget.w);
            max_y = max_y.max(widget.y + widget.h);
        }
        if !any {
            return;
        }
        let (bw, bh) = ((max_x - min_x).max(1.0), (max_y - min_y).max(1.0));
        let zoom = ((viewport.x as f64 - 64.0) / bw)
            .min((viewport.y as f64 - 64.0) / bh)
            .min(1.0)
            .max(canvas::MIN_ZOOM as f64);
        self.canvas.camera.zoom = zoom;
        self.canvas.camera.x = (viewport.x as f64 - bw * zoom) / 2.0 - min_x * zoom;
        self.canvas.camera.y = (viewport.y as f64 - bh * zoom) / 2.0 - min_y * zoom;
        self.commit_camera();
        cx.notify();
    }

    fn widget_kind(&self, id: &str) -> Option<String> {
        self.canvas
            .widgets
            .get(id)
            .and_then(|widget| widget.kind.clone())
    }
}

/// The title case the spawn paths give every kind ("files" → "Files").
/// The shell's `Ht` titles — `Sys Monitor` is not the widget's name.
fn kind_title(kind: &str) -> String {
    match kind {
        "sys-monitor" => return "System Monitor".to_owned(),
        "music-player" => return "Music Player".to_owned(),
        "chat" => return "AI Chat".to_owned(),
        _ => {}
    }
    kind.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// (idle, total) jiffies from /proc/stat's aggregate `cpu` line — two
/// samples apart make a busy percentage, the same trick the Electron
/// StatusBar's cpu probe used.
#[cfg(unix)]
fn proc_stat_sample() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let nums: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if nums.len() < 4 {
        return None;
    }
    let idle = nums[3] + nums.get(4).copied().unwrap_or(0);
    let total: u64 = nums.iter().take(8).sum();
    Some((idle, total))
}

#[cfg(not(unix))]
fn proc_stat_sample() -> Option<(u64, u64)> {
    None
}

/// Local wall clock as (hour, minute, second) — libc's localtime_r on unix,
/// UTC arithmetic elsewhere.
fn clock_hms() -> (u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    #[cfg(unix)]
    {
        unsafe {
            let raw = secs as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&raw, &mut tm).is_null() {
                return (tm.tm_hour as u32, tm.tm_min as u32, tm.tm_sec as u32);
            }
        }
    }
    (
        (secs / 3600 % 24) as u32,
        (secs / 60 % 60) as u32,
        (secs % 60) as u32,
    )
}

/// Cells a terminal body of this world size displays at this zoom: the
/// renderer scales the text with the frame, so the grid shrinks as you zoom
/// out — same contract the Electron app had with xterm + FitAddon.
/// CellRgb (r,g,b) packs into the u32 gpui's rgb() takes.
fn pack((r, g, b): slate_app::terminal_screen::CellRgb) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// The frame's screen rect — the world rect through the camera, except a
/// maximized widget, which is the shell's fixed overlay: the whole viewport
/// under the title band. Hit-tests and culling all use this so a maximized
/// frame behaves exactly where it is drawn.
fn widget_screen_rect(widget: &Widget, view: &canvas::View, viewport: Vec2) -> canvas::Rect {
    if widget.maximized {
        canvas::Rect::from_min_size(
            Pos2::new(0.0, canvas::TITLE_BAR_HEIGHT),
            Vec2::new(viewport.x, (viewport.y - canvas::TITLE_BAR_HEIGHT).max(0.0)),
        )
    } else {
        view.widget_rect(widget)
    }
}

fn terminal_cells(w: f64, h: f64, zoom: f64) -> (u16, u16) {
    let body_w = (w * zoom - 2.0).max(40.0) as f32;
    let body_h = (h * zoom - theme::geometry::HEADER_HEIGHT as f64 * zoom).max(20.0) as f32;
    let cols = (body_w / (8.4 * zoom as f32).max(2.0)).clamp(20.0, 500.0) as u16;
    let rows = (body_h / (17.0 * zoom as f32).max(2.0)).clamp(4.0, 300.0) as u16;
    (cols, rows)
}

/// Pointer position → terminal cell. The rendered metrics, not the grid
/// sizing's guesses: rows stack `h(font)` (12·zoom) apart and monospaced
/// text advances ≈0.6em (7.2·zoom), so a click lands on the cell the glyph
/// actually occupies. The 4px body padding (p_1) and 1px frame border come
/// off the top-left first.
fn terminal_cell(rect: canvas::Rect, zoom: f32, pointer: Pos2) -> (u16, u16) {
    let zoom = zoom.max(0.1);
    let body_x = rect.min.x + 5.0;
    let body_y = rect.min.y + 1.0 + theme::geometry::HEADER_HEIGHT * zoom + 4.0;
    let col = ((pointer.x - body_x) / (7.2 * zoom)).floor().max(0.0);
    let row = ((pointer.y - body_y) / (12.0 * zoom)).floor().max(0.0);
    (col.min(999.0) as u16, row.min(999.0) as u16)
}

/// `settings.background` — the shell stored the wallpaper as a
/// `data:image/<fmt>;base64,…` string (`Ga` gated it to a small whitelist
/// of raster formats). Decode it into a gpui Image so `img()` can render
/// it without a file on disk.
fn decode_data_image(url: &str) -> Option<gpui::ImageSource> {
    let rest = url.strip_prefix("data:image/")?;
    let (format, payload) = rest.split_once(";base64,")?;
    let format = match format {
        "png" => gpui::ImageFormat::Png,
        "jpg" | "jpeg" => gpui::ImageFormat::Jpeg,
        "gif" => gpui::ImageFormat::Gif,
        "webp" => gpui::ImageFormat::Webp,
        "bmp" => gpui::ImageFormat::Bmp,
        _ => return None,
    };
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some(std::sync::Arc::new(gpui::Image::from_bytes(format, bytes)).into())
}

/// A stroke's "#rrggbb" journal color as the 0xRRGGBBAA `rgba()` wants —
/// opaque. Anything unreadable falls back to the default white.
fn stroke_color_u32(color: &str) -> u32 {
    let hex = color.strip_prefix('#').unwrap_or(color);
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0xffffff);
    (rgb << 8) | 0xff
}

/// StrokeLayer's `<polyline>`: a stroked path through the points. Under two
/// points there is no line — the same check the SVG's render skipped.
fn paint_polyline(window: &mut Window, points: &[Pos2], color: impl Into<Background> + Copy) {
    if points.len() < 2 {
        return;
    }
    let mut builder = PathBuilder::stroke(px(3.0));
    builder.move_to(point(px(points[0].x), px(points[0].y)));
    for p in &points[1..] {
        builder.line_to(point(px(p.x), px(p.y)));
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn widget_body(
    widget: &Widget,
    snapshots: &[TerminalSnapshot],
    zoom: f32,
    terminals_flipped: bool,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let font = px(12.0 * zoom);
    match widget.kind.as_deref() {
        Some("terminal") | None => {
            let snapshot = snapshots.iter().find(|s| s.id == widget.id);
            if terminals_flipped {
                // `terminal-flip-card` — the shell swapped the xterm for a
                // centred MessageSquareText + the last user prompt. The
                // prompt feed doesn't exist natively, so the card carries
                // the widget's title (the name the rename flow keeps
                // fresh) and the PTY's cwd underneath.
                let cwd = snapshot
                    .map(|s| s.cwd.trim().to_owned())
                    .filter(|cwd| !cwd.is_empty())
                    .unwrap_or_else(|| "No user prompt yet".to_owned());
                return div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .overflow_hidden()
                    .bg(rgb(theme::hex(theme::monochrome::BASE)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(6.0))
                            .px_4()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(10.0))
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(px(14.0 * zoom))
                                            .text_color(rgb(theme::hex(theme::text::DIM)))
                                            .child("◈"),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_size(px((14.0 * zoom).clamp(13.0, 26.0)))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                            .child(widget.title.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0 * zoom))
                                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                                    .child(cwd),
                            ),
                    )
                    .into_any_element();
            }
            let mut body = div()
                .size_full()
                .p_1()
                .font_family("monospace")
                .text_size(font)
                .line_height(gpui::px(17.0 * zoom))
                .text_color(rgb(theme::hex(theme::text::DIM)));
            let styled = snapshot.map(|s| s.styled.as_slice()).unwrap_or(&[]);
            let blank = styled
                .iter()
                .all(|row| row.iter().all(|run| run.text.trim().is_empty()));
            if styled.is_empty() || blank {
                body = body.child("…");
            } else {
                let base_bg = theme::hex(theme::monochrome::BASE);
                for row in styled {
                    let mut line = div().h(font).flex().flex_row().flex_none();
                    if row.is_empty() {
                        line = line.child("\u{00a0}");
                    }
                    for run in row {
                        // All-whitespace runs (notably the cursor cell) get
                        // NBSPs so the text shaper keeps their width.
                        let text = if run.text.trim().is_empty() {
                            "\u{00a0}".repeat(run.text.chars().count().max(1))
                        } else {
                            run.text.clone()
                        };
                        let mut span = div().flex_none().text_color(rgb(pack(run.fg))).child(text);
                        let bg = pack(run.bg);
                        if bg != base_bg {
                            span = span.bg(rgb(bg));
                        }
                        if run.bold {
                            span = span.font_weight(gpui::FontWeight::BOLD);
                        }
                        if run.italic {
                            span = span.italic();
                        }
                        if run.underline {
                            span = span.underline();
                        }
                        line = line.child(span);
                    }
                    body = body.child(line);
                }
            }
            // The drag-selection highlight — xterm's selectionBackground
            // rgba(120,160,255,0.35). Cells ride the body's p_1 padding, so
            // the overlay starts at the padding edge; the cell geometry is
            // `terminal_cell`'s inverse (7.2·zoom cols, 12·zoom rows).
            let mut container = div().size_full().relative().child(body);
            if let Some(snapshot) = snapshot {
                if !snapshot.selection.is_empty() {
                    let cell_w = px(7.2 * zoom);
                    let cell_h = px(12.0 * zoom);
                    let mut overlay = div().absolute().left(px(4.0)).top(px(4.0));
                    for &(row, start, end) in &snapshot.selection {
                        overlay = overlay.child(
                            div()
                                .absolute()
                                .left(cell_w * start as f32)
                                .top(cell_h * row as f32)
                                .w(cell_w * (end - start) as f32)
                                .h(cell_h)
                                .bg(rgba(0x78a0ff59)),
                        );
                    }
                    container = container.child(overlay);
                }
            }
            container.into_any_element()
        }
        Some("planner") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_planner::planner_pane(widget, cx))
            .into_any_element(),
        Some("files") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_files::files_pane(
                &std::env::current_dir().unwrap_or_default(),
                widget,
                cx,
            ))
            .into_any_element(),
        Some("orchestration") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_orchestration::orchestration_pane(
                &load_orchestration_store(),
                widget,
                cx,
            ))
            .into_any_element(),
        Some("browser") => div()
            .size_full()
            .text_size(font)
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .p_2()
            .child("Browser — no embedded webview in this build".to_owned())
            .into_any_element(),
        Some("notes") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_notes::notes_pane(widget, cx))
            .into_any_element(),
        Some("timer") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_timer::timer_pane(&widget.id, cx))
            .into_any_element(),
        Some("sys-monitor") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_sysmon::sysmon_pane(cx))
            .into_any_element(),
        Some("kanban") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_kanban::kanban_pane(widget, cx))
            .into_any_element(),
        Some("calendar") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_calendar::calendar_pane(widget, cx))
            .into_any_element(),
        Some("links") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_links::links_pane(&widget.id, cx))
            .into_any_element(),
        Some("image") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_image::image_pane(
                widget.image_path.as_deref(),
                widget.image_name.as_deref(),
                cx,
            ))
            .into_any_element(),
        other => div()
            .size_full()
            .text_size(font)
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .p_2()
            .child(format!(
                "{} widget is not available in this build",
                other.unwrap_or("unknown")
            ))
            .into_any_element(),
    }
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let viewport_size = Vec2::new(f32::from(viewport.width), f32::from(viewport.height));
        let viewport_rect = canvas::Rect::from_min_size(Pos2::ZERO, viewport_size);
        self.viewport = viewport_size;
        let view = self.view();
        let zoom = view.zoom;

        // PTYs follow their frame's rendered size, resized on change only.
        let mut sizes: Vec<(String, u16, u16)> = Vec::new();
        for widget in self.canvas.widgets.values() {
            if is_terminal_kind(widget.kind.as_deref()) {
                // A maximized widget renders the size of the viewport below
                // the title band, so feed the PTY its real grid rather than
                // the saved world size.
                let (w, h) = if widget.maximized {
                    (
                        (viewport_size.x / zoom) as f64,
                        ((viewport_size.y - canvas::TITLE_BAR_HEIGHT) / zoom) as f64,
                    )
                } else {
                    (widget.w, widget.h)
                };
                let (cols, rows) = terminal_cells(w, h, zoom as f64);
                sizes.push((widget.id.clone(), cols, rows));
            }
        }
        for (id, cols, rows) in sizes {
            if self.term_cells.get(&id) != Some(&(cols, rows))
                && self.manager.resize(&id, cols, rows).is_ok()
            {
                self.term_cells.insert(id, (cols, rows));
            }
        }

        let camera = self.canvas.camera.clone();
        let mut root = div()
            .id("slate-canvas")
            .size_full()
            .relative()
            .overflow_hidden()
            .bg(rgb(theme::hex(theme::monochrome::BASE)))
            // draw and erase both take the shell's crosshair cursor.
            .when(self.draw_mode || self.erase_mode, |el| {
                el.cursor_crosshair()
            })
            .key_context("canvas")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
                this.on_key(event, viewport_size, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    // A click outside the input closes the command bar; the
                    // gesture itself still runs, like the shell's blur.
                    this.command_bar = None;
                    this.agent_menu = None;
                    // The rename input committed on blur — clicking the
                    // canvas away from it does the same.
                    if this.renaming.is_some() {
                        this.commit_rename(cx);
                    }
                    if this.menu.take().is_some() {
                        cx.notify();
                        return;
                    }
                    this.active = None;
                    this.frame_focus = None;
                    if event.modifiers.shift {
                        // `button===0 && shiftKey` pans regardless of the
                        // current tool — the shell's escape hatch.
                        this.drag = Some(Drag::Pan {
                            last: pos(event.position),
                        });
                    } else if this.erase_mode {
                        // eraseAt fires on pointerdown, not just on drag —
                        // a click erases whatever sits under the cursor.
                        let world = canvas::View::new(&this.canvas.camera, Pos2::ZERO)
                            .to_world(pos(event.position));
                        let dirty = this.erase_at(world);
                        this.drag = Some(Drag::Erase { dirty });
                    } else if this.draw_mode {
                        this.pending_stroke.clear();
                        this.drag = Some(Drag::Stroke {
                            start: pos(event.position),
                            moved: false,
                        });
                    } else if this.select_mode {
                        this.drag = Some(Drag::Select {
                            start: pos(event.position),
                            rect: None,
                        });
                    } else {
                        this.drag = Some(Drag::Pan {
                            last: pos(event.position),
                        });
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                    // `button===1` pans on any tool — the shell's middle-drag.
                    this.drag = Some(Drag::Pan {
                        last: pos(event.position),
                    });
                    cx.notify();
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    if let Some(Drag::Pan { .. }) = this.drag.take() {
                        this.camera_dirty_at = None;
                        this.commit_camera();
                        cx.notify();
                    }
                }),
            )
            // OS file drags: the move listener both marks the canvas a drop
            // target and tracks where the files are hovering (the drop
            // listener itself gets no position).
            .on_drag_move::<ExternalPaths>(cx.listener(
                |this, event: &DragMoveEvent<ExternalPaths>, _window, _cx| {
                    this.drop_point = Some(pos(event.event.position));
                },
            ))
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.handle_file_drop(paths, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    this.agent_menu = None;
                    let pointer = pos(event.position);
                    let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                    // The menu opens on bare canvas only — right-click inside
                    // a widget stays with the widget.
                    let size = window.viewport_size();
                    let viewport = Vec2::new(f32::from(size.width), f32::from(size.height));
                    let over_widget = this
                        .canvas
                        .widgets
                        .values()
                        .any(|w| widget_screen_rect(w, &view_now, viewport).contains(pointer));
                    this.menu_sel = 0;
                    this.menu = (!over_widget).then_some(pointer);
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                let pointer = pos(event.position);
                // An in-progress terminal selection follows the pointer even
                // outside its widget — xterm keeps tracking until mouseup.
                if let Some(sel_id) = this.selection_drag.clone() {
                    if let Some(widget) = this.canvas.widgets.get(&sel_id) {
                        let size = _window.viewport_size();
                        let viewport = Vec2::new(f32::from(size.width), f32::from(size.height));
                        let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                        let rect = widget_screen_rect(widget, &view_now, viewport);
                        let zoom = this.canvas.camera.zoom as f32;
                        let (col, row) = terminal_cell(rect, zoom, pointer);
                        let _ = this.manager.selection_update(&sel_id, col, row);
                        this.refresh_selection(&sel_id);
                        cx.notify();
                    }
                    return;
                }
                match &this.drag {
                    Some(Drag::Pan { last }) => {
                        let dx = pointer.x - last.x;
                        let dy = pointer.y - last.y;
                        this.canvas.camera.x += dx as f64;
                        this.canvas.camera.y += dy as f64;
                        this.drag = Some(Drag::Pan { last: pointer });
                        this.camera_dirty_at = Some(Instant::now());
                        cx.notify();
                    }
                    Some(Drag::Widget {
                        id,
                        grab,
                        press,
                        moved,
                    }) => {
                        let id = id.clone();
                        let grab = *grab;
                        let press = *press;
                        let moved = *moved;
                        // Under 3 screen px of travel the press is a click —
                        // the widget never moves (the original's threshold).
                        if !moved && (pointer - press).length() < 3.0 {
                            return;
                        }
                        let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                        let world = view_now.to_world(pointer);
                        // The header drag also keeps the top edge out of the
                        // title band (the original's titleBarWorldY floor).
                        let min_y = canvas::title_bar_world_y(
                            this.canvas.camera.y,
                            this.canvas.camera.zoom,
                        ) as f32;
                        // The shell's snap pass: edge/center alignment
                        // against every other non-maximized widget plus the
                        // origin, inside 8 screen px (8/zoom world) — Alt
                        // turns it off for the drag.
                        let st = (world.x - grab.x) as f64;
                        let lt = ((world.y - grab.y) as f64).max(min_y as f64);
                        let (mut jt, mut mt) = (st, lt);
                        let (mut gx, mut gy) = (None, None);
                        let drag_wh = this
                            .canvas
                            .widgets
                            .get(&id)
                            .map(|w| (w.w, w.h))
                            .unwrap_or((0.0, 0.0));
                        if !event.modifiers.alt {
                            let threshold = 8.0 / this.canvas.camera.zoom.max(1e-6);
                            let (pw, ph) = drag_wh;
                            let (cx_s, cy_s) = (st + pw / 2.0, lt + ph / 2.0);
                            let (mut wt, mut mtm) = (threshold, threshold);
                            for o in this
                                .canvas
                                .widgets
                                .values()
                                .filter(|w| w.id != id && !w.maximized)
                            {
                                let (right, bottom) = (o.x + o.w, o.y + o.h);
                                let (ocx, ocy) = (o.x + o.w / 2.0, o.y + o.h / 2.0);
                                for (d, jx, gl) in [
                                    ((st - o.x).abs(), o.x, o.x),
                                    ((st + pw - right).abs(), right - pw, right),
                                    ((st - right).abs(), right, right),
                                    ((st + pw - o.x).abs(), o.x - pw, o.x),
                                    ((cx_s - ocx).abs(), ocx - pw / 2.0, ocx),
                                ] {
                                    if d < wt {
                                        wt = d;
                                        jt = jx;
                                        gx = Some(gl);
                                    }
                                }
                                for (d, jy, gl) in [
                                    ((lt - o.y).abs(), o.y, o.y),
                                    ((lt + ph - bottom).abs(), bottom - ph, bottom),
                                    ((lt - bottom).abs(), bottom, bottom),
                                    ((lt + ph - o.y).abs(), o.y - ph, o.y),
                                    ((cy_s - ocy).abs(), ocy - ph / 2.0, ocy),
                                ] {
                                    if d < mtm {
                                        mtm = d;
                                        mt = jy;
                                        gy = Some(gl);
                                    }
                                }
                            }
                            if st.abs() < wt {
                                jt = 0.0;
                                gx = Some(0.0);
                            }
                            if lt.abs() < mtm {
                                mt = 0.0;
                                gy = Some(0.0);
                            }
                        }
                        this.snap_guides = (gx.is_some() || gy.is_some()).then_some((gx, gy));
                        if let Some(widget) = this.canvas.widgets.get_mut(&id) {
                            widget.x = jt;
                            widget.y = mt.max(min_y as f64);
                        }
                        this.drag = Some(Drag::Widget {
                            id,
                            grab,
                            press,
                            moved: true,
                        });
                        cx.notify();
                    }
                    Some(Drag::Resize {
                        id,
                        dir,
                        origin,
                        anchor,
                    }) => {
                        let id = id.clone();
                        let dir = *dir;
                        let (ox, oy, ow, oh) = *origin;
                        let world =
                            canvas::View::new(&this.canvas.camera, Pos2::ZERO).to_world(pointer);
                        let (dx, dy) = ((world.x - anchor.x) as f64, (world.y - anchor.y) as f64);
                        let min_y = canvas::title_bar_world_y(
                            this.canvas.camera.y,
                            this.canvas.camera.zoom,
                        );
                        if let Some(widget) = this.canvas.widgets.get_mut(&id) {
                            // Size first, position second — the ordering the
                            // Electron comments call out: deriving x/y from
                            // the unclamped size let the anchored edge keep
                            // travelling after a capped kind's size stopped
                            // growing, which slid the whole widget.
                            let (w, h) = canvas::clamp_widget_size(
                                widget.kind.as_deref(),
                                if dir.contains('e') {
                                    ow + dx
                                } else if dir.contains('w') {
                                    ow - dx
                                } else {
                                    ow
                                },
                                if dir.contains('s') {
                                    oh + dy
                                } else if dir.contains('n') {
                                    oh - dy
                                } else {
                                    oh
                                },
                            );
                            widget.w = w;
                            widget.h = h;
                            widget.x = if dir.contains('w') { ox + ow - w } else { ox };
                            widget.y = if dir.contains('n') { oy + oh - h } else { oy };
                            // The title band stays clear while resizing.
                            if widget.y < min_y {
                                let bottom = widget.y + widget.h;
                                widget.y = min_y;
                                widget.h = (bottom - min_y).max(canvas::MIN_WIDGET_H);
                            }
                        }
                        cx.notify();
                    }
                    Some(Drag::Stroke { start, moved }) => {
                        let start = *start;
                        let mut moved = *moved;
                        let delta = pointer - start;
                        if moved || delta.length() >= DRAW_CLICK_THRESHOLD {
                            let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                            if !moved {
                                // The stroke begins where the press landed.
                                moved = true;
                                this.pending_stroke.push(view_now.to_world(start));
                            }
                            if this.pending_stroke.len() < MAX_STROKE_POINTS {
                                this.pending_stroke.push(view_now.to_world(pointer));
                            }
                            this.drag = Some(Drag::Stroke { start, moved });
                            cx.notify();
                        }
                    }
                    Some(Drag::Erase { dirty }) => {
                        // trackDrag((ev) => canvas.eraseAt(...)) — every
                        // move cuts whatever the pointer is over.
                        let dirty = *dirty;
                        let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                        let world = view_now.to_world(pointer);
                        let dirty = this.erase_at(world) || dirty;
                        this.drag = Some(Drag::Erase { dirty });
                        cx.notify();
                    }
                    Some(Drag::Select { start, .. })
                        if (pointer - *start).length() >= DRAW_CLICK_THRESHOLD =>
                    {
                        // The marquee materializes past the same 4px click
                        // threshold strokes use.
                        this.drag = Some(Drag::Select {
                            start: *start,
                            rect: Some((*start, pointer)),
                        });
                        cx.notify();
                    }
                    Some(Drag::Select { .. }) | None => {}
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    if this.selection_drag.take().is_some() {
                        cx.notify();
                        return;
                    }
                    match this.drag.take() {
                        Some(Drag::Pan { .. }) => {
                            this.camera_dirty_at = None;
                            this.commit_camera();
                        }
                        Some(Drag::Widget { id, moved, .. }) => {
                            this.snap_guides = None;
                            // A sub-threshold press was a click — nothing to
                            // journal (the original only wrote real updates).
                            if moved {
                                if let Some(widget) = this.canvas.widgets.get(&id).cloned() {
                                    this.commit_widget(&widget);
                                }
                            }
                        }
                        Some(Drag::Resize { id, .. }) => {
                            if let Some(widget) = this.canvas.widgets.get(&id).cloned() {
                                this.commit_widget(&widget);
                            }
                        }
                        Some(Drag::Stroke { moved, .. }) => {
                            let points = std::mem::take(&mut this.pending_stroke);
                            // A click-sized gesture never began a stroke;
                            // under two points there is no line to keep.
                            if moved && points.len() >= 2 {
                                this.canvas.strokes.push(slate_app::projection::Stroke {
                                    id: format!("stroke-{}", uuid::Uuid::new_v4().simple()),
                                    color: STROKE_COLORS[this.stroke_color].to_owned(),
                                    points: points
                                        .into_iter()
                                        .map(|p| slate_app::projection::Point {
                                            x: p.x as f64,
                                            y: p.y as f64,
                                        })
                                        .collect(),
                                });
                                this.commit_strokes();
                            }
                        }
                        Some(Drag::Erase { dirty }) => {
                            // The whole gesture wrote one canvas.strokes
                            // event, like the original's rAF-batched flush.
                            if dirty {
                                this.commit_strokes();
                            }
                        }
                        Some(Drag::Select {
                            rect: Some((a, b)), ..
                        }) => {
                            // Marquee → world-space hit test: every widget
                            // the rect touches is a candidate, the topmost
                            // one wins the raise — bringToFront, nothing else.
                            let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                            let wa = view_now.to_world(a);
                            let wb = view_now.to_world(b);
                            let (min_x, max_x) = (wa.x.min(wb.x) as f64, wa.x.max(wb.x) as f64);
                            let (min_y, max_y) = (wa.y.min(wb.y) as f64, wa.y.max(wb.y) as f64);
                            let topmost = this
                                .canvas
                                .widgets
                                .values()
                                .filter(|w| {
                                    !(w.x + w.w < min_x
                                        || w.x > max_x
                                        || w.y + w.h < min_y
                                        || w.y > max_y)
                                })
                                .max_by(|x, y| {
                                    x.z.partial_cmp(&y.z).unwrap_or(std::cmp::Ordering::Equal)
                                })
                                .map(|w| w.id.clone());
                            if let Some(id) = topmost {
                                this.raise(&id);
                            }
                        }
                        Some(Drag::Select { .. }) | None => {}
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let pointer = pos(event.position);
                let zoom = this.canvas.camera.zoom as f32;
                let view_now = canvas::View::new(&this.canvas.camera, Pos2::ZERO);
                let size = window.viewport_size();
                let viewport = Vec2::new(f32::from(size.width), f32::from(size.height));
                // The original's wheel routing: a wheel event anywhere over a
                // widget — header included — is swallowed by it (terminals get
                // scrollback/mouse-report, panes scroll natively). On bare
                // canvas Ctrl/Cmd+wheel zooms at the pointer and a plain wheel
                // pans by deltaX+deltaY — the primary navigation gesture.
                let mut over_pane = false;
                let mut widgets: Vec<&Widget> = this.canvas.widgets.values().collect();
                widgets.sort_by(|a, b| b.maximized.cmp(&a.maximized).then(b.z.total_cmp(&a.z)));
                let mut scrolled = false;
                for widget in widgets {
                    let rect = widget_screen_rect(widget, &view_now, viewport);
                    let body_top = rect.min.y + theme::geometry::HEADER_HEIGHT * zoom;
                    let in_rect = pointer.x >= rect.min.x
                        && pointer.x <= rect.max.x
                        && pointer.y >= rect.min.y
                        && pointer.y <= rect.max.y;
                    let in_body = in_rect && pointer.y >= body_top;
                    if !is_terminal_kind(widget.kind.as_deref()) {
                        if in_rect {
                            over_pane = true;
                            break;
                        }
                        continue;
                    }
                    if in_rect && !in_body {
                        // Header only — the widget owns the event, nothing
                        // scrolls, the canvas stays put.
                        scrolled = true;
                        break;
                    }
                    if in_body {
                        let reporting = this
                            .snapshots
                            .iter()
                            .find(|s| s.id == widget.id)
                            .map(|s| s.mouse_reporting)
                            .unwrap_or(false);
                        if reporting {
                            // The program owns the wheel now — one button-64/
                            // 65 press per event (each notch is its own X11
                            // event), direction by sign, no release.
                            let (dy, dx) = match event.delta {
                                ScrollDelta::Pixels(p) => (f32::from(p.y), f32::from(p.x)),
                                ScrollDelta::Lines(p) => (p.y, p.x),
                            };
                            let mods = KeyMods {
                                shift: event.modifiers.shift,
                                alt: event.modifiers.alt,
                                ctrl: event.modifiers.control,
                                mac_cmd: event.modifiers.platform,
                            };
                            let (col, row) = terminal_cell(rect, zoom, pointer);
                            let kind = if dy != 0.0 {
                                Some(if dy < 0.0 {
                                    MouseKind::WheelDown
                                } else {
                                    MouseKind::WheelUp
                                })
                            } else if dx != 0.0 {
                                Some(if dx < 0.0 {
                                    MouseKind::WheelRight
                                } else {
                                    MouseKind::WheelLeft
                                })
                            } else {
                                None
                            };
                            if let Some(kind) = kind {
                                let _ = this.manager.mouse_input(&widget.id, kind, col, row, mods);
                            }
                        } else {
                            let pixels = match event.delta {
                                ScrollDelta::Pixels(p) => f32::from(p.y),
                                ScrollDelta::Lines(p) => p.y * 17.0 * zoom,
                            };
                            let _ = this.manager.scroll_pixels(
                                &widget.id,
                                pixels,
                                17.0 * zoom.max(0.2),
                            );
                        }
                        scrolled = true;
                        break;
                    }
                }
                if !scrolled && !over_pane {
                    let (dx, dy) = match event.delta {
                        ScrollDelta::Pixels(p) => (f32::from(p.x) as f64, f32::from(p.y) as f64),
                        ScrollDelta::Lines(p) => (p.x as f64 * 17.0, p.y as f64 * 17.0),
                    };
                    if event.modifiers.control || event.modifiers.platform {
                        // ctrlKey||metaKey → pointer-anchored zoom, factor
                        // exp(-deltaY*0.001) in the original.
                        let factor = (-dy * 0.001).exp().max(0.0) as f32;
                        zoom_at(&mut this.canvas.camera, Pos2::ZERO, pointer, factor);
                    } else {
                        // Plain wheel pans: camera -= delta, both axes.
                        this.canvas.camera.x -= dx;
                        this.canvas.camera.y -= dy;
                    }
                    this.camera_dirty_at = Some(Instant::now());
                }
                cx.notify();
            }));

        // The wallpaper — the shell's `.wallpaper-container`: a full-bleed
        // image under everything (settings `background` holds a data:image
        // URL), dimmed by `backgroundDim`'s rgba(8,8,8,d) overlay.
        if let Some(image) = self
            .settings
            .extra("background")
            .and_then(Value::as_str)
            .and_then(decode_data_image)
        {
            root = root.child(
                div().absolute().inset_0().child(
                    gpui::img(image)
                        .size_full()
                        .object_fit(gpui::ObjectFit::Cover),
                ),
            );
            let dim = (self.settings.background_dim.min(100) as f32 / 100.0 * 255.0) as u32;
            if dim > 0 {
                root = root.child(div().absolute().inset_0().bg(rgba(0x08080800 | dim)));
            }
        }

        // ConnectionsLayer: arcs between widget top-centers, painted under
        // the widgets like the SVG inside the world transform. A maximized
        // endpoint hides its arcs, same as the original.
        if !self.canvas.connections.is_empty() {
            let mut arcs: Vec<(Pos2, Pos2, Pos2)> = Vec::new();
            for connection in &self.canvas.connections {
                let (Some(from), Some(to)) = (
                    self.canvas.widgets.get(&connection.from),
                    self.canvas.widgets.get(&connection.to),
                ) else {
                    continue;
                };
                if from.id == to.id || from.maximized || to.maximized {
                    continue;
                }
                let a = view.to_screen(Pos2::new((from.x + from.w / 2.0) as f32, from.y as f32));
                let b = view.to_screen(Pos2::new((to.x + to.w / 2.0) as f32, to.y as f32));
                // arcPath: quadratic bezier, control point bowed upward by
                // clamp(24..120, dist*0.22) — the world-space distance, so
                // the bow scales with zoom like everything under transform.
                let dist = ((to.x - from.x).powi(2) + (to.y - from.y).powi(2)).sqrt() as f32;
                let bow = (dist * 0.22).clamp(24.0, 120.0) * zoom;
                arcs.push((a, Pos2::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0 - bow), b));
            }
            if !arcs.is_empty() {
                root = root.child(
                    gpui::canvas(
                        move |_, _, _| arcs,
                        |_bounds, arcs, window, _cx| {
                            // conn-glow under conn-thread — a wide soft pass
                            // under a hairline, standing in for the SVG's
                            // gaussian-blurred duplicate.
                            for (a, mid, b) in arcs {
                                for (width, color) in [(2.4f32, 0xdfe7ff80u32), (1.0, 0xdfe7ff57)] {
                                    let mut builder = PathBuilder::stroke(px(width));
                                    builder.move_to(point(px(a.x), px(a.y)));
                                    builder.curve_to(
                                        point(px(b.x), px(b.y)),
                                        point(px(mid.x), px(mid.y)),
                                    );
                                    if let Ok(path) = builder.build() {
                                        window.paint_path(path, rgba(color));
                                    }
                                }
                            }
                        },
                    )
                    .absolute()
                    .inset_0(),
                );
            }
        }

        // StrokesLayer: committed strokes plus the one in progress, as
        // 3px polylines in screen space — the original's `3/zoom` world
        // width is exactly that.
        {
            let mut stroke_paths: Vec<(u32, Vec<Pos2>)> = Vec::new();
            for stroke in &self.canvas.strokes {
                if stroke.points.len() < 2 {
                    continue;
                }
                stroke_paths.push((
                    stroke_color_u32(&stroke.color),
                    stroke
                        .points
                        .iter()
                        .map(|p| view.to_screen(Pos2::new(p.x as f32, p.y as f32)))
                        .collect(),
                ));
            }
            let pending: Vec<Pos2> = self
                .pending_stroke
                .iter()
                .map(|p| view.to_screen(*p))
                .collect();
            // The in-progress stroke previews in the selected swatch color,
            // slightly translucent like the original's overlay.
            let pending_color =
                stroke_color_u32(STROKE_COLORS[self.stroke_color]) & 0xffffff00 | 0xcc;
            if !stroke_paths.is_empty() || pending.len() >= 2 {
                root = root.child(
                    gpui::canvas(
                        move |_, _, _| (stroke_paths, pending),
                        move |_bounds, (strokes, pending), window, _cx| {
                            for (color, points) in strokes {
                                paint_polyline(window, &points, rgba(color));
                            }
                            paint_polyline(window, &pending, rgba(pending_color));
                        },
                    )
                    .absolute()
                    .inset_0(),
                );
            }
        }

        // Widgets paint in z order so the last drawn is the topmost — and a
        // maximized widget is the shell's zIndex-200 overlay, so it always
        // sorts after everything not maximized.
        let mut widgets: Vec<Widget> = self.canvas.widgets.values().cloned().collect();
        widgets.sort_by(|a, b| a.maximized.cmp(&b.maximized).then(a.z.total_cmp(&b.z)));
        for widget in widgets {
            let maximized = widget.maximized;
            // The overlay fills the viewport under the title band — the
            // original's world-unit style (-camera.x/zoom etc.) is this same
            // rect once the transform is factored out.
            let rect = widget_screen_rect(&widget, &view, viewport_size);
            if !maximized && !is_visible(rect, viewport_rect) {
                continue;
            }
            let id = widget.id.clone();
            let id_for_body = widget.id.clone();
            let id_for_close = widget.id.clone();
            let id_for_max = widget.id.clone();
            let id_for_title = widget.id.clone();
            let id_for_copy = widget.id.clone();
            let id_for_agent = widget.id.clone();
            let can_maximize = !canvas::non_maximizable(widget.kind.as_deref());
            let is_active = self.active.as_deref() == Some(widget.id.as_str());
            let is_terminal = is_terminal_kind(widget.kind.as_deref());
            let renaming_this = self.renaming.as_deref() == Some(widget.id.as_str());
            let name_copied = self.name_copied_at.as_ref().is_some_and(|(cid, at)| {
                cid == &widget.id && at.elapsed() < Duration::from_millis(1200)
            });
            let cam = camera.clone();
            let header_h = theme::geometry::HEADER_HEIGHT * zoom;

            let header = div()
                .id(("slate-widget-header", widget.z as u64))
                .h(px(header_h))
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .px(px(10.0 * zoom))
                .gap(px(8.0 * zoom))
                .bg(rgb(theme::hex(
                    if is_terminal_kind(widget.kind.as_deref()) {
                        theme::monochrome::TERMINAL_HEADER
                    } else {
                        theme::monochrome::ELEVATED
                    },
                )))
                .cursor_grab()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        // Clicking another widget commits an open rename —
                        // the shell's input blurred.
                        if this.renaming.is_some() {
                            this.commit_rename(cx);
                        }
                        // A maximized frame does not move — same early return
                        // as the shell's onHeaderPointerDown.
                        if !maximized {
                            let press = pos(event.position);
                            let world = canvas::View::new(&cam, Pos2::ZERO).to_world(press);
                            if let Some(widget) = this.canvas.widgets.get(&id) {
                                let grab =
                                    Pos2::new(world.x - widget.x as f32, world.y - widget.y as f32);
                                this.drag = Some(Drag::Widget {
                                    id: id.clone(),
                                    grab,
                                    press,
                                    moved: false,
                                });
                            }
                        }
                        this.raise(&id);
                        this.active = Some(id.clone());
                        // Pressing the frame gave it DOM focus in the shell —
                        // arrows now nudge this widget, not the camera.
                        this.frame_focus = Some(id.clone());
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .child(
                    // The shell's title: double-click turns it into a rename
                    // input. While renaming the buffer shows with a caret,
                    // brighter than the idle dim title.
                    div()
                        .id(("slate-widget-title", widget.z as u64))
                        .flex_1()
                        .flex()
                        .flex_row()
                        .items_center()
                        .min_w_0()
                        .text_size(px(12.0 * zoom))
                        .text_color(rgb(theme::hex(if renaming_this {
                            theme::text::NORMAL
                        } else {
                            theme::text::DIM
                        })))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                                if event.click_count >= 2 {
                                    // Second click belongs to the title, not
                                    // the header — no drag, edit the name.
                                    cx.stop_propagation();
                                    if this.renaming.as_deref() != Some(id_for_title.as_str()) {
                                        // An open rename on another widget
                                        // commits first — blur semantics.
                                        if this.renaming.is_some() {
                                            this.commit_rename(cx);
                                        }
                                        this.begin_rename(&id_for_title);
                                    }
                                    cx.notify();
                                }
                                // Single clicks bubble up to the header's
                                // drag/raise handler untouched.
                            }),
                        )
                        .child(if renaming_this {
                            self.rename_buffer.clone()
                        } else {
                            widget.title.clone()
                        })
                        .when(renaming_this, |el| {
                            el.child(
                                div()
                                    .text_color(rgb(theme::hex(theme::text::DIM)))
                                    .child("▌".to_owned()),
                            )
                        }),
                )
                .when(is_terminal, |header| {
                    // Copy terminal name — the shell's hover-revealed Copy
                    // button, always-on here (no group-hover trick). Shows
                    // the shell's Check briefly after a click.
                    header.child(
                        div()
                            .id(("slate-widget-copy", widget.z as u64))
                            .w(px(18.0 * zoom))
                            .h(px(18.0 * zoom))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.0 * zoom))
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .cursor_pointer()
                            .child(if name_copied { "✓" } else { "⧉" })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(widget) = this.canvas.widgets.get(&id_for_copy) {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            widget.title.clone(),
                                        ));
                                        this.name_copied_at =
                                            Some((id_for_copy.clone(), Instant::now()));
                                    }
                                    cx.notify();
                                }),
                            ),
                    )
                })
                .when(is_terminal, |header| {
                    // The shell's "Select Agent" pencil: opens the small
                    // picker parked under this header's right edge.
                    header.child(
                        div()
                            .id(("slate-widget-agent", widget.z as u64))
                            .w(px(18.0 * zoom))
                            .h(px(18.0 * zoom))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.0 * zoom))
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .cursor_pointer()
                            .child("✎".to_owned())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.agent_menu = if this.agent_menu.as_deref()
                                        == Some(id_for_agent.as_str())
                                    {
                                        None
                                    } else {
                                        Some(id_for_agent.clone())
                                    };
                                    cx.notify();
                                }),
                            ),
                    )
                })
                .child(
                    div()
                        .id(("slate-widget-max", widget.z as u64))
                        .w(px(18.0 * zoom))
                        .h(px(18.0 * zoom))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0 * zoom))
                        .text_color(rgb(theme::hex(if can_maximize {
                            theme::text::FAINT
                        } else {
                            // NON_MAXIMIZABLE kinds get the same button,
                            // drawn disabled — the shell's text-faint/40.
                            theme::hairline::FAINT
                        })))
                        .when(can_maximize, |el| el.cursor_pointer())
                        .when(!can_maximize, |el| el.cursor_not_allowed())
                        .child(if maximized { "↙" } else { "↗" })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                if can_maximize {
                                    this.toggle_maximize(&id_for_max, cx);
                                }
                            }),
                        ),
                )
                .child(
                    div()
                        .id(("slate-widget-close", widget.z as u64))
                        .w(px(18.0 * zoom))
                        .h(px(18.0 * zoom))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(13.0 * zoom))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .cursor_pointer()
                        .child("×".to_owned())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                // The shell's onClose ran the same confirm
                                // for terminals that frame-Delete does.
                                if is_terminal_kind(this.widget_kind(&id_for_close).as_deref()) {
                                    this.pending_close = Some(id_for_close.clone());
                                    cx.notify();
                                } else {
                                    this.close_widget(&id_for_close, cx);
                                }
                            }),
                        ),
                );

            let id_body_m = id_for_body.clone();
            let id_body_r = id_for_body.clone();
            let id_body_ul = id_for_body.clone();
            let id_body_um = id_for_body.clone();
            let id_body_ur = id_for_body.clone();
            let id_body_mv = id_for_body.clone();
            let body = div()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .bg(rgb(theme::hex(theme::monochrome::BASE)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        // Same blur-commit as the canvas press.
                        if this.renaming.is_some() {
                            this.commit_rename(cx);
                        }
                        this.active = Some(id_for_body.clone());
                        // Body press: a terminal's xterm textarea retakes key
                        // focus (frame keys stop reaching it); any other pane
                        // keeps its frame focused — it has no textarea to win.
                        this.frame_focus = (!is_terminal).then(|| id_for_body.clone());
                        // The frame's pointer-down brought the widget to the
                        // front regardless of where inside it landed.
                        this.raise(&id_for_body);
                        if is_terminal {
                            let pointer = pos(event.position);
                            let reporting = this
                                .snapshots
                                .iter()
                                .find(|s| s.id == id_for_body)
                                .map(|s| s.mouse_reporting)
                                .unwrap_or(false);
                            // xterm: a left press clears the selection; with
                            // mouse reporting armed it reports to the program,
                            // and Shift bypasses reporting to select.
                            this.manager.selection_clear(&id_for_body);
                            if reporting && !event.modifiers.shift {
                                this.terminal_mouse(
                                    &id_for_body,
                                    MouseKind::Press(TermMouseButton::Primary),
                                    rect,
                                    zoom,
                                    pointer,
                                    event.modifiers,
                                );
                            } else {
                                let (col, row) = terminal_cell(rect, zoom, pointer);
                                let _ = this.manager.selection_begin(&id_for_body, col, row);
                                this.selection_drag = Some(id_for_body.clone());
                                this.refresh_selection(&id_for_body);
                            }
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        if is_terminal {
                            this.active = Some(id_body_m.clone());
                            this.terminal_mouse(
                                &id_body_m,
                                MouseKind::Press(TermMouseButton::Middle),
                                rect,
                                zoom,
                                pos(event.position),
                                event.modifiers,
                            );
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        if is_terminal {
                            this.active = Some(id_body_r.clone());
                            this.terminal_mouse(
                                &id_body_r,
                                MouseKind::Press(TermMouseButton::Secondary),
                                rect,
                                zoom,
                                pos(event.position),
                                event.modifiers,
                            );
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }),
                )
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseUpEvent, _window, _cx| {
                        if is_terminal && this.selection_drag.is_none() {
                            this.terminal_mouse(
                                &id_body_ul,
                                MouseKind::Release(TermMouseButton::Primary),
                                rect,
                                zoom,
                                pos(event.position),
                                event.modifiers,
                            );
                        }
                    }),
                )
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(move |this, event: &MouseUpEvent, _window, _cx| {
                        if is_terminal {
                            this.terminal_mouse(
                                &id_body_um,
                                MouseKind::Release(TermMouseButton::Middle),
                                rect,
                                zoom,
                                pos(event.position),
                                event.modifiers,
                            );
                        }
                    }),
                )
                .on_mouse_up(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseUpEvent, _window, _cx| {
                        if is_terminal {
                            this.terminal_mouse(
                                &id_body_ur,
                                MouseKind::Release(TermMouseButton::Secondary),
                                rect,
                                zoom,
                                pos(event.position),
                                event.modifiers,
                            );
                        }
                    }),
                )
                // Drag motion for mode 1002; the screen turns a bare move
                // into the held button's drag report itself.
                .on_mouse_move(
                    cx.listener(move |this, event: &MouseMoveEvent, _window, _cx| {
                        if is_terminal
                            && this.selection_drag.as_deref() != Some(id_body_mv.as_str())
                        {
                            let button = event.pressed_button.map(|b| match b {
                                MouseButton::Middle => TermMouseButton::Middle,
                                MouseButton::Right => TermMouseButton::Secondary,
                                _ => TermMouseButton::Primary,
                            });
                            this.terminal_mouse(
                                &id_body_mv,
                                MouseKind::Move(button),
                                rect,
                                zoom,
                                pos(event.position),
                                event.modifiers,
                            );
                        }
                    }),
                )
                .child({
                    let body = widget_body(
                        &widget,
                        &self.snapshots,
                        zoom,
                        self.prefs.terminals_flipped,
                        cx,
                    );
                    // Non-terminal panes scroll their content natively —
                    // the shell's `overflow-auto` lists. Terminals keep their
                    // own scrollback wheel path instead.
                    if is_terminal_kind(widget.kind.as_deref()) {
                        body
                    } else {
                        div()
                            .id(format!("slate-pane-scroll-{}", widget.id))
                            .size_full()
                            .overflow_y_scroll()
                            .child(body)
                            .into_any_element()
                    }
                });

            let mut frame = div()
                .absolute()
                .left(px(rect.min.x))
                .top(px(rect.min.y))
                .w(px(rect.width()))
                .h(px(rect.height()))
                .flex()
                .flex_col()
                .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                .border_1()
                .border_color(rgb(theme::hex(if is_active {
                    theme::text::FAINT
                } else {
                    theme::hairline::SOFT
                })))
                .child(header)
                .child(body);

            // The shell's startupNotice banner — pinned to the widget's top
            // edge below the header, dismissable, until a fresh launch or a
            // close clears it. Frame coordinates are screen-space.
            if let Some(message) = self.startup_notice.get(&widget.id) {
                let id_for_notice = widget.id.clone();
                frame = frame.child(
                    div()
                        .id(format!("slate-startup-notice-{}", widget.id))
                        .absolute()
                        .left(px(6.0))
                        .right(px(6.0))
                        .top(px(header_h + 6.0))
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(8.0 * zoom))
                        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                        .px(px(10.0 * zoom))
                        .py(px(8.0 * zoom))
                        .text_size(px(11.0 * zoom))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(div().flex_1().min_w_0().child(message.clone()))
                        .child(
                            div()
                                .id(format!("slate-startup-dismiss-{}", widget.id))
                                .flex_none()
                                .cursor_pointer()
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .child("×".to_owned())
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.startup_notice.remove(&id_for_notice);
                                        cx.notify();
                                    }),
                                ),
                        ),
                );
            }

            // The eight resize handles the Electron frame drew — thin edges,
            // small squares at the corners, all in unscaled screen px. A
            // maximized frame has none (`!widget.maximized && RESIZE_HANDLES`).
            if !maximized {
                for dir in ["n", "s", "e", "w", "ne", "nw", "se", "sw"] {
                    let rid = widget.id.clone();
                    let rcam = camera.clone();
                    let mut handle = div()
                        .id(format!("slate-resize-{}-{dir}", widget.id))
                        .absolute()
                        .cursor(resize_cursor(dir))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                window.focus(&this.focus, cx);
                                let world = canvas::View::new(&rcam, Pos2::ZERO)
                                    .to_world(pos(event.position));
                                if let Some(w) = this.canvas.widgets.get(&rid) {
                                    this.drag = Some(Drag::Resize {
                                        id: rid.clone(),
                                        dir,
                                        origin: (w.x, w.y, w.w, w.h),
                                        anchor: world,
                                    });
                                }
                                this.raise(&rid);
                                this.active = Some(rid.clone());
                                this.frame_focus = Some(rid.clone());
                                cx.notify();
                            }),
                        );
                    let edge = px(8.0);
                    let corner = px(14.0);
                    let inset = px(12.0);
                    handle = match dir {
                        "n" => handle.top(px(0.0)).left(inset).right(inset).h(edge),
                        "s" => handle.bottom(px(0.0)).left(inset).right(inset).h(edge),
                        "e" => handle.right(px(0.0)).top(inset).bottom(inset).w(edge),
                        "w" => handle.left(px(0.0)).top(inset).bottom(inset).w(edge),
                        "ne" => handle.top(px(0.0)).right(px(0.0)).w(corner).h(corner),
                        "nw" => handle.top(px(0.0)).left(px(0.0)).w(corner).h(corner),
                        "se" => handle.bottom(px(0.0)).right(px(0.0)).w(corner).h(corner),
                        _ => handle.bottom(px(0.0)).left(px(0.0)).w(corner).h(corner),
                    };
                    frame = frame.child(handle);
                }
            }

            root = root.child(frame);
        }

        // The agent picker — WidgetFrame's "Select Agent" dropdown, parked
        // under the header's right edge of the terminal that opened it.
        // Picking one types its launch command into the PTY with Enter, the
        // shell's terminal.write(command + '\r').
        if let Some(menu_id) = self.agent_menu.clone() {
            if let Some(widget) = self.canvas.widgets.get(&menu_id) {
                let rect = widget_screen_rect(widget, &view, viewport_size);
                let menu_w = 170.0_f32;
                let menu_x = (rect.max.x - menu_w).max(4.0);
                let menu_y = rect.min.y + theme::geometry::HEADER_HEIGHT * zoom + 2.0;
                let mut menu = div()
                    .id("slate-agent-menu")
                    .absolute()
                    .left(px(menu_x))
                    .top(px(menu_y))
                    .w(px(menu_w))
                    .flex()
                    .flex_col()
                    .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .border_1()
                    .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                    .py_1();
                for (index, (agent_id, label, command)) in AGENT_LAUNCHERS.iter().enumerate() {
                    let menu_id = menu_id.clone();
                    let picked = self.agent_picks.select.get(&menu_id) == Some(&index);
                    menu = menu.child(
                        div()
                            .id(format!("slate-agent-{label}"))
                            .w_full()
                            .px_3()
                            .py(px(7.0))
                            .text_xs()
                            .text_color(rgb(theme::hex(if picked {
                                theme::text::NORMAL
                            } else {
                                theme::text::DIM
                            })))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                            .child(*label)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.agent_menu = None;
                                    // `Fd` + `Wd`: persist the row and the
                                    // launched id, then type the command.
                                    this.agent_picks.pick(&menu_id, index, agent_id);
                                    if let Err(error) = this.agent_picks.save() {
                                        this.notice(format!("agent pick: {error}"));
                                    }
                                    match this.manager.write_text(&menu_id, command, true) {
                                        // The shell armed its startup probe
                                        // the moment the agent command was
                                        // typed — same here.
                                        Ok(_) => this.arm_startup_watch(&menu_id, command),
                                        Err(error) => {
                                            this.notice(format!("agent launch: {error}"))
                                        }
                                    }
                                    cx.notify();
                                }),
                            ),
                    );
                }
                // "Plain shell (stop auto-launch)" — the shell's trailing
                // separator row: clears every per-terminal agent key (`vn`).
                {
                    let menu_id = menu_id.clone();
                    menu = menu.child(
                        div()
                            .id("slate-agent-plain")
                            .w_full()
                            .px_3()
                            .py(px(7.0))
                            .mt(px(2.0))
                            .border_t_1()
                            .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                            .text_xs()
                            .text_color(rgb(theme::hex(theme::text::DIM)))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                            .child("Plain shell (stop auto-launch)")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.agent_menu = None;
                                    this.agent_picks.clear(&menu_id);
                                    if let Err(error) = this.agent_picks.save() {
                                        this.notice(format!("agent pick: {error}"));
                                    }
                                    cx.notify();
                                }),
                            ),
                    );
                }
                root = root.child(menu);
            } else {
                // The widget went away under an open menu.
                self.agent_menu = None;
            }
        }

        // Right-click spawn menu — the Electron app's ContextMenu.
        if let Some(at) = self.menu {
            let mut menu = div()
                .absolute()
                .left(px(at.x.max(4.0).min(viewport_size.x - 270.0)))
                .top(px(at.y.max(4.0).min(viewport_size.y - 160.0)))
                .w(px(260.0))
                .flex()
                .flex_col()
                .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .py_1();
            let items = self.menu_items();
            if items.is_empty() {
                // The shell's empty-favorites hint.
                menu = menu.child(
                    div()
                        .w_full()
                        .px_3()
                        .py(px(7.0))
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child("No favorite widgets — pick favorites in Settings"),
                );
            }
            for (index, &(kind, label, hint)) in items.iter().enumerate() {
                let selected = index == self.menu_sel;
                menu = menu.child(
                    div()
                        .id(format!("slate-menu-{kind}"))
                        .w_full()
                        .px_3()
                        .py(px(6.0))
                        .flex()
                        .flex_col()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .cursor_pointer()
                        .when(selected, |el| {
                            el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                        })
                        .hover(|s| s.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                        .child(label)
                        .child(
                            div()
                                .text_size(px(9.0))
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .child(hint),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, _w, cx| {
                                cx.stop_propagation();
                                this.menu = None;
                                this.spawn_menu_widget(kind, at, cx);
                            }),
                        ),
                );
            }
            root = root.child(menu);
        }

        // The empty-canvas card — drawn only when nothing at all is on the
        // canvas, strokes included, like the original's guard. "Add
        // terminal" is the same addWidget path the button called.
        if self.canvas.widgets.is_empty() && self.canvas.strokes.is_empty() {
            let viewport = self.viewport;
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .px_6()
                            .py_4()
                            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                            .border_1()
                            .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                            .rounded_md()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                    .child("Your canvas is clear".to_owned()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                                    .child(
                                        "Use the command bar below: /terminal, .files, @planner, or plain terminal"
                                            .to_owned(),
                                    ),
                            )
                            .child(
                                div()
                                    .id("slate-empty-add-terminal")
                                    .px_3()
                                    .py_1()
                                    .bg(rgb(theme::hex(theme::status::INFO)))
                                    .text_size(px(11.0))
                                    .text_color(rgb(theme::hex(theme::monochrome::BASE)))
                                    .rounded_md()
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, _w, cx| {
                                            this.spawn_terminal(viewport, cx);
                                            cx.stop_propagation();
                                        }),
                                    )
                                    .child("Add terminal".to_owned()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::hex(theme::hairline::FAINT)))
                                    .child(
                                        "T N terminal · drag pan · scroll zoom · + - 0 · F fit · Home reset · d draw · e erase"
                                            .to_owned(),
                                    ),
                            ),
                    ),
            );
        }

        // The bottom toolbar — the Electron `Fu` strip: tool buttons, the
        // command input with suggestions, the overflow cluster (workspace
        // chip, mode/target, zoom, clear-strokes, settings). `command_bar`
        // = Some(buffer) only while it owns the keyboard.
        {
            let buffer = self.command_bar.clone().unwrap_or_default();
            let open = self.command_bar.is_some();
            let message = self.command_message;
            let names = self.manager.names();
            let target_name = self
                .command_target
                .as_deref()
                .map(|id| names.get(id).cloned().unwrap_or_else(|| id.to_owned()));
            let terminals: Vec<(String, String)> = self
                .snapshots
                .iter()
                .map(|snap| {
                    (
                        snap.id.clone(),
                        names
                            .get(&snap.id)
                            .cloned()
                            .unwrap_or_else(|| snap.id.clone()),
                    )
                })
                .collect();
            let tool = if self.draw_mode {
                "draw"
            } else if self.erase_mode {
                "erase"
            } else if self.select_mode {
                "select"
            } else {
                // No tool picked = the original's Pan state.
                "pan"
            };
            let workspace_name = slate_app::workspace::current()
                .and_then(|dir| {
                    std::path::Path::new(&dir)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "No folder".to_owned());
            let state = crate::views_toolbar::ToolbarState {
                buffer: buffer.clone(),
                open,
                message_mode: message,
                target_name,
                terminals,
                tool: tool.to_owned(),
                stroke_color: self.stroke_color,
                can_undo: !self.undo_stack.is_empty(),
                can_redo: !self.redo_stack.is_empty(),
                zoom_percent: (self.canvas.camera.zoom * 100.0).round().max(0.0) as u32,
                workspace_name,
                suggestions: if message {
                    Vec::new()
                } else {
                    toolbar_suggestions(&buffer, self.settings.command_prefix())
                },
                suggestion_sel: self.command_suggestion,
                enabled: true,
            };
            root = root.child(
                div()
                    .id("slate-toolbar-row")
                    .absolute()
                    .bottom(px(34.0))
                    .left_0()
                    .right_0()
                    .flex()
                    .justify_center()
                    // A click on the row focuses the canvas input like the
                    // old bar's body did — the strip's own buttons stop
                    // propagation before this sees them.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, window, cx| {
                            cx.stop_propagation();
                            window.focus(&this.focus, cx);
                            this.active = None;
                            this.frame_focus = None;
                            cx.notify();
                        }),
                    )
                    .child(crate::views_toolbar::toolbar_pane(&state, cx)),
            );
        }

        // The close-terminal confirm — the shell's `confirm("Close terminal?
        // The running process will be terminated.", {danger, title: "Close
        // Terminal", confirmLabel: "Close"})`, a centered modal over a dim
        // backdrop that eats the press.
        if let Some(pending) = self.pending_close.clone() {
            let title = self
                .canvas
                .widgets
                .get(&pending)
                .map(|w| w.title.clone())
                .unwrap_or_else(|| pending.clone());
            root = root.child(
                div()
                    .id("slate-close-confirm")
                    .absolute()
                    .top(px(0.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba(0x00000055))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| {
                            // Backdrop clicks cancel — same as Escape.
                            this.pending_close = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .id("slate-close-confirm-box")
                            .w(px(360.0))
                            .flex()
                            .flex_col()
                            .gap_2()
                            .p_4()
                            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                            .border_1()
                            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|_this, _event, _window, cx| {
                                    cx.stop_propagation();
                                }),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                    .child("Close Terminal".to_owned()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::hex(theme::text::DIM)))
                                    .child(format!(
                                        "Close terminal \"{title}\"? The running process will be terminated."
                                    )),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .justify_end()
                                    .gap_2()
                                    .child(
                                        div()
                                            .id("slate-close-cancel")
                                            .px_3()
                                            .py_1()
                                            .text_xs()
                                            .cursor_pointer()
                                            .border_1()
                                            .border_color(rgb(theme::hex(
                                                theme::hairline::FAINT,
                                            )))
                                            .text_color(rgb(theme::hex(
                                                theme::text::DIM,
                                            )))
                                            .child("Cancel".to_owned())
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this, _event, _window, cx| {
                                                        this.pending_close = None;
                                                        cx.stop_propagation();
                                                        cx.notify();
                                                    },
                                                ),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("slate-close-danger")
                                            .px_3()
                                            .py_1()
                                            .text_xs()
                                            .cursor_pointer()
                                            .bg(rgb(theme::hex(theme::status::DANGER)))
                                            .text_color(rgb(0xffffff))
                                            .child("Close".to_owned())
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(
                                                    |this, _event, _window, cx| {
                                                        if let Some(id) =
                                                            this.pending_close.take()
                                                        {
                                                            this.close_widget(&id, cx);
                                                        }
                                                        cx.stop_propagation();
                                                        cx.notify();
                                                    },
                                                ),
                                            ),
                                    ),
                            ),
                    ),
            );
        }

        // The select-tool marquee — a screen-space rect for as long as the
        // drag is live (the shell drew the same overlay in pointer coords).
        if let Some(Drag::Select {
            rect: Some((a, b)), ..
        }) = &self.drag
        {
            let (x, y) = (a.x.min(b.x), a.y.min(b.y));
            let (w, h) = ((a.x - b.x).abs(), (a.y - b.y).abs());
            root = root.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .h(px(h))
                    .border_1()
                    .border_color(rgb(theme::hex(theme::status::INFO)))
                    .bg(rgba(0x3b82f618)),
            );
        }

        // The toast — top-center, brief, like the shell's notice state.
        if let Some((text, _)) = self
            .notice
            .as_ref()
            .filter(|(_, expiry)| *expiry > Instant::now())
        {
            root = root.child(
                div()
                    .absolute()
                    .top(px(48.0))
                    .left(px(0.0))
                    .right(px(0.0))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                            .border_1()
                            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                            .text_size(px(11.0))
                            .text_color(rgb(theme::hex(theme::status::WARN)))
                            .child(text.clone()),
                    ),
            );
        }

        // Snap guides — the shell's accent lines through the aligned edge
        // while a widget drag is inside the 8px threshold.
        if let Some((gx, gy)) = self.snap_guides {
            let accent = rgb(theme::hex(theme::status::INFO));
            if let Some(wx) = gx {
                let sx = view.to_screen(Pos2::new(wx as f32, 0.0)).x;
                root = root.child(
                    div()
                        .absolute()
                        .left(px(sx))
                        .top(px(0.0))
                        .bottom(px(0.0))
                        .w(px(1.0))
                        .bg(accent),
                );
            }
            if let Some(wy) = gy {
                let sy = view.to_screen(Pos2::new(0.0, wy as f32)).y;
                root = root.child(
                    div()
                        .absolute()
                        .top(px(sy))
                        .left(px(0.0))
                        .right(px(0.0))
                        .h(px(1.0))
                        .bg(accent),
                );
            }
        }

        // StatusBar: the 22px bottom strip that was never a widget — right
        // aligned CPU%, zoom, widget count and the local clock.
        let widget_count = self.canvas.widgets.len();
        let zoom_label = canvas::zoom_label(&view);
        let (hh, mm, ss) = clock_hms();
        root = root.child(
            div()
                .absolute()
                .bottom(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .h(px(22.0))
                .flex()
                .flex_row()
                .items_center()
                .justify_end()
                .gap(px(12.0))
                .px_3()
                .bg(rgb(theme::hex(theme::monochrome::BASE)))
                .border_t_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .text_size(px(10.0))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_this, _event, _window, cx| {
                        cx.stop_propagation();
                    }),
                )
                // The TitleBar arrange menu — a chip that opens the four
                // modes over the status bar.
                .child(
                    div()
                        .id("slate-arrange-chip")
                        .px_2()
                        .py(px(2.0))
                        .cursor_pointer()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .hover(|s| s.text_color(rgb(theme::hex(theme::text::NORMAL))))
                        .child(format!(
                            "arrange: {}",
                            self.prefs.arrange_mode.label().to_lowercase()
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.arrange_menu = !this.arrange_menu;
                                cx.notify();
                            }),
                        ),
                )
                .when_some(self.cpu_percent, |bar, pct| {
                    bar.child(
                        div()
                            .text_color(rgb(theme::hex(if pct > 80 {
                                theme::status::DANGER
                            } else if pct > 50 {
                                theme::status::WARN
                            } else {
                                theme::status::INFO
                            })))
                            .child(format!("{pct}%")),
                    )
                })
                // The active tool, where the shell's Toolbar showed it —
                // "draw" carries the current swatch so the picked color is
                // visible without a palette row.
                .when(self.draw_mode, |bar| {
                    bar.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(8.0))
                                    .h(px(8.0))
                                    .bg(rgba(stroke_color_u32(STROKE_COLORS[self.stroke_color]))),
                            )
                            .text_color(rgba(stroke_color_u32(STROKE_COLORS[self.stroke_color])))
                            .child("draw".to_owned()),
                    )
                })
                .when(self.erase_mode, |bar| {
                    bar.child(
                        div()
                            .text_color(rgb(theme::hex(theme::status::WARN)))
                            .child("erase".to_owned()),
                    )
                })
                .when(self.select_mode, |bar| {
                    bar.child(
                        div()
                            .text_color(rgb(theme::hex(theme::status::INFO)))
                            .child("select".to_owned()),
                    )
                })
                .child(
                    div()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!(
                            "{zoom_label} · {widget_count} widgets · {hh:02}:{mm:02}:{ss:02}"
                        )),
                ),
        );

        // The arrange dropdown — four modes with the shell's hints, parked
        // just above the status bar's right edge like the TitleBar menu.
        if self.arrange_menu {
            let mut popup = div()
                .id("slate-arrange-menu")
                .absolute()
                .bottom(px(24.0))
                .right(px(4.0))
                .w(px(240.0))
                .flex()
                .flex_col()
                .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .py_1();
            for mode in slate_app::arrange::ArrangeMode::ALL {
                let current = mode == self.prefs.arrange_mode;
                popup = popup.child(
                    div()
                        .id(format!("slate-arrange-{}", mode.label().to_lowercase()))
                        .w_full()
                        .px_3()
                        .py(px(6.0))
                        .flex()
                        .flex_col()
                        .cursor_pointer()
                        .text_color(rgb(theme::hex(if current {
                            theme::text::NORMAL
                        } else {
                            theme::text::DIM
                        })))
                        .hover(|s| s.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                        .child(mode.label())
                        .child(
                            div()
                                .text_size(px(9.0))
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .child(mode.hint()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _e, _w, cx| {
                                cx.stop_propagation();
                                this.arrange_menu = false;
                                this.apply_arrange(mode, cx);
                            }),
                        ),
                );
            }
            root = root.child(popup);
        }

        // The left rail — the shell's `rail-shell` adapted to canvas mode:
        // collapsed it is a 56px icon strip, expanded a 200px panel with
        // the workspace, open widgets and the planner preview. It floats
        // over the canvas between the title bar and the status bar.
        {
            let (rows, _) = crate::views_planner::planner_items();
            let planner_items = json!({
                "items": rows
                    .iter()
                    .map(|row| json!({
                        "id": row.id,
                        "title": row.title,
                        "done": row.done,
                        "day": row.day,
                        "order": row.order,
                    }))
                    .collect::<Vec<_>>()
            });
            root = root.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(canvas::TITLE_BAR_HEIGHT))
                    .bottom(px(24.0))
                    .child(crate::views_sidebar::sidebar_pane(
                        &self.canvas,
                        &planner_items,
                        self.prefs.sidebar_open,
                        cx,
                    )),
            );
        }

        // The TitleBar — the shell's fixed 40px top strip (history
        // arrows, view tabs, flip, git chip, window controls), floating
        // over the canvas exactly like the Electron layer order: canvas →
        // chrome.
        if let Some(dir) = slate_app::workspace::current() {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(canvas::TITLE_BAR_HEIGHT))
                    .child(crate::views_titlebar::titlebar_pane(
                        &crate::views_titlebar::TitlebarState {
                            workspace_dir: std::path::Path::new(&dir),
                            presence_count: self.snapshots.len(),
                            can_undo: !self.undo_stack.is_empty(),
                            can_redo: !self.redo_stack.is_empty(),
                            terminals_flipped: self.prefs.terminals_flipped,
                            git: &self.git_chip,
                        },
                        cx,
                    )),
            );
        }

        // The settings modal — `orcspace:open-settings` from the gear.
        if self.settings_open {
            root = root.child(crate::views_settings::settings_modal(&self.settings, cx));
        }
        root
    }
}

impl CanvasView {
    fn on_key(&mut self, event: &KeyDownEvent, viewport: Vec2, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;

        // Canvas history — the shell's keydown listener fired globally, so
        // ^Z/^⇧Z/^Y is undo/redo even over a focused terminal (the PTY never
        // saw ^Z as suspend; the TitleBar buttons dispatched the same events).
        if (mods.control || mods.platform) && !mods.alt {
            match keystroke.key.as_str() {
                "z" if !self.undo_stack.is_empty() || mods.shift => {
                    self.undo(mods.shift);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                "y" if !self.redo_stack.is_empty() => {
                    self.undo(true);
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                _ => {}
            }
        }

        // The context menu's keyboard path — the shell's ContextMenu:
        // arrows move the cursor, Enter spawns, Escape closes. The agent
        // launcher keeps Escape only.
        if self.menu.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.menu = None;
                    self.agent_menu = None;
                }
                "up" | "down" => {
                    let len = self.menu_items().len().max(1);
                    self.menu_sel = if keystroke.key.as_str() == "up" {
                        (self.menu_sel + len - 1) % len
                    } else {
                        (self.menu_sel + 1) % len
                    };
                }
                "home" => {
                    self.menu_sel = 0;
                }
                "end" => {
                    self.menu_sel = self.menu_items().len().saturating_sub(1);
                }
                "enter" => {
                    let items = self.menu_items();
                    if let Some(&(kind, _, _)) =
                        items.get(self.menu_sel.min(items.len().saturating_sub(1)))
                    {
                        let at = self.menu.take().unwrap();
                        self.spawn_menu_widget(kind, at, cx);
                    }
                }
                _ => {}
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        if (self.agent_menu.is_some() || self.arrange_menu) && keystroke.key.as_str() == "escape" {
            self.agent_menu = None;
            self.arrange_menu = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // The settings modal is modal too — Escape closes it, everything
        // else is swallowed while it is up.
        if self.settings_open {
            if keystroke.key.as_str() == "escape" {
                self.settings_open = false;
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // The close-confirm dialog is modal while it is up — the shell's
        // `confirm()`: Enter is the danger button, Escape cancels, and no
        // other key reaches the canvas.
        if self.pending_close.is_some() {
            match keystroke.key.as_str() {
                "enter" => {
                    let id = self.pending_close.take().unwrap();
                    self.close_widget(&id, cx);
                }
                "escape" => {
                    self.pending_close = None;
                }
                _ => {}
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // The command bar owns the keyboard while it is open: Escape closes,
        // Enter runs, Backspace edits, and anything printable appends. This
        // sits before the terminal branch on purpose — a bar that is open
        // keeps its keys even over a focused widget.
        if self.command_bar.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.command_bar = None;
                }
                "tab" => {
                    // Tab adopts the highlighted suggestion while the
                    // listbox is up (the input's `chooseSuggestion`); only
                    // with nothing suggested does it toggle the mode.
                    if !self.command_suggestions().is_empty() {
                        self.adopt_suggestion();
                    } else {
                        self.command_message = !self.command_message;
                        if self.command_message && self.command_target.is_none() {
                            self.command_target = self.command_target_terminal();
                        }
                    }
                }
                "up" | "down" if self.command_message => {
                    // The Terminal <select>: ↑/↓ moves through the roster.
                    let ids: Vec<String> = self.snapshots.iter().map(|s| s.id.clone()).collect();
                    if !ids.is_empty() {
                        let at = self
                            .command_target
                            .as_deref()
                            .and_then(|id| ids.iter().position(|x| x == id))
                            .unwrap_or(0);
                        let next = if keystroke.key.as_str() == "up" {
                            (at + ids.len() - 1) % ids.len()
                        } else {
                            (at + 1) % ids.len()
                        };
                        self.command_target = Some(ids[next].clone());
                    }
                }
                "up" | "down" => {
                    // The listbox's suggestionIndex — same modulo walk.
                    let len = self.command_suggestions().len();
                    if len > 0 {
                        let at = self.command_suggestion.min(len - 1);
                        self.command_suggestion = if keystroke.key.as_str() == "up" {
                            (at + len - 1) % len
                        } else {
                            (at + 1) % len
                        };
                    }
                }
                "enter" => {
                    // A highlighted suggestion whose name isn't yet the
                    // typed token is adopted, not run — `submitCommand`'s
                    // `typedName !== picked` branch.
                    let typed = self
                        .command_bar
                        .as_deref()
                        .and_then(|b| {
                            b.trim_start()
                                .split_whitespace()
                                .next()
                                .map(|t| t.trim_start_matches(['/', '.', '@']).to_lowercase())
                        })
                        .unwrap_or_default();
                    let suggestions = self.command_suggestions();
                    let picked = suggestions
                        .get(self.command_suggestion.min(suggestions.len().saturating_sub(1)))
                        .map(|(kind, _, _)| kind.as_str());
                    if picked.is_some_and(|kind| kind != typed) {
                        self.adopt_suggestion();
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                    let buffer = self.command_bar.take().unwrap_or_default();
                    // `if (!value) return` — an empty submit kept the input
                    // and its text; reopen with the buffer untouched.
                    if buffer.trim().is_empty() {
                        self.command_bar = Some(buffer);
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                    let target = self.command_target_terminal();
                    if self.command_message {
                        // Message mode writes the text verbatim — no
                        // trailing `\r` (`mode === 'message' ? command : …`).
                        if let Some(target) = target.as_deref() {
                            let _ = self.manager.write_text(target, &buffer, false);
                        }
                        self.command_message = false;
                    } else if let Some((kind, initial)) =
                        parse_widget_invocation(&buffer, self.settings.command_prefix())
                    {
                        self.create_widget_from_command(kind, &initial, viewport, cx);
                    } else if let Some(target) = target.as_deref() {
                        // A non-invocation line in command mode is terminal
                        // input — the original's `${command}\r` write.
                        let _ = self.manager.write_text(target, &buffer, true);
                    } else {
                        // Nothing to receive it — a miss keeps the bar open
                        // so a typo can be fixed, like the disabled submit.
                        self.command_bar = Some(buffer);
                    }
                }
                "backspace" => {
                    if let Some(buffer) = self.command_bar.as_mut() {
                        buffer.pop();
                    }
                    // A new query resets the highlight — the shell's
                    // `setSuggestionIndex(0)` on suggestionQuery change.
                    self.command_suggestion = 0;
                }
                _ => {
                    if !mods.control && !mods.alt && !mods.platform {
                        // key_char can be None for synthetic key events (no
                        // IME round-trip) — the keysym is the fallback so the
                        // bar still types.
                        let text =
                            keystroke
                                .key_char
                                .clone()
                                .or_else(|| match keystroke.key.as_str() {
                                    "space" => Some(" ".to_owned()),
                                    k if k.chars().count() == 1 => Some(k.to_owned()),
                                    _ => None,
                                });
                        if let (Some(text), Some(buffer)) = (text, self.command_bar.as_mut()) {
                            buffer.push_str(&text);
                        }
                        self.command_suggestion = 0;
                    }
                }
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // The header's rename input owns the keyboard while it is open —
        // same shape as the command bar: Enter commits, Escape cancels,
        // Backspace edits, printable keys append. Sits above the terminal
        // branch so renaming a terminal never types into the PTY.
        if self.renaming.is_some() {
            match keystroke.key.as_str() {
                "escape" => self.cancel_rename(),
                "enter" => self.commit_rename(cx),
                "backspace" => {
                    self.rename_buffer.pop();
                }
                _ => {
                    if !mods.control && !mods.alt && !mods.platform {
                        let text =
                            keystroke
                                .key_char
                                .clone()
                                .or_else(|| match keystroke.key.as_str() {
                                    "space" => Some(" ".to_owned()),
                                    k if k.chars().count() == 1 => Some(k.to_owned()),
                                    _ => None,
                                });
                        if let Some(text) = text {
                            self.rename_buffer.push_str(&text);
                        }
                    }
                }
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // The image-widget hotkey — `matchesShortcut` against
        // `imageInsertShortcut` (default Mod+Shift+I). The shell's guard
        // skipped real inputs and dialogs only; a focused xterm was NOT an
        // input context, so this runs even over the focused terminal (the
        // image lands in a widget, not the PTY).
        if Self::shortcut_matches(keystroke, &self.settings.image_insert_shortcut) {
            self.image_insert(cx);
            cx.stop_propagation();
            return;
        }

        // Frame focus — the widget frame's own keydown in the shell, reached
        // after a header or handle press. Arrows nudge 1px (⇧16), Alt+arrows
        // resize through the same clamp the pointer handles use, Delete asks
        // to close a terminal, Escape hands focus back to the body — or to
        // the canvas for a pane. Sits above the terminal branch on purpose:
        // a terminal whose frame holds focus never sees these keys.
        if let Some(id) = self.frame_focus.clone() {
            let widget = self.canvas.widgets.get(&id).cloned();
            let is_term = widget
                .as_ref()
                .map(|w| is_terminal_kind(w.kind.as_deref()))
                .unwrap_or(false);
            let mut handled = false;
            match keystroke.key.as_str() {
                "left" | "right" | "up" | "down" => {
                    if let Some(mut w) = widget {
                        // A maximized frame ignores the keys, as the shell's
                        // handler early-returned.
                        if !w.maximized {
                            let step = if mods.shift { 16.0 } else { 1.0 };
                            let (fx, fy) = match keystroke.key.as_str() {
                                "left" => (-1.0, 0.0),
                                "right" => (1.0, 0.0),
                                "up" => (0.0, -1.0),
                                _ => (0.0, 1.0),
                            };
                            if mods.alt {
                                // Alt+arrow resizes the near edge: left/up
                                // drag their own edge, right/down extend —
                                // clamped exactly like the pointer handles.
                                let (nw, nh) = canvas::clamp_widget_size(
                                    w.kind.as_deref(),
                                    w.w + fx * step,
                                    w.h + fy * step,
                                );
                                if fx < 0.0 {
                                    w.x += w.w - nw;
                                }
                                if fy < 0.0 {
                                    w.y += w.h - nh;
                                }
                                w.w = nw;
                                w.h = nh;
                            } else {
                                w.x += fx * step;
                                w.y += fy * step;
                            }
                            self.canvas.widgets.insert(id.clone(), w.clone());
                            self.commit_widget(&w);
                        }
                        handled = true;
                    } else {
                        self.frame_focus = None;
                    }
                }
                "delete" | "backspace" if !mods.control && !mods.alt && !mods.platform => {
                    if widget.is_some() {
                        if is_term {
                            // `confirm("Close terminal? The running process
                            // will be terminated.")` — the dialog below.
                            self.pending_close = Some(id.clone());
                        } else {
                            self.close_widget(&id, cx);
                        }
                    } else {
                        self.frame_focus = None;
                    }
                    handled = true;
                }
                "escape" => {
                    // Terminal frame Escape went to the xterm as `\x1b` and
                    // refocused it; a pane's Escape refocused the canvas.
                    if is_term {
                        let _ = self.manager.write_text(&id, "\x1b", false);
                    }
                    self.frame_focus = None;
                    handled = true;
                }
                key => {
                    // A printable char on a terminal's frame refocused the
                    // xterm and was eaten by it — the keypress went nowhere.
                    if is_term
                        && key.chars().count() == 1
                        && !mods.control
                        && !mods.alt
                        && !mods.platform
                    {
                        self.frame_focus = None;
                        handled = true;
                    } else if widget.is_none() {
                        self.frame_focus = None;
                    }
                }
            }
            if handled {
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }

        // A focused terminal owns the keyboard, like the xterm widget did.
        if let Some(id) = self.active.clone() {
            if is_terminal_kind(self.widget_kind(&id).as_deref()) {
                // isGlobalZoom: Ctrl/Cmd+=, -, 0 bypass the terminal too —
                // the shell's one shortcut a focused xterm never swallowed.
                if (mods.control || mods.platform)
                    && matches!(keystroke.key.as_str(), "=" | "+" | "-" | "0")
                {
                    let center = Pos2::new(viewport.x / 2.0, viewport.y / 2.0);
                    match keystroke.key.as_str() {
                        "=" | "+" => zoom_at(&mut self.canvas.camera, Pos2::ZERO, center, 1.2),
                        "-" => zoom_at(&mut self.canvas.camera, Pos2::ZERO, center, 1.0 / 1.2),
                        _ => {
                            let zoom = self.canvas.camera.zoom;
                            if zoom != 1.0 {
                                let world = self.view().to_world(center);
                                self.canvas.camera.zoom = 1.0;
                                self.canvas.camera.x = (center.x - world.x) as f64;
                                self.canvas.camera.y = (center.y - world.y) as f64;
                            }
                        }
                    }
                    self.camera_dirty_at = Some(Instant::now());
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                // isTerminalPasteShortcut: Ctrl+Shift+V, Ctrl+V, Alt+V, and
                // Shift+Insert all paste — a bare Ctrl+V must NOT reach the
                // PTY as ^V (the shell's literal-next key).
                let paste = match keystroke.key.as_str() {
                    "v" | "V" => mods.control || mods.alt,
                    "insert" => mods.shift,
                    _ => false,
                };
                if paste {
                    // The shell's document-level paste handler: clipboard
                    // files attach as quoted path tokens, a clipboard image
                    // saves into the media store and attaches the same way,
                    // and plain text pastes verbatim — in that order.
                    if let Some(item) = cx.read_from_clipboard() {
                        let mut tokens = String::new();
                        let mut image_saved = false;
                        for entry in &item.entries {
                            match entry {
                                ClipboardEntry::ExternalPaths(paths) => {
                                    for path in paths.paths() {
                                        if let Ok(token) = slate_app::attachments::path_token(
                                            &path.to_string_lossy(),
                                        ) {
                                            tokens.push_str(&token);
                                        }
                                    }
                                }
                                ClipboardEntry::Image(image) if !image_saved => {
                                    let media_dir = slate_app::ipc::user_data_dir().join("media");
                                    if std::fs::create_dir_all(&media_dir).is_ok() {
                                        let path = media_dir.join(format!(
                                            "pasted-{}.{}",
                                            uuid::Uuid::new_v4().simple(),
                                            image.format.extension()
                                        ));
                                        if std::fs::write(&path, &image.bytes).is_ok() {
                                            if let Ok(token) = slate_app::attachments::path_token(
                                                &path.to_string_lossy(),
                                            ) {
                                                tokens.push_str(&token);
                                            }
                                            image_saved = true;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        if tokens.is_empty() {
                            if let Some(text) = item.text() {
                                let _ = self.manager.key_input(&id, &KeyInput::Paste(text));
                            }
                        } else {
                            let _ = self.manager.key_input(&id, &KeyInput::Paste(tokens));
                        }
                    }
                    cx.stop_propagation();
                    return;
                }
                // Ctrl+Shift+C/X copies the selection then clears it — the
                // shell's non-mac copy path. A bare Ctrl+C still sends ^C.
                if mods.control
                    && mods.shift
                    && matches!(keystroke.key.as_str(), "c" | "x" | "C" | "X")
                {
                    if let Some(text) = self.manager.selection_text(&id) {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        self.manager.selection_clear(&id);
                        self.refresh_selection(&id);
                    }
                    cx.stop_propagation();
                    return;
                }
                // xterm's scrollback keys: Shift+PageUp/Down = page,
                // Shift+Home/End = top/bottom, Shift+Arrow = line (x5 with
                // Ctrl) — they scroll the pane instead of encoding to the PTY.
                if mods.shift {
                    let rows = self.manager.screen_rows(&id) as i32;
                    let step = if mods.control { 5 } else { 1 };
                    let lines = match keystroke.key.as_str() {
                        "pageup" => Some(rows),
                        "pagedown" => Some(-rows),
                        "home" => Some(i32::MAX),
                        "end" => Some(i32::MIN),
                        "up" => Some(step),
                        "down" => Some(-step),
                        _ => None,
                    };
                    if let Some(lines) = lines {
                        let _ = self.manager.scroll_lines(&id, lines);
                        cx.stop_propagation();
                        return;
                    }
                }
                if let Some(input) = crate::views_terminal::map_keystroke(keystroke) {
                    if let Err(error) = self.manager.key_input(&id, &input) {
                        eprintln!("key_input {id} failed: {error}");
                    }
                    // scrollOnUserInput: typing snaps the pane to the bottom.
                    let _ = self.manager.scroll_to_bottom(&id);
                    cx.stop_propagation();
                }
                return;
            }
        }

        // `/` opens the bottom command bar — the Electron Toolbar's command
        // input — and seeds it with the prefix pressed. `.` and `@` are the
        // same gesture in the original's prefix set. This lives below the
        // terminal branch so a focused terminal still receives its `/`.
        if !mods.control && !mods.alt && !mods.platform {
            // key_char is None under synthetic events; fall back to the
            // keysym names for the three prefixes.
            let prefix = keystroke
                .key_char
                .as_deref()
                .or(match keystroke.key.as_str() {
                    "slash" => Some("/"),
                    "period" => Some("."),
                    "at" => Some("@"),
                    _ => None,
                });
            if let Some(prefix @ ("/" | "." | "@")) = prefix {
                self.command_bar = Some(prefix.to_owned());
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }

        // Canvas-level shortcuts, same keys the Electron shell advertised.
        let key = keystroke.key.as_str();
        match key {
            "escape" => {
                if self.draw_mode || self.erase_mode || self.select_mode {
                    self.draw_mode = false;
                    self.erase_mode = false;
                    self.select_mode = false;
                    self.pending_stroke.clear();
                } else if let Some(id) = self
                    .canvas
                    .widgets
                    .values()
                    .find(|widget| widget.maximized)
                    .map(|widget| widget.id.clone())
                {
                    // Escape restores a maximized widget — unless a terminal
                    // held the key, which is why this sits below the terminal
                    // branch just like the shell's xterm guard.
                    self.toggle_maximize(&id, cx);
                } else if self.active.is_some() {
                    // A non-terminal widget (or a stale active id) releases
                    // focus on Escape — terminals keep it (their Escape went
                    // to the PTY above).
                    self.active = None;
                } else {
                    return;
                }
            }
            "d" if !mods.control && !mods.alt && !mods.platform => {
                if mods.shift {
                    // ⇧D is the toolbar's "Clear all drawings".
                    if !self.canvas.strokes.is_empty() {
                        self.canvas.strokes.clear();
                        self.commit_strokes();
                    }
                } else {
                    self.draw_mode = !self.draw_mode;
                    // `tool` was one state — draw, erase and select never
                    // coexist.
                    self.erase_mode = false;
                    self.select_mode = false;
                    if !self.draw_mode {
                        self.pending_stroke.clear();
                    }
                }
            }
            "e" if !mods.control && !mods.alt && !mods.platform && !mods.shift => {
                // The toolbar's Eraser tool — drags cut strokes under the
                // pointer. Mutually exclusive with draw, as `tool` was.
                self.erase_mode = !self.erase_mode;
                if self.erase_mode {
                    self.draw_mode = false;
                    self.select_mode = false;
                    self.pending_stroke.clear();
                }
            }
            "s" if !mods.control && !mods.alt && !mods.platform && !mods.shift => {
                // The toolbar's Select tool — empty-canvas drags draw a
                // marquee; the topmost widget inside it comes to the front.
                self.select_mode = !self.select_mode;
                if self.select_mode {
                    self.draw_mode = false;
                    self.erase_mode = false;
                    self.pending_stroke.clear();
                }
            }
            "c" if !mods.control
                && !mods.alt
                && !mods.platform
                && !mods.shift
                && self.draw_mode =>
            {
                // Cycle the toolbar's swatch row; the next stroke takes it.
                self.stroke_color = (self.stroke_color + 1) % STROKE_COLORS.len();
            }
            "x" if !mods.control && !mods.alt && !mods.platform && mods.shift && self.draw_mode => {
                // The toolbar's "Erase entire drawing" (confirm dialog in
                // the shell; ⇧D already clears unconditionally here, so ⇧X
                // matches it).
                if !self.canvas.strokes.is_empty() {
                    self.canvas.strokes.clear();
                    self.commit_strokes();
                }
            }
            "t" | "T" | "n" | "N" if !mods.control && !mods.alt && !mods.platform => {
                // The shell bound both letters to `orcspace:new-terminal`.
                self.spawn_terminal(viewport, cx);
            }
            "+" | "=" => {
                let center = Pos2::new(viewport.x / 2.0, viewport.y / 2.0);
                zoom_at(&mut self.canvas.camera, Pos2::ZERO, center, 1.2);
                self.camera_dirty_at = Some(Instant::now());
            }
            "-" => {
                let center = Pos2::new(viewport.x / 2.0, viewport.y / 2.0);
                zoom_at(&mut self.canvas.camera, Pos2::ZERO, center, 1.0 / 1.2);
                self.camera_dirty_at = Some(Instant::now());
            }
            "0" => {
                // `{...c, zoom: 1}` — the original reset zoom literally and
                // left x/y alone; the world under the pointer does not stay
                // anchored, it shifts back toward the origin.
                if self.canvas.camera.zoom != 1.0 {
                    self.canvas.camera.zoom = 1.0;
                    self.camera_dirty_at = Some(Instant::now());
                }
            }
            "f" | "F" | "а" | "А" if !mods.control && !mods.alt && !mods.platform => {
                // Cyrillic а is the same physical key — the shell aliased it.
                self.fit(viewport, cx);
            }
            "home" => {
                self.canvas.camera = Default::default();
                self.camera_dirty_at = Some(Instant::now());
            }
            "left" | "right" | "up" | "down" => {
                let step = if mods.shift { 10.0 } else { 50.0 };
                let (dx, dy) = match key {
                    "left" => (step, 0.0),
                    "right" => (-step, 0.0),
                    "up" => (0.0, step),
                    _ => (0.0, -step),
                };
                self.canvas.camera.x += dx;
                self.canvas.camera.y += dy;
                self.camera_dirty_at = Some(Instant::now());
            }
            _ => return,
        }
        cx.notify();
    }
}
