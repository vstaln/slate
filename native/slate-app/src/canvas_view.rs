//! The workspace shell — Slate's root UI. Widgets render as panes: one
//! fills the work area under the pane strip, and the strip (or the sidebar,
//! or Ctrl+Tab) switches between them. Everything still mutates through the
//! journal so a relaunch restores the pane set. Widget state reduction comes
//! from `projection.rs`, colours from `theme.rs` — both ports of the
//! Electron renderer's `canvasState.ts`/`tokens.ts`.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};

use crate::engine::{TerminalManager, TerminalSnapshot};
use slate_app::canvas::{self, Pos2, Vec2};
use slate_app::journal_log::JournalLog;
use slate_app::projection::{self, CanvasState, Widget};
use slate_app::terminal_screen::{
    KeyInput, KeyMods, MouseButton as TermMouseButton, MouseKind, CELL_H, CELL_W,
    FONT_FAMILY, FONT_PX,
};
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


/// The shell's `isTerminal`: a widget with no kind is a terminal, not an
/// "unknown widget" — legacy journal entries rely on it.
fn is_terminal_kind(kind: Option<&str>) -> bool {
    matches!(kind, None | Some("terminal"))
}


/// StartupWordmark's fixed beats, in seconds since the first frame: the
/// particle timeline runs to t=8, and the startup screen itself fades out
/// over the following ~0.9s (`is-ready`'s 200ms delay + 900ms transition).
const SPLASH_PARTICLES_END: f32 = 8.0;

/// `smooth()` from StartupWordmark.tsx — smoothstep on a clamped [0,1].
fn smooth01(value: f32) -> f32 {
    let t = value.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// One react-bits `cloudPoint`: a unit-sphere position plus the four
/// `random` seeds the draw loop reads, the per-dot stagger delay and its
/// base radius.
struct SplashPoint {
    cloud: [f32; 3],
    random: [f32; 4],
    delay: f32,
    radius: f32,
}

/// splitmix64 — the splash wants a stable field across frames, not crypto;
/// this avoids pulling `rand` in for one overlay.
struct SplashRng(u64);

impl SplashRng {
    fn next(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        ((z >> 40) as f32) / 16_777_216.0
    }
}

/// particleCloud.ts `cloudPoint()` over `Math.min(2400, w*h/500)` dots —
/// the rejection sample into the unit sphere scaled by `cbrt(rand)`,
/// followed by the four `random` seeds, `delay = rand*2` and
/// `radius = .3 + rand*.35`, in that order.
fn splash_field(width: f32, height: f32) -> Vec<SplashPoint> {
    let count = ((width * height) / 500.0).min(2400.0) as usize;
    let mut rng = SplashRng(0x5A17_5A17_5A17_5EED);
    (0..count)
        .map(|_| {
            let (x, y, z) = loop {
                let x = rng.next() * 2.0 - 1.0;
                let y = rng.next() * 2.0 - 1.0;
                let z = rng.next() * 2.0 - 1.0;
                let length = x * x + y * y + z * z;
                if length <= 1.0 && length > 0.0 {
                    break (x, y, z);
                }
            };
            let radius = rng.next().cbrt();
            SplashPoint {
                cloud: [x * radius, y * radius, z * radius],
                random: [rng.next(), rng.next(), rng.next(), rng.next()],
                delay: rng.next() * 2.0,
                radius: 0.3 + rng.next() * 0.35,
            }
        })
        .collect()
}

/// StartupWordmark's per-dot draw loop, line for line: dots scattered over
/// the viewport spiral into the wordmark point at (w/2, h*.47), burst
/// outward at t≈5.9, and the whole field fades between t=6 and t=7.
/// Painted as white rounded quads standing in for the DOM's filled arcs.
fn paint_splash_particles(window: &mut Window, points: &[SplashPoint], t: f32) {
    let viewport = window.viewport_size();
    let width = f32::from(viewport.width);
    let height = f32::from(viewport.height);
    let cx = width / 2.0;
    let cy = height * 0.47;
    let fade = 1.0 - smooth01((t - 6.0) / 1.0);
    if fade <= 0.0 {
        return;
    }
    for p in points {
        let r = p.random;
        let travel = ((t - 1.0 - p.delay) / (6.5 - p.delay * 0.4)).clamp(0.0, 1.0);
        let pull = smooth01(travel).powf(1.5);
        // moveCloudPoint(cloud, t * .3) — the tiny idle drift on the
        // scattered start position.
        let drift_t = t * 0.3;
        let cloud_x = p.cloud[0] + (drift_t * r[2] + std::f32::consts::TAU * r[3]).sin() * (0.015 + 0.06 * r[0]);
        let cloud_y = p.cloud[1] + (drift_t * r[1] + std::f32::consts::TAU * r[0]).sin() * (0.015 + 0.06 * r[3]);
        let sx = r[0] * width + cloud_x * 12.0;
        let sy = r[1] * height + cloud_y * 12.0;
        let angle = (sy - cy).atan2(sx - cx) + pull * (2.5 + r[3] * 2.0);
        let burst = ((t - 5.9 - p.delay * 0.08) / 0.55).clamp(0.0, 1.0);
        let distance = if burst > 0.0 {
            (8.0 + r[2] * 55.0) * (1.0 + burst * burst * 125.0)
        } else {
            (sx - cx).hypot(sy - cy) * (1.0 - pull)
        };
        let x = cx + angle.cos() * distance;
        let y = cy + angle.sin() * distance;
        let alpha = smooth01(t / 1.5)
            * if burst > 0.0 {
                smooth01(burst * 7.0)
            } else {
                1.0 - smooth01((travel - 0.93) / 0.07)
            }
            * (0.25 + r[2] * 0.65)
            * fade;
        if alpha <= 0.004 {
            continue;
        }
        let radius = p.radius * (1.0 + burst * 6.0);
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
        window.paint_quad(
            gpui::fill(
                Bounds::new(
                    point(px(x - radius), px(y - radius)),
                    size(px((radius * 2.0).max(1.0)), px((radius * 2.0).max(1.0))),
                ),
                rgba(0xffff_ff00 | a),
            )
            .corner_radii(Corners::all(px(radius))),
        );
    }
}

/// A one-line tooltip body — stand-in for the shell's `title=` attribute
/// on the kbd cheatsheet rows.
struct TooltipLine(&'static str);

impl Render for TooltipLine {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .text_xs()
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .child(self.0)
    }
}

/// A `kbd` cheatsheet chip — the bordered mono keycap in the empty card.
fn kbd_chip(label: &'static str) -> Div {
    div()
        .px_1()
        .py(px(0.5))
        .text_size(px(9.0))
        .font_family("Departure Mono")
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .child(label)
}

/// Chip plus its word under the row's shared `title=` tooltip, e.g.
/// `<kbd>0</kbd> zoom` — "0 resets zoom only".
fn kbd_hint(chip: &'static str, suffix: &'static str, tip: &'static str) -> Stateful<Div> {
    div()
        .id(tip)
        .flex()
        .items_center()
        .gap(px(2.0))
        .tooltip(move |_, cx| AnyView::from(cx.new(|_| TooltipLine(tip))))
        .child(kbd_chip(chip))
        .child(suffix)
}


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
    pub(crate) active: Option<String>,
    /// Widget whose *frame* owns the keyboard — the shell's DOM focus on
    /// the frame element after a header or handle press. Arrows nudge it
    /// 1px (⇧16), Alt+arrows resize, Delete closes, Escape hands focus
    /// back to the body. A terminal's body press clears it — the xterm
    /// textarea retakes focus just like in the shell.
    frame_focus: Option<String>,
    /// Terminal awaiting the "Close terminal? The running process will be
    /// terminated." answer — the shell's `confirm()` dialog.
    pending_close: Option<String>,
    /// (cols, rows) last pushed to each PTY — resized only on change.
    term_cells: HashMap<String, (u16, u16)>,
    /// Debounce slot for PTY resizes — the shell's 150ms fit timer.
    /// `(target size, last change)` per terminal; applied once settled.
    term_resize_pending: HashMap<String, ((u16, u16), Instant)>,
    /// Screen position of the right-click spawn menu, when open.
    menu: Option<Pos2>,
    /// Context-menu keyboard cursor — the shell's arrow/Enter nav. Resets
    /// to 0 each time the menu opens.
    menu_sel: usize,
    /// Screen-space pointer position while an OS file drag hovers the
    /// canvas — `on_drag_move::<ExternalPaths>` keeps it fresh so `on_drop`
    /// knows where the files landed (the shell's `M(clientX, clientY)`).
    drop_point: Option<Pos2>,
    /// The shell's `cascadeRef` — counts socket-adopted widgets so each
    /// lands at `screenToWorld(90 + col*60, 90 + row*60)`, six per row,
    /// reset when the canvas hydrates (workspace switch included).
    cascade_count: usize,
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
    /// Widget id mid-title-rename plus the edit buffer — the shell's
    /// double-click-on-title input. Keystrokes route here while it is set.
    renaming: Option<String>,
    rename_buffer: String,
    /// The browser omnibox's editing state — the widget id whose address
    /// field owns the keyboard, plus its edit buffer. Same shape as
    /// `renaming`/`rename_buffer`; the pane sets it from its own click.
    pub(crate) browser_editing: Option<String>,
    pub(crate) browser_address: String,
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
    /// The launcher's worktree toggle — `launch_code_session` spends it
    /// on `git worktree add` when the workspace is a repo.
    code_worktree: bool,
    /// herdr's pane classifier output per terminal — refreshed on the
    /// snapshot tick (`refresh_agent_states`), the card dots' source.
    agent_states: HashMap<String, slate_app::agent_state::AgentState>,
    /// Throttle for the classifier — a busy terminal repaints far faster
    /// than tail-pattern rules need to see it.
    agent_scan_at: Instant,
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
    /// Startup splash clock — the Electron `StartupWordmark` overlay.
    /// `Some` holds the launch instant while the intro plays; `None` once
    /// it finishes or the first deliberate press dismisses it.
    splash_started: Option<Instant>,
    /// The particle field — react-bits `cloudPoint`s generated once the
    /// viewport size is known (the count scales with it). `Rc` so the
    /// paint closure can share it without a per-frame clone.
    splash_points: Rc<Vec<SplashPoint>>,
    /// Boot work the splash actually waits on: ids of every terminal PTY
    /// respawned from the journal, dropped as each shows its first screen
    /// (or exits). `ready` does not flip while this is non-empty — the
    /// "Preparing your workspace" caption measures real loading instead
    /// of running on a fixed clock.
    splash_pending: HashSet<String>,
    /// When boot work and the intro animation both finished — the ready
    /// latch. The screen's fade-out runs off this instant, not the launch
    /// clock, so slow PTYs hold the loader instead of lying about it.
    splash_ready_at: Option<Instant>,
    /// The wallpaper as rendered, keyed `(data-url hash, backgroundBlur)`
    /// — the blur runs off-thread (`blur_wallpaper`) because gpui has no
    /// image filter; the unblurred decode paints until it lands.
    wallpaper_cache: Option<(u64, u64, gpui::ImageSource)>,
    /// The plain decode keyed by data-url hash — the blur job's fallback
    /// paint and the `backgroundBlur: 0` path.
    wallpaper_raw: Option<(u64, gpui::ImageSource)>,
    /// The key of the in-flight blur job so renders don't stack workers.
    wallpaper_blurring: Option<(u64, u64)>,
    /// Lazily-resolved wallpaper for `backgroundImage` paths — the Electron
    /// shell stored a managed *file path* there (not a data URL), so the
    /// render read below encodes it once and memoizes on the raw string.
    wallpaper_resolved: Option<(String, String)>,
    /// `codeSessions` — the Code view's cards. Each owns a manager PTY
    /// (`terminal_id`) that has no canvas widget; the pane paints it from
    /// `snapshots` like a terminal widget body does.
    code_sessions: Vec<crate::views_code::CodeSession>,
    /// `ResumeAgents` — conversations scanned for `workspace_dir`, newest
    /// first. See `views_code::CodeState::conversations`.
    code_conversations: Vec<slate_app::code_resume::AgentConversation>,
    /// The dir the last scan answered for — `None` arms a fresh scan the
    /// next time the empty code grid renders.
    code_resume_dir: Option<String>,
    code_resume_loading: bool,
    code_resume_expanded: bool,
    /// `conversation_key`s — ticked rows / waved-away rows.
    code_resume_selected: std::collections::HashSet<String>,
    code_resume_dismissed: std::collections::HashSet<String>,
    /// `selectedAgentIds` / `agentCounts` / `activeAgentId` — the
    /// multi-select fan-out state shared by the empty grid and the
    /// launcher panel.
    code_selected_agents: Vec<String>,
    code_agent_counts: std::collections::HashMap<String, u32>,
    code_active_agent: Option<String>,
    /// `focusedSessionId` — the card with the accent hairline; its
    /// terminal_id also lands in `active` so the PTY gets the keyboard.
    code_focused: Option<String>,
    /// `launcherOpen` — the `+` pick row of agent chips.
    code_launcher_open: bool,
    /// `layoutMode` — `code_layout::arrange`'s mode for the card grid.
    code_layout: slate_app::code_layout::CodeLayout,
    counter: u64,
    // fix-files: the files pane's keyboard owner — its `Search files…`
    // field or an in-row rename, keyed by widget id exactly like
    // `browser_editing`/`browser_address`. Set from the pane's own clicks;
    // `on_key` routes keystrokes while it is `Some`.
    pub(crate) files_editing: Option<crate::views_files::FilesEditing>,
    // fix-files: the files pane's `previewFile` — the loaded preview
    // overlay. Lives here rather than in `widget.state` so file bytes
    // never enter the journal.
    pub(crate) files_preview: Option<crate::views_files::FilesPreview>,
    // fix-browser: the omnibox's caret and selection — `browser_address`
    // edits at `browser_cursor`; `browser_sel` is the selection's anchor
    // (byte offsets on char boundaries, `None` = a collapsed caret). The DOM
    // input's model: `anchor..cursor` is the highlighted range.
    pub(crate) browser_cursor: usize,
    pub(crate) browser_sel: Option<usize>,
    // fix-browser: App.tsx's canvas-wide `window.open` budget — at most 3
    // popups per sliding 5s, on top of webview.rs's per-webview gate.
    popup_opens: std::collections::VecDeque<Instant>,
    // fix-planner: the planner pane's open text field (create form or a row's
    // inline title/note/date) — it owns the keyboard while `Some`, the same
    // single-owner shape as `browser_editing`. `planner_draft` holds the create
    // form's text and staged photos.
    pub(crate) planner_editing: Option<crate::views_planner::PlannerEdit>,
    pub(crate) planner_draft: crate::views_planner::PlannerDraft,
    // fix-orchestration: the orchestration pane's keyboard owner — the reply
    // or permission-note field armed by its own click — and the typed text
    // per message id, which survives focus moving between cards.
    orc_editing: Option<crate::views_orchestration::OrcEdit>,
    orc_drafts: HashMap<String, String>,
    // fix-orchestration: the last rejected control write, shown as the
    // pane's red banner until dismissed or the next write succeeds.
    orc_error: Option<String>,
    // fix-panes: the notes pane's open field — (note id, field name) — and
    // its edit buffer. Keystrokes route here while it is `Some`.
    pub(crate) notes_editing: Option<(String, String)>,
    pub(crate) notes_buffer: String,
}

/// A `/term <command>` line waiting to be typed into a fresh PTY.
struct PendingCommand {
    terminal_id: String,
    command: String,
    attempts: u8,
    /// True for agent relaunches — once the line lands the terminal's
    /// output is watched for a startup failure.
    watch: bool,
    /// The shell's `live` — the PTY was already running when the widget
    /// connected. A live terminal whose emulator sits on the alternate
    /// buffer or has mouse reporting armed is owned by a TUI, and the
    /// initial command must not be typed into it.
    live: bool,
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
        // The catalog kinds the port added beyond the shell's five —
        // without these a suggestion adopts `/timer` only to write the
        // literal text into a terminal instead of spawning the widget.
        "sys-monitor" | "sysmon" | "monitor" | "cpu" => "sys-monitor",
        "timer" | "countdown" | "stopwatch" => "timer",
        "image" | "img" | "picture" | "photo" => "image",
        "links" | "link" | "bookmarks" => "links",
        "notes" | "note" => "notes",
        "calendar" | "cal" => "calendar",
        "kanban" | "board" => "kanban",
        _ => return None,
    })
}

/// One row of the arrange popover — label + hint + `✓` on the active
/// mode; the shell's `role=menuitemradio` row. The click handler stays
/// with the caller: Canvas rows journal an arrange, Code rows pick a
/// terminal layout.
fn arrange_menu_row(slug: String, label: &'static str, hint: &'static str, current: bool) -> Stateful<Div> {
    div()
        .id(slug)
        .w_full()
        .px_2()
        .py(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .text_color(rgb(theme::hex(if current {
            theme::text::NORMAL
        } else {
            theme::text::DIM
        })))
        .hover(|s| s.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(label)
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(hint),
                ),
        )
        .when(current, |el| {
            el.child(
                div()
                    .flex_none()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    .child("✓"),
            )
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
    // `typedPrefix && typedPrefix !== commandPrefix` — the sigil gate
    // applies only when the user actually typed one; a bare query suggests
    // regardless of the configured prefix.
    if command_prefix != "any"
        && sigil.map(|c| c.to_string()).as_deref().is_some_and(|s| s != command_prefix)
    {
        return Vec::new();
    }
    // suggestionQuery was lowercased — `/Term` still completes `terminal`.
    let query = match sigil {
        Some(_) => first[1..].to_lowercase(),
        None => first.to_lowercase(),
    };
    let squashed = query.replace('-', "");
    let mut out: Vec<(String, String, String)> = WIDGET_CATALOG
        .iter()
        .filter(|(kind, _, _)| renderable(kind))
        .filter(|(kind, _, _)| {
            kind.starts_with(query.as_str())
                || kind.replace('-', "").starts_with(squashed.as_str())
        })
        .map(|&(kind, label, hint)| (kind.to_owned(), label.to_owned(), hint.to_owned()))
        .collect();
    // `/agent` is a verb, not a widget — it persists a custom launcher
    // chip — but it still completes like one in the listbox.
    if "agent".starts_with(query.as_str()) || "agent".starts_with(squashed.as_str()) {
        out.push((
            "agent".to_owned(),
            "Agent".to_owned(),
            "Save a custom CLI to the launcher".to_owned(),
        ));
    }
    out
}

/// WidgetFrame.tsx AGENTS — the launch picker types the command into the
/// terminal and presses Enter. The pick list is dynamic like the launcher's:
/// the built-in `gray` plus the user's `/agent` customs, resolving to
/// `(id, label, command)` rows. The id persists as
/// `orcspace-launched-agent:<id>` so the pick re-launches.
fn agent_launchers(
    prefs: &slate_app::ui_preferences::UiPreferences,
) -> Vec<(String, String, String)> {
    crate::views_code::CODE_AGENTS
        .iter()
        .map(|a| (a.id.to_owned(), a.label.to_owned(), a.command.to_owned()))
        .chain(
            prefs
                .custom_agents
                .iter()
                .map(|a| (a.id.clone(), a.label.clone(), a.command.clone())),
        )
        .collect()
}


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

/// The GUI's `RouteEnv` — the same live objects `EngineEnv` hands the HTTP
/// router, borrowed from the view instead of the server. `rate_limit`
/// always answers `true`: `isPrivileged` in the TypeScript skipped the flow
/// limiter for actors outside the agent registry (`user`, `system`,
/// `arrange`), and the canvas never submits under a registered agent id.
/// Locks and `baseVersion` still gate, so a `user` drag and an `api`
/// command contend for `widget:x` on the same terms the renderer did.
struct GuiEnv<'a> {
    manager: &'a TerminalManager,
}

fn lock_recover<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The apply's `throw new CommandError(code, message)` — the abort entry
/// carries the code, the notice carries the message.
fn command_failed(
    code: slate_app::command::ErrorCode,
    message: impl Into<String>,
) -> slate_app::http::Response {
    slate_app::http::Response::from_error(&slate_app::command::CommandError::new(
        code,
        message.into(),
    ))
}

/// The submit chain without a `CanvasView` — the intent→work→commit halves
/// `commit_work` wraps, callable where `self` cannot reach (the boot
/// relaunch runs inside `new`, the mtime watcher inside its closure).
#[allow(clippy::too_many_arguments)]
fn gui_submit(
    manager: &TerminalManager,
    workspace_dir: Option<&str>,
    actor: &str,
    base_version: Option<f64>,
    entry_type: &str,
    target: &str,
    payload: Value,
    work: impl FnOnce() -> Result<Value, slate_app::http::Response>,
) -> Result<
    (slate_app::journal::JournalEntry, Value),
    slate_app::http::Response,
> {
    let mut env = GuiEnv { manager };
    let mut orchestration = load_orchestration_store();
    let mut deps = slate_app::http::RouteDeps {
        orchestration: &mut orchestration,
        app_version: env!("CARGO_PKG_VERSION"),
        workspace_dir,
        now: SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64),
        env: Some(&mut env),
    };
    match slate_app::http::submit_begin(
        &mut deps,
        actor,
        base_version,
        None,
        entry_type,
        target,
        payload,
        "canvas edit",
    ) {
        Ok(slate_app::http::Submit::Run(pending)) => {
            let outcome = work();
            // The commit *is* the write — the fold applies it like the
            // handler's store mutation did.
            slate_app::http::submit_finish(&mut deps, pending, outcome, "canvas edit")
        }
        Ok(slate_app::http::Submit::Replay(result)) => result,
        Err(response) => Err(response),
    }
}

/// `terminal:create` IPC — the widget-mount spawn went through
/// `send('terminal.spawn', terminal:{id})`, so a mount journals like any
/// other command: the intent, then the PTY work, then the commit. The
/// answer is `spawn_with_options`' `live` flag — `true` means the shell
/// was already running and this adopted it.
fn mount_spawn(
    manager: &TerminalManager,
    workspace_dir: Option<&str>,
    id: &str,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    title: Option<&str>,
) -> Result<bool, String> {
    let mut payload = json!({ "cols": cols, "rows": rows });
    if let Some(cwd) = &cwd {
        payload["cwd"] = json!(cwd);
    }
    if let Some(title) = title {
        payload["title"] = json!(title);
    }
    let mut live = None;
    let result = gui_submit(
        manager,
        workspace_dir,
        "user",
        None,
        "terminal.spawn",
        &format!("terminal:{id}"),
        payload,
        || {
            let adopted = manager.spawn_with_options(
                id.to_owned(),
                cols,
                rows,
                cwd,
                slate_app::platform::shell_command(),
                HashMap::new(),
            )
            .map_err(|error| command_failed(slate_app::command::ErrorCode::Failed, error))?;
            live = Some(adopted);
            // The apply answered `{ok, live, scrollback}` — `fullOutput` for
            // a reconnect, the retained scrollback otherwise.
            let scrollback = manager
                .snapshot(id)
                .map(|snapshot| snapshot.output)
                .unwrap_or_default();
            Ok(json!({ "ok": true, "live": adopted, "scrollback": scrollback }))
        },
    );
    match result {
        Ok(_) => live.ok_or_else(|| "terminal.spawn committed no spawn".to_owned()),
        Err(response) => Err(response
            .body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("terminal.spawn failed")
            .to_owned()),
    }
}

impl slate_app::http::RouteEnv for GuiEnv<'_> {
    fn journal(&mut self) -> Result<JournalLog, String> {
        JournalLog::open(journal_path())
    }

    fn terminals(&mut self) -> Vec<Value> {
        let names = self.manager.names();
        self.manager
            .snapshots()
            .iter()
            .map(|snapshot| {
                let name = names.get(&snapshot.id).cloned();
                json!({
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
    ) -> Result<String, slate_app::command::CommandError> {
        let id = self.manager.resolve(target, caller)?;
        self.manager
            .snapshot(&id)
            .map(|_| id.clone())
            .map_err(|_| {
                slate_app::command::CommandError::new(
                    slate_app::command::ErrorCode::NotFound,
                    format!("no terminal \"{id}\""),
                )
            })
    }

    fn resolve_write_target(&mut self, requested: &str) -> Option<String> {
        self.manager.resolve_write_target(requested)
    }

    fn next_terminal_id(&mut self, prefix: &str) -> String {
        self.manager.next_terminal_id(prefix)
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
            slate_app::platform::shell_command(),
            std::collections::HashMap::new(),
        )?;
        self.manager.set_name(id, title);
        Ok(())
    }

    fn terminal_running(&mut self, id: &str) -> bool {
        self.manager.is_running(id)
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
        lock_recover(slate_app::locks::shared())
    }

    fn data_dir(&self) -> std::path::PathBuf {
        slate_app::ipc::user_data_dir()
    }

    fn socket_path(&self) -> Option<String> {
        self.manager.control_socket()
    }

    fn target_version(&mut self, target: &str) -> u64 {
        let data_dir = slate_app::ipc::user_data_dir();
        let entries = self
            .journal()
            .map(|log| log.entries().to_vec())
            .unwrap_or_default();
        slate_app::http::folded_target_version(&entries, &data_dir, target)
    }

    fn idempotency(
        &mut self,
    ) -> std::sync::MutexGuard<'_, slate_app::idempotency::IdempotencyCache> {
        lock_recover(slate_app::idempotency::shared())
    }

    fn rate_limit(&mut self, _actor: &str, _now: i64) -> bool {
        true
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
        let prefs = slate_app::ui_preferences::UiPreferences::load_or_default();
        // `targetTerminalId` — the command bar's remembered target.
        let command_target = settings
            .extra("targetTerminalId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut relaunch: Vec<PendingCommand> = Vec::new();
        let mut boot_pending: HashSet<String> = HashSet::new();
        for widget in canvas.widgets.values() {
            if is_terminal_kind(widget.kind.as_deref()) {
                let (cols, rows) = terminal_cells(widget.w, widget.h, canvas.camera.zoom);
                // The Electron widget mount went through `terminal:create`
                // → `send('terminal.spawn')`, so a boot relaunch journals
                // intent+commit like the mount did.
                if let Ok(live) = mount_spawn(
                    &manager,
                    workspace_dir.as_deref(),
                    &widget.id,
                    cols,
                    rows,
                    None,
                    Some(&widget.title),
                ) {
                    boot_pending.insert(widget.id.clone());
                    if let Some(agent_id) = agent_picks.launched.get(&widget.id) {
                        if let Some((_, _, command)) = agent_launchers(&prefs)
                            .iter()
                            .find(|(id, _, _)| *id == *agent_id)
                        {
                            relaunch.push(PendingCommand {
                                terminal_id: widget.id.clone(),
                                command: command.clone(),
                                attempts: 0,
                                watch: true,
                                live,
                            });
                        }
                    }

                }
            }
        }

        // `orcspace-code-layout-mode` — the picker choice persists
        // across restarts through `native-ui.json`.
        let code_layout = prefs.layout;
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
            selection_drag: None,
            term_cells: HashMap::new(),
            term_resize_pending: HashMap::new(),
            menu: None,
            menu_sel: 0,
            drop_point: None,
            cascade_count: 0,
            notice: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            suppress_history: false,
            renaming: None,
            rename_buffer: String::new(),
            browser_editing: None,
            browser_address: String::new(),
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
            viewport: Vec2::new(0.0, 0.0),
            agent_picks,
            code_worktree: false,
            agent_states: HashMap::new(),
            agent_scan_at: Instant::now(),
            agent_relaunched: HashSet::new(),
            startup_watch: HashMap::new(),
            startup_notice: HashMap::new(),
            prefs,
            settings,
            arrange_menu: false,
            settings_open: false,
            // The shell's mount-time `refreshGit()` — due immediately.
            git_chip: Default::default(),
            git_next_poll: Instant::now(),
            git_polling: false,
            // The intro only runs when there's genuinely something
            // loading — respawned PTYs still coming up. An empty boot
            // (new workspace, nothing to restore) skips the screen
            // entirely; it was never a welcome, it just always pretended
            // there was work to do.
            splash_started: if boot_pending.is_empty() {
                None
            } else {
                Some(Instant::now())
            },
            splash_points: Rc::new(Vec::new()),
            splash_pending: boot_pending,
            splash_ready_at: None,
            wallpaper_cache: None,
            wallpaper_raw: None,
            wallpaper_blurring: None,
            wallpaper_resolved: None,
            code_sessions: Vec::new(),
            code_conversations: Vec::new(),
            code_resume_dir: None,
            code_resume_loading: false,
            code_resume_expanded: false,
            code_resume_selected: std::collections::HashSet::new(),
            code_resume_dismissed: std::collections::HashSet::new(),
            code_selected_agents: Vec::new(),
            code_agent_counts: std::collections::HashMap::new(),
            code_active_agent: None,
            code_focused: None,
            code_launcher_open: false,
            code_layout,
            counter: 0,
            // fix-files:
            files_editing: None,
            files_preview: None,
            // fix-browser:
            browser_cursor: 0,
            browser_sel: None,
            popup_opens: std::collections::VecDeque::new(),
            // fix-planner:
            planner_editing: None,
            planner_draft: Default::default(),
            // fix-orchestration:
            orc_editing: None,
            orc_drafts: HashMap::new(),
            orc_error: None,
            // fix-panes:
            notes_editing: None,
            notes_buffer: String::new(),
        };
        view.snapshots = view.manager.snapshots();
        view.refresh_agent_states();
        for pending in relaunch {
            view.agent_relaunched.insert(pending.terminal_id.clone());
            view.pending_commands.push(pending);
        }
        // First run: the Electron app opened empty and let the welcome card
        // ("Your canvas is clear" + an Add terminal button) do the greeting —
        // one key (T) or one click away from a shell.

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let alive = this
                    .update(cx, |view, cx| {
                        view.snapshots = view.manager.snapshots();
        view.refresh_agent_states();
                        // Recovery timers — the child-exit confirmation and
                        // the Ctrl-C/D corpse check (the shell's
                        // `modeRecoveryTimer`/`interruptRecoveryTimer`).
                        view.manager.tick_mode_recovery();
                        for event in view.manager.drain_events() {
                            // The shell closed the widget when its process
                            // exited (`onProcessExit ?? onClose`) after writing
                            // the dim marker — same here.
                            if let crate::engine::TerminalEvent::Exited { ref id, code } = event {
                                view.manager.mark_exited(id, code);
                                // Code sessions aren't canvas widgets — a
                                // finished session keeps its card and last
                                // screen (the shell's `status:'finished'`
                                // gray dot); a widget.remove journal entry
                                // for a widget that never existed is noise.
                                if let Some(session) = view
                                    .code_sessions
                                    .iter_mut()
                                    .find(|s| s.terminal_id == *id)
                                {
                                    session.alive = false;
                                    if view.active.as_deref() == Some(id.as_str()) {
                                        view.active = None;
                                    }
                                    cx.notify();
                                    continue;
                                }
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
                        // here: panes have no camera to move, so a widget id
                        // just raises and activates that pane.
                        if let Some(request) = view.manager.take_camera_request() {
                            if let Some(id) = request.widget_id.as_deref() {
                                if view.canvas.widgets.contains_key(id) {
                                    view.active = Some(id.to_owned());
                                }
                            }
                        }
                        // Second-instance raise: a `slate` invocation asked
                        // the running window to come forward. Same mailbox
                        // shape as take_camera_request — `Some(dir)` also
                        // carries the shell's `setWorkspaceDir` handoff.
                        if let Some(dir) = view.manager.take_raise_request() {
                            cx.activate(true);
                            for handle in cx.windows() {
                                let _ = handle.update(cx, |_, window, _| {
                                    window.activate_window();
                                });
                            }
                            if let Some(dir) = dir {
                                view.switch_workspace(dir);
                            }
                        }
                        // DECSET 1004: tell a terminal when widget focus
                        // actually moved, not per click — the original's
                        // xterm got focusin/focusout from the DOM.
                        let focus_now = view
                            .active
                            .clone()
                            .filter(|id| view.terminal_targetable(id));
                        if focus_now != view.focus_sent {
                            if let Some(old) = view.focus_sent.take() {
                                let _ = view.manager.focus_input(&old, false);
                            }
                            if let Some(new) = &focus_now {
                                let _ = view.manager.focus_input(new, true);
                            }
                            view.focus_sent = focus_now;
                        }
                        // The CLI is a second journal writer (`slate move`,
                        // `plan create`): when the file changes under us,
                        // refold so the panes match.
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
                                    // The shell merged deltas into live
                                    // state; the refold simply replaces it.
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
                                    let newcomers: Vec<(String, String, f64, f64)> = view
                                        .canvas
                                        .widgets
                                        .values()
                                        .filter(|w| {
                                            !prior.contains(&w.id)
                                                && is_terminal_kind(w.kind.as_deref())
                                                && view.manager.snapshot(&w.id).is_err()
                                        })
                                        .map(|w| {
                                            (w.id.clone(), w.title.clone(), w.w, w.h)
                                        })
                                        .collect();
                                    for (id, title, w, h) in newcomers {
                                        let (cols, rows) =
                                            terminal_cells(w, h, view.canvas.camera.zoom);
                                        // The mount journaled
                                        // `terminal.spawn` — same
                                        // intent+commit as the IPC.
                                        if let Ok(live) = mount_spawn(
                                            &view.manager,
                                            view.workspace_dir.as_deref(),
                                            &id,
                                            cols,
                                            rows,
                                            None,
                                            Some(&title),
                                        ) {
                                            view.relaunch_agent(&id, live);
                                        }
                                    }
                                    // `view.snapshots` was taken at tick
                                    // start — a journal-created terminal's
                                    // fresh PTY isn't in it yet, and the
                                    // stale sweep below would remove the
                                    // widget one beat after it appeared.
                                    // Refresh so `live` sees the spawns.
                                    view.snapshots = view.manager.snapshots();
        view.refresh_agent_states();
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
                            .filter(|id| {
                                !view.canvas.widgets.contains_key(id)
                                    // Code sessions own PTYs without widgets —
                                    // adopting one would drop a ghost
                                    // terminal onto the canvas.
                                    && !view
                                        .code_sessions
                                        .iter()
                                        .any(|s| &s.terminal_id == id)
                            })
                            .collect();
                        for id in orphans {
                            let title = view
                                .manager
                                .name(&id)
                                .unwrap_or_else(|| "Terminal".to_owned());
                            // The shell's `cascadeRef` grid: screen point
                            // (90 + col*60, 90 + row*60), six per row, run
                            // through `screenToWorld` — so adopted terminals
                            // fan out under the camera rather than stack.
                            let n = view.cascade_count;
                            view.cascade_count += 1;
                            let col = (n % 6) as f32;
                            let row = (n / 6) as f32;
                            let at = view
                                .view()
                                .to_world(Pos2::new(90.0 + col * 60.0, 90.0 + row * 60.0));
                            view.create_widget_at(&id, "terminal", &title, at);
                            // The mount journaled `terminal.spawn` too —
                            // `spawn` on a live shell answers `live: true`,
                            // so this is a journalled reconnect, not a second
                            // PTY.
                            let (w, h) = view
                                .canvas
                                .widgets
                                .get(&id)
                                .map(|widget| (widget.w, widget.h))
                                .unwrap_or((352.0, 295.0));
                            let (cols, rows) =
                                terminal_cells(w, h, view.canvas.camera.zoom);
                            let _ = mount_spawn(
                                &view.manager,
                                view.workspace_dir.as_deref(),
                                &id,
                                cols,
                                rows,
                                None,
                                Some(&title),
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
                        // The GTK/X11 webview threads report back through
                        // this mailbox — page-initiated navigations journal
                        // the same `set` the omnibox writes, `window.open`
                        // spawns a fresh browser widget (browser:onOpenTab).
                        for event in slate_app::webview::drain_events() {
                            match event {
                                slate_app::webview::WebViewEvent::Navigated { widget, url } => {
                                    view.widget_command(
                                        &widget,
                                        json!({ "op": "set", "state": { "url": url } }),
                                        cx,
                                    );
                                }
                                slate_app::webview::WebViewEvent::OpenTab { url, .. } => {
                                    // App.tsx's app-wide burst cap — at most
                                    // 3 `window.open`s per sliding 5s across
                                    // every browser, on top of webview.rs's
                                    // per-page 350ms dedup and 5/10s budget.
                                    // Dropping here is what kept a popup
                                    // loop from minting MAX_WIDGETS widgets.
                                    let now = Instant::now();
                                    while view
                                        .popup_opens
                                        .front()
                                        .is_some_and(|stamp| {
                                            now.duration_since(*stamp)
                                                > std::time::Duration::from_secs(5)
                                        })
                                    {
                                        view.popup_opens.pop_front();
                                    }
                                    if view.popup_opens.len() < 3 {
                                        view.popup_opens.push_back(now);
                                        let id = view.fresh_id("browser");
                                        let zoom = view.canvas.camera.zoom;
                                        let center = Pos2::new(
                                            ((view.viewport.x as f64 / 2.0
                                                - view.canvas.camera.x)
                                                / zoom) as f32,
                                            ((view.viewport.y as f64 / 2.0
                                                - view.canvas.camera.y)
                                                / zoom) as f32,
                                        );
                                        view.create_widget_at(
                                            &id, "browser", "Browser", center,
                                        );
                                        view.widget_command(
                                            &id,
                                            json!({ "op": "browser_navigate", "url": url }),
                                            cx,
                                        );
                                    }
                                }
                                slate_app::webview::WebViewEvent::Loaded { widget } => {
                                    // did-stop-loading reset
                                    // `retryCountRef` — a clean load
                                    // re-arms the error banner's Retry.
                                    // A load that ended in `status.error`
                                    // keeps the counter so the 3-try cap
                                    // still engages.
                                    let clean = slate_app::webview::status_for(&widget)
                                        .map(|status| status.error.is_none())
                                        .unwrap_or(true);
                                    let counted = view
                                        .canvas
                                        .widgets
                                        .get(&widget)
                                        .and_then(|widget| widget.state.as_ref())
                                        .and_then(|state| state.get("retry"))
                                        .and_then(Value::as_u64)
                                        .unwrap_or(0);
                                    if clean && counted > 0 {
                                        view.widget_command(
                                            &widget,
                                            json!({ "op": "set", "state": { "retry": 0 } }),
                                            cx,
                                        );
                                    }
                                }
                            }
                        }
                        // Journal-side removals never run close_widget —
                        // drop webviews whose widgets vanished, and clear
                        // an omnibox edit whose widget is gone.
                        slate_app::webview::gc(
                            &view.canvas.widgets.keys().cloned().collect(),
                        );
                        if let Some(editing) = &view.browser_editing {
                            if !view.canvas.widgets.contains_key(editing) {
                                view.browser_editing = None;
                                view.browser_address.clear();
                                view.browser_cursor = 0;
                                view.browser_sel = None;
                            }
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
                            // The shell's `initialCommandVerdict`: a PTY
                            // that was already running when the widget
                            // connected and is owned by a full-screen app
                            // (alternate buffer or mouse reporting armed)
                            // must not be typed into — the command counts
                            // as delivered because the agent it would start
                            // is the thing already running.
                            if pending.live
                                && view
                                    .manager
                                    .snapshot(&pending.terminal_id)
                                    .map(|s| s.alternate || s.mouse_reporting)
                                    .unwrap_or(false)
                            {
                                continue;
                            }
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

    /// `makeLocalId` — `{kind}-{crypto.randomUUID()}` in the renderer, so a
    /// disposed widget's id never recycles into a second journal history.
    fn fresh_id(&mut self, kind: &str) -> String {
        format!("{kind}-{}", uuid::Uuid::new_v4())
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
    /// Every commit runs the command flow's gate chain — the renderer's
    /// `send` called `flow.submit`, so heartbeats, lock conflicts,
    /// `baseVersion`, and the intent→commit/abort pair all apply here too.
    fn commit_as(&mut self, actor: &str, entry_type: &str, target: &str, payload: Value) {
        let _ = self.commit_work(actor, entry_type, target, payload, || Ok(json!({})));
    }

    /// `commit_as` with the handler's work between the intent and the
    /// commit — the file-pane mutations run their filesystem op inside the
    /// gates so a failure lands `intent + abort` like the original's apply
    /// throw, and a successful op still carries the journal's version. The
    /// closure answers the apply's `CommandError('failed', …)` as `Err`.
    fn commit_work(
        &mut self,
        actor: &str,
        entry_type: &str,
        target: &str,
        payload: Value,
        work: impl FnOnce() -> Result<Value, slate_app::http::Response>,
    ) -> Result<slate_app::journal::JournalEntry, slate_app::http::Response> {
        if !self.suppress_history
            && (entry_type.starts_with("widget.") || entry_type.starts_with("canvas."))
        {
            self.record_history();
        }
        let base_version = self.commit_base_version(entry_type, target);
        let result = gui_submit(
            &self.manager,
            self.workspace_dir.as_deref(),
            actor,
            base_version,
            entry_type,
            target,
            payload,
            work,
        );
        match result {
            Ok((entry, _)) => {
                self.adopt_committed_version(&entry);
                Ok(entry)
            }
            Err(response) => {
                let message = response
                    .body
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("submit failed")
                    .to_owned();
                self.notice(format!("journal {entry_type}: {message}"));
                Err(response)
            }
        }
    }

    /// The `baseVersion` the renderer sent — what it believed the target's
    /// version to be from its own folded state, not the journal's freshest
    /// read. A commit the mtime watcher has not refolded yet then reads as
    /// a genuine conflict, exactly like a stale renderer did. Only the
    /// commands whose IPC carried a version get one; the rest submit
    /// ungated, matching their `send` calls.
    fn commit_base_version(&self, entry_type: &str, target: &str) -> Option<f64> {
        let parsed = slate_app::resources::parse_resource(target)?;
        match (entry_type, parsed.scheme.as_str()) {
            ("widget.update", "widget") => self
                .canvas
                .widgets
                .get(parsed.id.as_str())
                .map(|widget| widget.version),
            ("plan.update" | "plan.toggle", "plan") => {
                let dir = slate_app::ipc::user_data_dir();
                let document = slate_app::planner_document::PlannerDocument::open(
                    dir.join("workspace-planner.json"),
                )
                .ok()?;
                let log = JournalLog::open(journal_path()).ok()?;
                let items = document
                    .folded_items(&log, &slate_app::planner::today_local())
                    .ok()?;
                items.get(parsed.id.as_str()).map(|item| item.version)
            }
            _ => None,
        }
    }

    /// After a commit lands, the local widget's version must match what the
    /// fold will assign — otherwise the next `widget.update` submits the
    /// stale version and conflicts on the commit it just wrote. (The
    /// planner document refolds from the journal on every read, so plan
    /// items never need this.)
    fn adopt_committed_version(&mut self, entry: &slate_app::journal::JournalEntry) {
        if entry.entry_type != "widget.update" && entry.entry_type != "widget.create" {
            return;
        }
        let Some(parsed) = slate_app::resources::parse_resource(&entry.target) else {
            return;
        };
        if parsed.scheme != "widget" {
            return;
        }
        let version = entry.version.unwrap_or(0) as f64;
        if let Some(widget) = self.canvas.widgets.get_mut(parsed.id.as_str()) {
            widget.version = version;
        }
    }

    /// The shell's mount effect: a terminal with a recorded
    /// `orcspace-launched-agent` key re-types that agent's command once per
    /// app run, through the same deferred-write queue the command bar uses.
    /// `live` is the spawn's adoption flag — the shell's
    /// `initialCommandVerdict` gate checks it before typing.
    fn relaunch_agent(&mut self, id: &str, live: bool) {
        if self.agent_relaunched.contains(id) {
            return;
        }
        let Some(agent_id) = self.agent_picks.launched.get(id).cloned() else {
            return;
        };
                if let Some((_, _, command)) = agent_launchers(&self.prefs)
            .iter()
            .find(|(aid, _, _)| *aid == *agent_id)
        {
            self.agent_relaunched.insert(id.to_owned());
            self.pending_commands.push(PendingCommand {
                terminal_id: id.to_owned(),
                command: command.clone(),
                attempts: 0,
                watch: true,
                live,
            });
        }
    }

    /// herdr's `detect_state` per live terminal — the manifest is chosen
    /// by the code session's `agent_id`, else the terminal's named-agent
    /// match; `detect` reads the snapshot's parsed screen tail.
    fn refresh_agent_states(&mut self) {
        if self.agent_scan_at.elapsed() < Duration::from_millis(400) {
            return;
        }
        self.agent_scan_at = Instant::now();
        let names = self.manager.names();
        let mut out = HashMap::new();
        for snap in &self.snapshots {
            let hint = self
                .code_sessions
                .iter()
                .find(|s| s.terminal_id == snap.id)
                .map(|s| s.agent_id.clone())
                .or_else(|| {
                    names
                        .get(&snap.id)
                        .and_then(|n| slate_app::agent_state::manifest_for("", n))
                });
            out.insert(
                snap.id.clone(),
                slate_app::agent_state::detect(&snap.screen, "", hint.as_deref()).state,
            );
        }
        self.agent_states = out;
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

    /// Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y — the shell's `orcspace:canvas-undo`
    /// and `-redo` events. The popped state journals as `canvas.restore` so
    /// the fold replays the undo exactly like it replays everything else.
    fn undo(&mut self, redo: bool) {
        let (from, to) = if redo {
            (&mut self.redo_stack, &mut self.undo_stack)
        } else {
            (&mut self.undo_stack, &mut self.redo_stack)
        };
        let Some(mut restored) = from.pop() else { return };
        // The shell's history snapshots never included the camera — undo
        // walks widgets/strokes/connections, the view stays put.
        restored.camera = self.canvas.camera.clone();
        to.push(self.canvas.clone());
        let mut payload = slate_app::canvas_store::canvas_to_value(&restored, 0);
        // canvas.restore ignores snapshotSeq — it serializes with the
        // snapshot so the same shape serves both writers.
        payload.as_object_mut().map(|map| map.remove("snapshotSeq"));
        self.suppress_history = true;
        self.commit("canvas.restore", "canvas:main", payload);
        self.suppress_history = false;
        self.canvas = restored;
    }

    /// Buttons in the TitleBar (`views_titlebar::titlebar_pane`) — window
    /// controls ride gpui's Window; the rest flip view state.
    pub(crate) fn titlebar_command(
        &mut self,
        action: &str,
        _payload: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            "settings" => {
                self.settings_open = !self.settings_open;
                if self.settings_open {
                    // A modal opening dismisses the floating menus — the
                    // backdrop eats the outside click that would.
                    self.arrange_menu = false;
                }
            }
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
            // The Arrange `LayoutGrid` button — opens the mode menu that
            // used to ride the status bar chip; the popover anchors under
            // the bar, the shell's `top-[36px]` drop.
            "arrange" => {
                self.arrange_menu = !self.arrange_menu;
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

    /// The shell's `settings:clear-background` — nulls BOTH keys (the port's
    /// inlined `background` data URL and the Electron `backgroundImage`
    /// path) and removes the managed `backgrounds/` dir.
    fn clear_wallpaper(&mut self) {
        self.settings.set_extra("background", Value::Null);
        self.settings.set_extra("backgroundImage", Value::Null);
        self.wallpaper_resolved = None;
        let dir = slate_app::ipc::user_data_dir().join("backgrounds");
        if dir.is_dir() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.settings.save();
    }

    /// The wallpaper as a data URL for the render path. `background` holds a
    /// port-written inlined URL; `backgroundImage` holds the Electron-style
    /// managed *path*, resolved once and memoized on the raw string.
    fn wallpaper_url(&mut self) -> Option<String> {
        if let Some(raw) = self
            .settings
            .extra("backgroundImage")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
        {
            if raw.starts_with("data:") {
                return Some(raw);
            }
            if let Some((key, url)) = &self.wallpaper_resolved {
                if *key == raw {
                    return Some(url.clone());
                }
            }
            if let Some(url) = encode_wallpaper_path(&raw) {
                self.wallpaper_resolved = Some((raw, url.clone()));
                return Some(url);
            }
        }
        self.settings
            .extra("background")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
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
            // The settings context's pickBackground/clearBackground — mirrors
            // `ipc/settings.ts`: the pick copies the image into
            // `userData/backgrounds/` and stores the *path* under
            // `extra["backgroundImage"]`; the canvas resolves it to a data
            // URL lazily (`wallpaper_url`) so settings.json stays small.
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
                            // isLocalPath — UNC / network paths refused.
                            let raw = path.as_os_str().to_string_lossy();
                            if !path.is_absolute()
                                || raw.starts_with("\\\\")
                                || raw.starts_with("//")
                            {
                                return;
                            }
                            let Ok(meta) = std::fs::metadata(path) else {
                                return;
                            };
                            if !meta.is_file() || meta.len() > MAX_WALLPAPER_BYTES {
                                return;
                            }
                            let ext = path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("")
                                .to_ascii_lowercase();
                            let safe_ext =
                                if WALLPAPER_EXTENSIONS.contains(&ext.as_str()) {
                                    ext
                                } else {
                                    "png".to_owned()
                                };
                            let dir =
                                slate_app::ipc::user_data_dir().join("backgrounds");
                            if std::fs::create_dir_all(&dir).is_err() {
                                return;
                            }
                            let millis = SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0);
                            let target = dir.join(format!("background-{millis}.{safe_ext}"));
                            // Copy the replacement in first — a failed copy
                            // leaves the existing background untouched.
                            if std::fs::copy(path, &target).is_err() {
                                return;
                            }
                            if let Ok(entries) = std::fs::read_dir(&dir) {
                                for entry in entries.flatten() {
                                    if entry.path() != target {
                                        let _ = std::fs::remove_file(entry.path());
                                    }
                                }
                            }
                            let stored = target.to_string_lossy().into_owned();
                            let _ = this.update(cx, |this, cx| {
                                this.settings
                                    .set_extra("backgroundImage", Value::from(stored));
                                // Drop any stale inlined data URL so the new
                                // pick isn't shadowed by an older `background`.
                                this.settings.set_extra("background", Value::Null);
                                this.wallpaper_resolved = None;
                                this.settings.save();
                                cx.notify();
                            });
                        }
                    }
                })
                .detach();
            }
            "background_clear" => self.clear_wallpaper(),
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
                                if self.settings.set_background_blur(v.min(90)) {
                                    self.settings.save();
                                }
                            }
                        }
                        "background" | "backgroundImage" => {
                            // Set via pick_background; null here = Remove.
                            if value == Some(Value::Null) {
                                self.clear_wallpaper();
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
            // The account footer's gear — same modal the titlebar's
            // "settings" action toggles.
            "settings" => {
                self.settings_open = !self.settings_open;
                if self.settings_open {
                    self.arrange_menu = false;
                }
            }
            "focus" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    self.active = Some(id.to_owned());
                    // (view is pinned — no canvas to reveal)
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
                // The journal entry needs no planner widget on the canvas —
                // commit `plan.toggle` directly so the sidebar preview's
                // checkboxes work with or without the pane open.
                if let (Some(item), Some(done)) = (
                    payload.get("id").and_then(Value::as_str),
                    payload.get("done").and_then(Value::as_bool),
                ) {
                    self.commit(
                        "plan.toggle",
                        &format!("plan:{item}"),
                        json!({ "done": done }),
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

    /// The workspace swap shared by the toolbar's pick and the Overview's
    /// recents: persist the dir, update `workspace_dir` (a stale field
    /// refolds the old canvas on the next journal-mtime check), and
    /// hydrate the new workspace's canvas — snapshot + journal tail are
    /// per-dir, like the shell's `onDirChange` reload.
    fn switch_workspace(&mut self, dir: String) {
        self.code_resume_dir = None;
        self.code_conversations.clear();
        self.code_resume_selected.clear();
        self.code_resume_dismissed.clear();
        let mut state = slate_app::workspace::WorkspaceState::load();
        if !state.set_workspace_dir(Some(dir.clone())) {
            return;
        }
        state.save();
        self.workspace_dir = Some(dir);
        let journal_path = slate_app::ipc::user_data_dir().join("command-journal.ndjson");
        if let Ok(log) = slate_app::journal_log::JournalLog::open(&journal_path) {
            self.canvas = slate_app::canvas_store::load_with_tail(
                self.workspace_dir.as_deref(),
                &log,
            )
            .0;
            // The shell reset `cascadeRef` on hydrate — a fresh canvas
            // starts the adoption grid over.
            self.cascade_count = 0;
        }
    }

    /// The shell's `defaultSize(kind, vw, vh, zoom)`: the catalog size
    /// shrinks to the visible world (64px/192px chrome reserves), clamped at
    /// the 280×160 minimum — small windows never spawn oversized widgets.
    fn spawn_size(&self, kind: &str) -> (f64, f64) {
        let (dw, dh) = default_size(kind);
        if self.viewport.x <= 0.0 {
            return (dw, dh);
        }
        // Panes have no zoom — sizes compute against the plain viewport.
        let c = self.viewport.x - 64.0;
        let i = self.viewport.y - 192.0;
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
            self.notice("Pane limit reached — close a widget before adding another.");
            return;
        }
        // `addWidget` anchored spawns at `point - 16` in world units. The
        // journaled position is invisible in pane mode — it only feeds the
        // Overview minimap — so stagger each spawn a notch to keep the rects
        // distinct there.
        let n = self.cascade_count;
        self.cascade_count += 1;
        let at = Pos2::new(
            at.x - 16.0 + (n % 6) as f32 * 28.0,
            at.y - 16.0 + (n / 6 % 6) as f32 * 28.0,
        );
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

    /// The shell's canvas `onDrop`: every droppable file imports into the
    /// media store and lands at the pointer's world point in a 28px cascade
    /// (`CASCADE_PX`, counted over successfully placed files). The classified
    /// kind picks the widget — `image` for image types, `browser` for the
    /// other viewable kinds, which carry the media reference in their
    /// journaled `state.media` (the port of the shell's `orcspace:open-media`
    /// delivery persisted as `orcspace-browser-media:<id>` in localStorage).
    /// A file with no viewer — `doc`, the shell's open-externally fallback —
    /// gets the refusal notice instead.
    fn handle_file_drop(&mut self, paths: &ExternalPaths, cx: &mut Context<Self>) {
        let point = self
            .drop_point
            .take()
            .unwrap_or_else(|| Pos2::new(self.viewport.x / 2.0, self.viewport.y / 2.0));
        let view_now = canvas::View::new(&self.canvas.camera, Pos2::ZERO);
        let drop = view_now.to_world(point);
        let media_dir = slate_app::ipc::user_data_dir().join("media");
        // The shell's `shortName`: notices never carry a name past 80 chars.
        let short_name = |name: &str| -> String {
            if name.chars().count() > 80 {
                format!("{}…", name.chars().take(77).collect::<String>())
            } else {
                name.to_owned()
            }
        };
        let mut placed = 0usize;
        for source in paths.paths() {
            if self.canvas.widgets.len() >= projection::MAX_WIDGETS {
                self.notice("Pane limit reached — close a widget before adding another.");
                continue;
            }
            let name = source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_owned());
            let media_kind = slate_app::attachments::classify_media(source);
            let Some(widget_kind) = media_kind.widget_kind() else {
                self.notice(format!(
                    "can't open \"{}\" — that file type isn't viewable",
                    short_name(&name)
                ));
                continue;
            };
            let imported = match slate_app::attachments::import_media(source, &media_dir) {
                Ok(imported) => imported,
                Err(error) => {
                    self.notice(format!("Failed to open file: {error}"));
                    continue;
                }
            };
            let (w, h) = self.spawn_size(widget_kind);
            // `placeWidget` → `addWidget`: the requested point is clamped
            // into the visible world first, then stored at `point - 16`.
            let mut at = Pos2::new(
                drop.x + placed as f32 * 28.0,
                drop.y + placed as f32 * 28.0,
            );
            if self.viewport.x > 0.0 {
                at = canvas::clamp_to_visible_world(&self.canvas.camera, self.viewport, at, w, h);
            }
            let at = Pos2::new(at.x - 16.0, at.y - 16.0);
            let z = self
                .canvas
                .widgets
                .values()
                .map(|widget| widget.z)
                .fold(0.0, f64::max)
                + 1.0;
            let id = self.fresh_id(widget_kind);
            let store_name = imported
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            let image_path = (widget_kind == "image")
                .then(|| imported.to_string_lossy().into_owned());
            // For a browser widget the media reference rides in `state.media`
            // — `{mediaUrl, path, name, kind}`, the shape the shell persisted
            // under `orcspace-browser-media:<id>` after the open-media event.
            let state = (widget_kind == "browser").then(|| {
                json!({
                    "media": {
                        "mediaUrl": format!("orc://media/{store_name}"),
                        "path": imported.to_string_lossy(),
                        "name": name,
                        "kind": media_kind.name(),
                    }
                })
            });
            let mut payload = json!({
                "id": id, "title": name, "kind": widget_kind,
                "x": at.x as f64, "y": at.y as f64,
                "w": w, "h": h, "z": z, "maximized": false,
            });
            if let Some(path) = &image_path {
                payload["imagePath"] = json!(path);
                // `imageName` is the media-store name (`<sha256>.<ext>`), the
                // same value `POST /canvas/image` records — the original file
                // name already rides as the widget title.
                payload["imageName"] = json!(store_name);
            }
            if let Some(state) = &state {
                payload["state"] = state.clone();
            }
            let image_name = image_path.as_ref().map(|_| store_name.clone());
            self.commit("widget.create", &format!("widget:{id}"), payload);
            self.canvas.widgets.insert(
                id.clone(),
                Widget {
                    id,
                    title: name.clone(),
                    kind: Some(widget_kind.to_owned()),
                    x: at.x as f64,
                    y: at.y as f64,
                    w,
                    h,
                    z,
                    maximized: false,
                    image_path,
                    image_name,
                    state,
                    version: 1.0,
                    updated_at: 0.0,
                },
            );
            placed += 1;
            self.notice(format!("Opened \"{}\"", short_name(&name)));
        }
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
        let (cols, rows) = terminal_cells(w, h, 1.0);
        // The mount journaled `terminal.spawn` (the `terminal:create` IPC
        // submitted it) — a user-spawned shell goes through the same gates.
        let _ = mount_spawn(
            &self.manager,
            self.workspace_dir.as_deref(),
            &id,
            cols,
            rows,
            None,
            Some(&title),
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
                    // A fresh id can never adopt a live PTY.
                    live: false,
                });
            }
            return;
        }
        let id = self.fresh_id(kind);
        self.create_widget_at(&id, kind, &kind_title(kind), at);
        self.active = Some(id);
        cx.notify();
    }


    fn close_widget(&mut self, id: &str, cx: &mut Context<Self>) {
        let is_terminal = self
            .canvas
            .widgets
            .get(id)
            .is_some_and(|widget| is_terminal_kind(widget.kind.as_deref()));
        self.canvas.widgets.shift_remove(id);
        self.canvas
            .connections
            .retain(|c| c.from != id && c.to != id);
        let manager = self.manager.clone();
        let dispose_id = id.to_owned();
        // The apply disposed the shell and failed its live dispatches inside
        // the command — post-intent — so they ride in `commit_work` rather
        // than before the journal entry.
        let _ = self.commit_work(
            "user",
            "widget.remove",
            &format!("widget:{id}"),
            json!({}),
            move || {
                if is_terminal && manager.snapshot(&dispose_id).is_ok() {
                    let _ = manager.dispose(&dispose_id);
                }
                // `failTerminalDispatches` — a running dispatch on a dead
                // shell cannot report, so it settles failed.
                let path = slate_app::ipc::user_data_dir().join("orchestration.json");
                let mut store = load_orchestration_store();
                let live: Vec<String> = store
                    .list_dispatches(None, None, Some(&dispose_id))
                    .iter()
                    .filter(|dispatch| dispatch.state == "running")
                    .map(|dispatch| dispatch.id.clone())
                    .collect();
                if !live.is_empty() {
                    let now = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map_or(0, |elapsed| elapsed.as_millis() as i64);
                    for dispatch_id in &live {
                        let _ = store.settle_dispatch(dispatch_id, "failed", None, now);
                    }
                    let _ = crate::engine::persist_orchestration(&path, &store);
                }
                Ok(json!({ "id": dispose_id }))
            },
        );
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
        self.term_resize_pending.remove(id);
        self.startup_watch.remove(id);
        self.startup_notice.remove(id);
        // Browser widgets own a native webview — it dies with the widget.
        // A no-op for every other kind (no key registered).
        slate_app::webview::remove_widget(id);
        if self.browser_editing.as_deref() == Some(id) {
            self.browser_editing = None;
            self.browser_address.clear();
            self.browser_cursor = 0;
            self.browser_sel = None;
        }
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

    /// Which widget fills the work area: the active widget, or the topmost
    /// one when nothing is focused. `z` is the pane's position in the strip.
    fn shown_widget_id(&self) -> Option<String> {
        if let Some(id) = self
            .active
            .as_deref()
            .filter(|id| self.canvas.widgets.contains_key(*id))
        {
            return Some(id.to_owned());
        }
        self.canvas
            .widgets
            .values()
            .max_by(|a, b| a.z.total_cmp(&b.z))
            .map(|w| w.id.clone())
    }


    /// The command bar's `activeTerminalId`: the configured target while
    /// that terminal still lives, else the topmost one — the shell's
    /// `terminals[0]?.id` fallback on its z-descending list.
    fn command_target_terminal(&self) -> Option<String> {
        if let Some(target) = self
            .command_target
            .as_deref()
            .filter(|id| self.snapshots.iter().any(|s| s.id == *id))
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
    fn adopt_kind(&mut self, kind: &str) {
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

    /// The `/agent` verb — `agent <name> <command…>` saves a launcher chip
    /// (`UiPreferences::custom_agents`, persisted to `native-ui.json`),
    /// `agent rm <name>` drops one, a bare `agent` prints usage. T3 Code's
    /// "Other" provider slot, kept at PTY fidelity.
    fn add_agent_line(&mut self, buffer: &str) {
        let mut parts = buffer.split_whitespace();
        let _verb = parts.next();
        let rest: Vec<&str> = parts.collect();
        match rest.as_slice() {
            ["rm", name] | ["remove", name] => {
                let before = self.prefs.custom_agents.len();
                self.prefs
                    .custom_agents
                    .retain(|a| a.id != *name && a.label != *name);
                if self.prefs.custom_agents.len() == before {
                    self.notice(format!("no custom agent `{name}`"));
                } else {
                    let _ = self
                        .prefs
                        .save(&slate_app::ui_preferences::UiPreferences::path());
                    self.notice(format!("agent removed: {name}"));
                }
            }
            [name, command @ ..] if !command.is_empty() => {
                let slug: String = name
                    .to_lowercase()
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            c
                        } else {
                            '-'
                        }
                    })
                    .collect();
                let id = format!("custom-{slug}");
                let command = command.join(" ");
                self.prefs.custom_agents.retain(|a| a.id != id);
                self.prefs
                    .custom_agents
                    .push(slate_app::ui_preferences::CustomAgent {
                        id,
                        label: (*name).to_owned(),
                        command,
                    });
                let _ = self
                    .prefs
                    .save(&slate_app::ui_preferences::UiPreferences::path());
                self.notice(format!("agent added: {name}"));
            }
            _ => self.notice("usage: /agent <name> <command> — /agent rm <name> removes"),
        }
    }

    fn adopt_suggestion(&mut self) {
        let suggestions = self.command_suggestions();
        let Some((kind, _, _)) = suggestions
            .get(self.command_suggestion.min(suggestions.len().saturating_sub(1)))
        else {
            return;
        };
        self.adopt_kind(&kind.clone());
    }

    /// The summon-only command bar's two click verbs: `↵` is Enter's
    /// `submitCommand`, `suggest:<kind>` a listbox row's adopt.
    pub(crate) fn commandbar_command(&mut self, action: &str, cx: &mut Context<Self>) {
        if action == "submit" {
            self.submit_command_bar(self.viewport, cx);
        } else if let Some(kind) = action.strip_prefix("suggest:") {
            self.adopt_kind(kind);
        }
        cx.notify();
    }

    /// `submitCommand` — the form's Enter/↵ path, shared by the keypress
    /// and the toolbar's submit button. A highlighted suggestion whose
    /// name isn't yet the typed token is adopted, not run.
    fn submit_command_bar(&mut self, viewport: Vec2, cx: &mut Context<Self>) {
        let typed = self
            .command_bar
            .as_deref()
            .and_then(|b| {
                b.split_whitespace()
                    .next()
                    // The shell's `first.slice(1)` — exactly one sigil
                    // strips; `//term` stays `/term` and misses.
                    .map(|t| {
                        t.strip_prefix(|c: char| "/.@".contains(c))
                            .unwrap_or(t)
                            .to_lowercase()
                    })
            })
            .unwrap_or_default();
        let suggestions = self.command_suggestions();
        let picked = suggestions
            .get(self.command_suggestion.min(suggestions.len().saturating_sub(1)))
            .map(|(kind, _, _)| kind.as_str());
        if picked.is_some_and(|kind| kind != typed) {
            self.adopt_suggestion();
            cx.notify();
            return;
        }
        let buffer = self.command_bar.take().unwrap_or_default();
        // `if (!value) return` — an empty submit kept the input and its
        // text; reopen with the buffer untouched.
        if buffer.trim().is_empty() {
            self.command_bar = Some(buffer);
            cx.notify();
            return;
        }
        // `/agent <name> <command>` — the launcher's "+ Other" flow. Runs
        // in command mode only; message mode keeps treating it as text.
        if !self.command_message {
            let first = buffer
                .split_whitespace()
                .next()
                .unwrap_or("");
            let verb = first
                .strip_prefix(|c: char| "/.@".contains(c))
                .unwrap_or(first);
            if verb.eq_ignore_ascii_case("agent") {
                self.add_agent_line(&buffer);
                self.command_bar = Some(String::new());
                self.command_suggestion = 0;
                cx.notify();
                return;
            }
        }
        let target = self.command_target_terminal();
        if self.command_message {
            // Message mode writes the text verbatim — no trailing `\r`
            // (`mode === 'message' ? command : …`). Message mode stayed
            // engaged after a send until Escape — a chat flow kept sending
            // messages without re-toggling per line.
            if let Some(target) = target.as_deref() {
                let _ = self.manager.write_text(target, &buffer, false);
            }
        } else if let Some((kind, initial)) =
            parse_widget_invocation(&buffer, self.settings.command_prefix())
        {
            self.create_widget_from_command(kind, &initial, viewport, cx);
        } else if let Some(target) = target.as_deref() {
            // A non-invocation line in command mode is terminal input —
            // the original's `${command}\r` write.
            let _ = self.manager.write_text(target, &buffer, true);
        } else {
            // Nothing to receive it — a miss keeps the bar open so a
            // typo can be fixed, like the disabled submit.
            self.command_bar = Some(buffer);
            cx.notify();
            return;
        }
        // `submitCommand` cleared the input but kept it mounted and
        // focused — the bar stayed up for the next line. Same here: reopen
        // empty rather than vanish.
        self.command_bar = Some(String::new());
        self.command_suggestion = 0;
        cx.notify();
    }

    /// Transient toast — the shell's `notice` state: cap hits, control-route
    /// failures, save errors. Auto-clears after four seconds of display.
    fn notice(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now() + Duration::from_secs(4)));
    }

    /// `PlannerStore.nextOrder` against the folded planner state — snapshot
    /// items plus the journal tail, read-only. `None` when the store can't
    /// be read; the fold then falls back to the payload's own `order`.
    fn planner_next_order(&self, day: Option<&str>, today: &slate_app::planner::Today) -> Option<f64> {
        let dir = slate_app::ipc::user_data_dir();
        let document =
            slate_app::planner_document::PlannerDocument::open(dir.join("workspace-planner.json"))
                .ok()?;
        // Open the journal fresh — commits land through the submit path,
        // which writes via its own handles, so a cached handle would fold a
        // stale tail forever.
        let log = JournalLog::open(journal_path()).ok()?;
        let items = document.folded_items(&log, today).ok()?;
        Some(slate_app::planner::next_order(items.values(), day))
    }

    /// The files pane's jail root — the selected workspace dir like the
    /// Electron `ws()`, falling back to the process cwd when none is set.
    fn files_root(&self) -> std::path::PathBuf {
        self.workspace_dir
            .as_deref()
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }

    /// `resolveInWorkspaceSync` — canonicalize `raw` and keep it only inside
    /// the files root. A symlink pointing out resolves outside and denies.
    /// For paths that do not exist yet (create/mkdir) the nearest existing
    /// ancestor is canonicalized and the tail re-appended before the check,
    /// so `root/new-file` passes but `root/link-to-etc/passwd` denies.
    fn file_jail(&self, raw: &str) -> Option<std::path::PathBuf> {
        let path = std::path::PathBuf::from(raw.trim());
        if raw.trim().is_empty() || !path.is_absolute() {
            return None;
        }
        let root = self.files_root();
        let root_real = root.canonicalize().ok()?;
        if let Ok(canon) = path.canonicalize() {
            return canon.starts_with(&root_real).then_some(canon);
        }
        // Walk up to the nearest existing ancestor for not-yet-created paths.
        let mut cursor = path.clone();
        let mut parts: Vec<std::ffi::OsString> = Vec::new();
        loop {
            let parent = cursor.parent()?.to_path_buf();
            if let Some(name) = cursor.file_name() {
                parts.push(name.to_os_string());
            }
            cursor = parent;
            if let Ok(real) = cursor.canonicalize() {
                let mut candidate = real;
                for part in parts.iter().rev() {
                    candidate.push(part);
                }
                return candidate.starts_with(&root_real).then_some(candidate);
            }
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
                // `createItem` stamped `order: nextOrder(day)` — max order
                // among same-day items + 1.
                let today = slate_app::planner::today_local();
                let day = slate_app::planner::safe_day(payload.get("day"), &today);
                if let Some(order) = self.planner_next_order(day.as_deref(), &today) {
                    payload["order"] = json!(order);
                }
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
                    self.orc_send_ask(ask_id, body);
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
                    self.orc_reply(ask_id, body, subject);
                }
            }
            Some("orc_gate") => {
                if let (Some(gate), Some(resolution)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("resolution").and_then(Value::as_str),
                ) {
                    self.orc_post(
                        "POST",
                        &format!("/orchestration/gates/{gate}/resolve"),
                        Some(json!({ "resolution": resolution })),
                    );
                }
            }
            Some("orc_ack") => {
                if let Some(message) = action.get("id").and_then(Value::as_str) {
                    self.orc_post(
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
                    self.orc_post(
                        "POST",
                        &format!("/orchestration/dispatches/{dispatch}/account"),
                        Some(json!({ "state": state })),
                    );
                }
            }
            // fix-orchestration: a field click arms its keyboard owner; the
            // draft is created on first keystroke, so a click alone stores
            // nothing.
            Some("orc_edit") => {
                if let Some(id) = action.get("id").and_then(Value::as_str) {
                    self.orc_editing = Some(match action.get("kind").and_then(Value::as_str) {
                        Some("note") => crate::views_orchestration::OrcEdit::Note(id.to_owned()),
                        _ => crate::views_orchestration::OrcEdit::Reply(id.to_owned()),
                    });
                }
            }
            Some("orc_dismiss_error") => {
                self.orc_error = None;
            }
            // Files pane ops — pure filesystem work the pane itself can't
            // own; confirmed twice on the pane side before this runs. Every
            // path passes `file_jail` — the port of the shell's
            // `resolveInWorkspaceSync` — so a forged or tampered action can't
            // touch anything outside the workspace.
            Some("file_delete") => {
                if let Some(raw) = action.get("path").and_then(Value::as_str) {
                    // `lstat` the raw path first — a symlink is refused
                    // outright, never followed to its target (the shell's
                    // `file.delete` rule). The jail check then covers the
                    // resolved path for non-symlinks.
                    match std::fs::symlink_metadata(raw) {
                        Ok(meta) if meta.file_type().is_symlink() => {
                            self.notice(format!("refusing to delete a symlink: {raw}"));
                        }
                        _ => match self.file_jail(raw) {
                            None => {
                                self.notice("delete refused — path escapes workspace")
                            }
                            // `file.delete` on `file:{path}` — the removal
                            // runs inside the gate chain like the original's
                            // apply, so a failed delete journals
                            // intent+abort rather than landing silently.
                            Some(path) => {
                                let path_str = path.to_string_lossy().into_owned();
                                let for_work = path.clone();
                                let _ = self.commit_work(
                                    "user",
                                    "file.delete",
                                    &slate_app::resources::file_resource(&path_str),
                                    json!({ "path": path_str }),
                                    move || {
                                        // The apply's lstat: a missing path is
                                        // `not_found`, a symlink is refused
                                        // before `is_dir()` can follow it.
                                        let meta = match std::fs::symlink_metadata(&for_work) {
                                            Ok(meta) => meta,
                                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                                return Err(command_failed(
                                                    slate_app::command::ErrorCode::NotFound,
                                                    "Target does not exist",
                                                ));
                                            }
                                            Err(e) => {
                                                return Err(command_failed(
                                                    slate_app::command::ErrorCode::Failed,
                                                    e.to_string(),
                                                ));
                                            }
                                        };
                                        if meta.file_type().is_symlink() {
                                            return Err(command_failed(
                                                slate_app::command::ErrorCode::Invalid,
                                                "refusing to delete a symlink",
                                            ));
                                        }
                                        let result = if meta.is_dir() {
                                            std::fs::remove_dir_all(&for_work)
                                        } else {
                                            std::fs::remove_file(&for_work)
                                        };
                                        result
                                            .map(|_| json!({ "ok": true }))
                                            .map_err(|e| {
                                                command_failed(
                                                    slate_app::command::ErrorCode::Failed,
                                                    e.to_string(),
                                                )
                                            })
                                    },
                                );
                            }
                        },
                    }
                }
            }
            Some("file_new") => {
                if let (Some(dir), Some(kind)) = (
                    action.get("dir").and_then(Value::as_str),
                    action.get("kind").and_then(Value::as_str),
                ) {
                    match self.file_jail(dir) {
                        None => self.notice("create refused — path escapes workspace"),
                        Some(dir) => {
                            // `untitled`, `untitled-1`, … — the shell's
                            // placeholder names; rename happens after the
                            // row exists. Picking the name is not the
                            // mutation, so it happens before the intent.
                            let mut candidate = dir.join("untitled");
                            for index in 1usize.. {
                                if !candidate.exists() {
                                    break;
                                }
                                candidate = dir.join(format!("untitled-{index}"));
                            }
                            // `file.create`/`file.mkdir` on `file:{path}` —
                            // the 'wx'-style exclusive create runs inside
                            // the gate chain like the original's apply.
                            let entry_type = if kind == "dir" {
                                "file.mkdir"
                            } else {
                                "file.create"
                            };
                            let path_str = candidate.to_string_lossy().into_owned();
                            let for_work = candidate.clone();
                            let is_dir = kind == "dir";
                            let committed = self.commit_work(
                                "user",
                                entry_type,
                                &slate_app::resources::file_resource(&path_str),
                                json!({ "path": path_str }),
                                move || {
                                    // `file.mkdir` created the parents
                                    // recursively, then the leaf atomically;
                                    // `file.create` opened 'wx' — EEXIST is
                                    // its own message either way.
                                    let result = if is_dir {
                                        if let Some(parent) = for_work.parent() {
                                            let _ = std::fs::create_dir_all(parent);
                                        }
                                        std::fs::create_dir(&for_work).map_err(|e| {
                                            if e.kind() == std::io::ErrorKind::AlreadyExists {
                                                "Folder already exists".to_owned()
                                            } else {
                                                e.to_string()
                                            }
                                        })
                                    } else {
                                        std::fs::OpenOptions::new()
                                            .write(true)
                                            .create_new(true)
                                            .open(&for_work)
                                            .map(|_| ())
                                            .map_err(|e| {
                                                if e.kind() == std::io::ErrorKind::AlreadyExists {
                                                    "File already exists".to_owned()
                                                } else {
                                                    e.to_string()
                                                }
                                            })
                                    };
                                    result
                                        .map(|_| json!({ "ok": true }))
                                        .map_err(|message| {
                                            command_failed(
                                                slate_app::command::ErrorCode::Failed,
                                                message,
                                            )
                                        })
                                },
                            );
                            if committed.is_ok() {
                                // fix-files: the create → rename flow —
                                // the fresh `untitled` row opens its
                                // inline rename immediately, seeded
                                // with the placeholder name.
                                self.files_editing =
                                    Some(crate::views_files::FilesEditing::Rename {
                                        id: id.to_owned(),
                                        path: candidate.to_string_lossy().into_owned(),
                                        buffer: candidate
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                    });
                            }
                        }
                    }
                }
            }
            // fix-files: `fs:rename` — the ✎ Save / Enter path. `name`
            // follows the original's `joinChildPath` rules (a single
            // component: no separators, never an embedded `..`), and the
            // destination stays in the renamed entry's own directory —
            // the shell joined it to `currentPath`, which is the same
            // parent a listing row lives in.
            Some("file_rename") => {
                if let (Some(raw), Some(name)) = (
                    action.get("path").and_then(Value::as_str),
                    action.get("name").and_then(Value::as_str),
                ) {
                    let name = name.trim();
                    if name.is_empty()
                        || name == "."
                        || name == ".."
                        || name.contains(['/', '\\'])
                        || name.contains("..")
                    {
                        self.notice("Name cannot contain path separators or \"..\"");
                    } else {
                        match self.file_jail(raw) {
                            None => self.notice("rename refused — path escapes workspace"),
                            Some(src) => {
                                let target = src
                                    .parent()
                                    .map(|parent| parent.join(name))
                                    .and_then(|t| self.file_jail(&t.to_string_lossy()));
                                match target {
                                    None => self
                                        .notice("rename refused — path escapes workspace"),
                                    // `file.rename` on `file:{src}` —
                                    // `{path: src, to: destination}` like
                                    // `sendFileCommand` sent.
                                    Some(target) => {
                                        let src_str = src.to_string_lossy().into_owned();
                                        let target_str = target.to_string_lossy().into_owned();
                                        let for_work = (src.clone(), target.clone());
                                        let committed = self.commit_work(
                                            "user",
                                            "file.rename",
                                            &slate_app::resources::file_resource(&src_str),
                                            json!({ "path": src_str, "to": target_str }),
                                            move || {
                                                // The apply's guards: a
                                                // missing source is
                                                // `not_found`, an existing
                                                // destination is refused —
                                                // `fs::rename` overwrites.
                                                if !for_work.0.exists() {
                                                    return Err(command_failed(
                                                        slate_app::command::ErrorCode::NotFound,
                                                        "Source file does not exist",
                                                    ));
                                                }
                                                if for_work.1.exists() {
                                                    return Err(command_failed(
                                                        slate_app::command::ErrorCode::Failed,
                                                        "Target file name already exists",
                                                    ));
                                                }
                                                std::fs::rename(&for_work.0, &for_work.1)
                                                    .map(|_| json!({ "ok": true }))
                                                    .map_err(|e| {
                                                        command_failed(
                                                            slate_app::command::ErrorCode::Failed,
                                                            e.to_string(),
                                                        )
                                                    })
                                            },
                                        );
                                        if committed.is_ok() {
                                            self.notice(format!("Renamed to \"{name}\""));
                                            // The original re-viewed an
                                            // open preview at the new
                                            // path.
                                            let previewed = matches!(
                                                &self.files_preview,
                                                Some(p) if p.id == id
                                                    && p.path == src.to_string_lossy()
                                            );
                                            if previewed {
                                                match crate::views_files::preview_load(&target)
                                                {
                                                    Ok((body, size)) => {
                                                        self.files_preview = Some(
                                                            crate::views_files::FilesPreview {
                                                                id: id.to_owned(),
                                                                path: target
                                                                    .to_string_lossy()
                                                                    .into_owned(),
                                                                name: name.to_owned(),
                                                                size,
                                                                body,
                                                            },
                                                        );
                                                    }
                                                    Err(error) => {
                                                        self.files_preview = None;
                                                        self.notice(format!(
                                                            "Cannot preview: {error}"
                                                        ));
                                                    }
                                                }
                                            }
                                            // A commit lands here from
                                            // the Save button too —
                                            // release the field either
                                            // way.
                                            if matches!(
                                                &self.files_editing,
                                                Some(crate::views_files::FilesEditing::Rename {
                                                    id: editing,
                                                    ..
                                                }) if editing == id
                                            ) {
                                                self.files_editing = None;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Some("file_open") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    match self.file_jail(path) {
                        None => self.notice("open refused — path escapes workspace"),
                        Some(path) => {
                            // fs:open-path's extension blocklists, checked
                            // over the requested name AND the realpath so a
                            // `notes.txt` symlink to `payload.sh` is caught
                            // from either side.
                            const EXECUTABLE: &[&str] = &[
                                "exe", "bat", "cmd", "com", "scr", "msi", "ps1", "vbs", "vbe",
                                "js", "mjs", "cjs", "jse", "wsf", "wsh", "jar", "sh", "bash",
                                "lnk", "reg", "py", "pyw", "iso", "vhd", "vhdx", "hta", "msc",
                                "cpl", "url", "inf", "pif", "appref-ms", "wsb", "mof", "gadget",
                            ];
                            const ACTIVE_MARKUP: &[&str] =
                                &["html", "htm", "xhtml", "shtml", "svg", "mhtml", "xht"];
                            let trimmed = path.with_file_name(
                                path.file_name()
                                    .map(|n| {
                                        n.to_string_lossy()
                                            .trim_end_matches(['.', ' '])
                                            .to_owned()
                                    })
                                    .unwrap_or_default(),
                            );
                            let mut candidates = vec![path.clone(), trimmed];
                            if let Ok(real) = path.canonicalize() {
                                candidates.push(real);
                            }
                            let ext_of = |p: &std::path::Path| {
                                p.extension()
                                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
                                    .unwrap_or_default()
                            };
                            if candidates
                                .iter()
                                .any(|p| EXECUTABLE.contains(&ext_of(p).as_str()))
                            {
                                self.notice("Executable files cannot be opened from here");
                            } else if candidates
                                .iter()
                                .any(|p| ACTIVE_MARKUP.contains(&ext_of(p).as_str()))
                            {
                                self.notice("Web pages cannot be opened in your browser from here — open it in the editor instead");
                            } else if let Err(error) =
                                std::process::Command::new("xdg-open").arg(&path).spawn()
                            {
                                self.notice(format!("open {}: {error}", path.display()));
                            }
                        }
                    }
                }
            }
            Some("file_reveal") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    match self.file_jail(path) {
                        None => self.notice("reveal refused — path escapes workspace"),
                        Some(path) => {
                            // showItemInFolder — the file manager on the
                            // file's parent, or the dir itself.
                            let dir = if path.is_dir() {
                                path.as_path()
                            } else {
                                path.parent().unwrap_or(path.as_path())
                            };
                            if let Err(error) =
                                std::process::Command::new("xdg-open").arg(dir).spawn()
                            {
                                self.notice(format!("reveal {}: {error}", dir.display()));
                            }
                        }
                    }
                }
            }
            Some("file_copy_path") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.to_owned()));
                }
            }
            // fix-files: `viewFile` — the bounded `fs:read-file` behind the
            // preview overlay. Errors surface as the pane's `actionNotice`
            // ("Cannot preview: …"), a success replaces `files_preview` and
            // takes the keyboard like the original's modal.
            Some("file_preview") => {
                if let Some(raw) = action.get("path").and_then(Value::as_str) {
                    match self.file_jail(raw) {
                        None => self.notice("preview refused — path escapes workspace"),
                        Some(path) => match crate::views_files::preview_load(&path) {
                            Err(error) => {
                                self.notice(format!("Cannot preview: {error}"));
                            }
                            Ok((body, size)) => {
                                // The dialog traps focus — an open field
                                // edit releases first.
                                self.files_editing = None;
                                self.files_preview =
                                    Some(crate::views_files::FilesPreview {
                                        id: id.to_owned(),
                                        name: path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default(),
                                        path: path.to_string_lossy().into_owned(),
                                        size,
                                        body,
                                    });
                            }
                        },
                    }
                }
            }
            // fix-files: `closePreview` — the overlay's ✕; Escape routes
            // through `on_key` instead.
            Some("file_preview_close") => {
                if matches!(&self.files_preview, Some(p) if p.id == id) {
                    self.files_preview = None;
                }
            }
            // fix-files: the dialog's "Copy Text" — clipboard straight
            // from the loaded preview, so megabytes never round-trip
            // through the action JSON.
            Some("file_copy_content") => {
                if let Some(preview) = &self.files_preview {
                    if preview.id == id {
                        if let crate::views_files::FilesPreviewBody::Text(text) =
                            &preview.body
                        {
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                text.clone(),
                            ));
                        }
                    }
                }
            }
            // Browser pane ops — the omnibox submit and the chrome buttons.
            // `state.url` is journaled so a restart reopens the same page;
            // the wry element gets the matching message on its own thread.
            Some("browser_navigate") => {
                if let Some(raw) = action.get("url").and_then(Value::as_str) {
                    match slate_app::browser::to_navigation_url(raw) {
                        Some(url) => {
                            // `navigate()` also ran `setMedia(null)` — a
                            // submit leaves the media viewer for the page.
                            self.widget_command(
                                id,
                                json!({ "op": "set", "state": { "url": url, "media": Value::Null } }),
                                cx,
                            );
                            slate_app::webview::navigate(id, &url);
                            slate_app::webview::set_error(id, None);
                        }
                        None => {
                            slate_app::webview::set_error(
                                id,
                                Some(format!("invalid address: {raw}")),
                            );
                        }
                    }
                }
            }
            Some("browser_nav") => match action.get("dir").and_then(Value::as_str) {
                Some("back") => slate_app::webview::go_back(id),
                Some("forward") => slate_app::webview::go_forward(id),
                Some("reload") => slate_app::webview::reload(id),
                Some("stop") => slate_app::webview::stop(id),
                _ => {}
            },
            // The error banner's Home — back to the landing URL. The pane
            // stops mounting the element there, so drop the native view.
            // The original also cleared the retry counter and any media.
            Some("browser_home") => {
                self.widget_command(
                    id,
                    json!({ "op": "set", "state": { "url": slate_app::browser::HOME_URL, "media": Value::Null, "retry": 0 } }),
                    cx,
                );
                slate_app::webview::remove_widget(id);
            }
            // The error banner's Retry — `retryCountRef.current < 3` gates
            // the button; the count is journaled so the cap survives a
            // refold, and `Loaded` resets it after a clean load.
            Some("browser_retry") => {
                let retries = self
                    .canvas
                    .widgets
                    .get(id)
                    .and_then(|widget| widget.state.as_ref())
                    .and_then(|state| state.get("retry"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.widget_command(
                    id,
                    json!({ "op": "set", "state": { "retry": retries + 1 } }),
                    cx,
                );
                slate_app::webview::set_error(id, None);
                slate_app::webview::reload(id);
            }
            // MediaViewer's close — `setMedia(null)`. The journaled
            // `state.media` clears, `widget_url` falls back to the page URL,
            // and the element's `maybe_sync_url` loads it on the next paint.
            Some("browser_media_close") => {
                self.widget_command(
                    id,
                    json!({ "op": "set", "state": { "media": Value::Null } }),
                    cx,
                );
                slate_app::webview::set_error(id, None);
            }
            // The doc card's Open — `window.api.fs.openPath(path)` on the
            // stored path (the media store sits outside the files jail, so
            // no jail check — the shell's had none either).
            Some("browser_media_open") => {
                if let Some(path) = action.get("path").and_then(Value::as_str) {
                    if let Err(error) =
                        std::process::Command::new("xdg-open").arg(path).spawn()
                    {
                        self.notice(format!("open {path}: {error}"));
                    }
                }
            }
            // Omnibox focus tracking — carried as a pane op so the buffer
            // lives on CanvasView next to `rename_buffer`. Entering edit
            // selects the whole value, the DOM input's `e.target.select()`.
            Some("browser_edit") => {
                if action.get("editing").and_then(Value::as_bool).unwrap_or(false) {
                    self.browser_editing = Some(id.to_owned());
                    self.browser_cursor = self.browser_address.len();
                    self.browser_sel = Some(0);
                } else if self.browser_editing.as_deref() == Some(id) {
                    self.browser_editing = None;
                    self.browser_address.clear();
                    self.browser_cursor = 0;
                    self.browser_sel = None;
                }
            }
            // fix-panes: the notes pane's writes. Each is a REST call to `/notes`,
            // which journals the matching `note.*` entry and then writes the
            // store — so the pane, the CLI and REST all fold the same way.
            Some("note_create") => {
                if let Some(note) = action.get("id").and_then(Value::as_str) {
                    self.control_post(
                        "POST",
                        "/notes",
                        Some(json!({ "id": note, "title": "New note" })),
                    );
                    self.widget_command(
                        id,
                        json!({ "op": "set", "state": { "expanded": note } }),
                        cx,
                    );
                }
            }
            Some("note_edit") => {
                if let (Some(note), Some(field)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("field").and_then(Value::as_str),
                ) {
                    self.notes_editing = Some((note.to_owned(), field.to_owned()));
                    self.notes_buffer = action
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                }
            }
            Some("note_update") => {
                if let Some(note) = action.get("id").and_then(Value::as_str) {
                    let patch = action.get("patch").cloned().unwrap_or_else(|| json!({}));
                    self.control_post("PATCH", &format!("/notes/{note}"), Some(patch));
                }
            }
            // Reorder swaps `order` with the neighbour: the mover takes the
            // neighbour's slot and the neighbour takes the mover's.
            Some("note_move") => {
                if let (Some(note), Some(order), Some(other), Some(other_order)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("order").and_then(Value::as_f64),
                    action.get("otherId").and_then(Value::as_str),
                    action.get("otherOrder").and_then(Value::as_f64),
                ) {
                    self.control_post(
                        "PATCH",
                        &format!("/notes/{note}"),
                        Some(json!({ "order": order })),
                    );
                    self.control_post(
                        "PATCH",
                        &format!("/notes/{other}"),
                        Some(json!({ "order": other_order })),
                    );
                }
            }
            Some("note_recolor") => {
                if let (Some(category), Some(color)) = (
                    action.get("category").and_then(Value::as_str),
                    action.get("color").and_then(Value::as_str),
                ) {
                    self.control_post(
                        "POST",
                        "/notes/recolor",
                        Some(json!({ "category": category, "color": color })),
                    );
                }
            }
            Some("note_delete") => {
                if let Some(note) = action.get("id").and_then(Value::as_str) {
                    if matches!(&self.notes_editing, Some((editing, _)) if editing == note) {
                        self.notes_editing = None;
                        self.notes_buffer.clear();
                    }
                    self.control_post("DELETE", &format!("/notes/{note}"), None);
                    self.widget_command(
                        id,
                        json!({ "op": "set", "state": { "expanded": null } }),
                        cx,
                    );
                }
            }
            // fix-panes2: the links, timer and kanban panes' writes. Each is a
            // REST call that journals its entry and then applies it, the way
            // the notes ops above do.
            Some("link_add") => {
                // No text input on the canvas yet, so `+` adds the same stub
                // row the pane always did — now journaled as `link.create`.
                self.control_post(
                    "POST",
                    &format!("/links/{id}"),
                    Some(json!({ "title": "New link", "url": "https://example.com" })),
                );
            }
            Some("link_remove") => {
                if let Some(link) = action.get("id").and_then(Value::as_str) {
                    self.control_post("DELETE", &format!("/links/{id}/{link}"), None);
                }
            }
            Some("timer_toggle") => {
                self.control_post("POST", &format!("/timers/{id}/toggle"), None);
            }
            Some("timer_reset") => {
                self.control_post("POST", &format!("/timers/{id}/reset"), None);
            }
            Some("kanban_move") => {
                if let (Some(item), Some(status)) = (
                    action.get("id").and_then(Value::as_str),
                    action.get("status").and_then(Value::as_str),
                ) {
                    self.control_post(
                        "PATCH",
                        &format!("/planner/{item}"),
                        Some(json!({ "status": status })),
                    );
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


    fn widget_kind(&self, id: &str) -> Option<String> {
        self.canvas
            .widgets
            .get(id)
            .and_then(|widget| widget.kind.clone())
    }

    /// The keyboard's "is a terminal" test — a canvas widget of a
    /// terminal kind, or a session PTY (a manager terminal that owns no
    /// widget).
    fn terminal_targetable(&self, id: &str) -> bool {
        is_terminal_kind(self.widget_kind(id).as_deref())
            || self
                .code_sessions
                .iter()
                .any(|session| session.terminal_id == id)
    }


    /// The Code grid's top-left in window space — the cover's offset plus
    /// the pane's own chrome (title strip, then the launcher row).
    fn code_grid_offset(&self) -> Pos2 {
        let rail_w = if self.prefs.sidebar_open {
            theme::geometry::SIDEBAR_EXPANDED
        } else {
            theme::geometry::RAIL_WIDTH
        };
        let chrome = crate::views_code::HEADER_H
            + if self.code_launcher_open {
                crate::views_code::LAUNCHER_H
            } else {
                0.0
            };
        Pos2::new(rail_w, canvas::TITLE_BAR_HEIGHT + chrome)
    }

    /// The card rects `code_pane` lays out, recomputed for hit-testing —
    /// window-space pointer → `(session index, grid-local rect)`.
    fn code_hit(&self, pointer: Pos2) -> Option<(usize, canvas::Rect)> {
        if self.code_sessions.is_empty() {
            return None;
        }
        let offset = self.code_grid_offset();
        let width = (self.viewport.x - offset.x).max(0.0);
        let height = (self.viewport.y - offset.y).max(0.0);
        let focus = self
            .code_sessions
            .iter()
            .position(|s| Some(&s.id) == self.code_focused.as_ref())
            .unwrap_or(0);
        let area = canvas::Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height));
        let rects = slate_app::code_layout::arrange(
            self.code_layout,
            area,
            self.code_sessions.len(),
            focus,
        );
        let gx = pointer.x - offset.x;
        let gy = pointer.y - offset.y;
        rects.iter().enumerate().find_map(|(index, rect)| {
            (gx >= rect.left() && gx <= rect.right() && gy >= rect.top() && gy <= rect.bottom())
                .then_some((index, *rect))
        })
    }

    /// Window-space pointer → the `(col, row)` inside a session card's
    /// body. Border 1px + the 24px card header + the body's 4px pad come
    /// off first — same accounting `terminal_cell` runs for widgets.
    fn code_cell(rect: &canvas::Rect, offset: Pos2, pointer: Pos2) -> (u16, u16) {
        let col = ((pointer.x - offset.x - rect.left() - 5.0) / CELL_W)
            .floor()
            .max(0.0) as u16;
        let row = ((pointer.y - offset.y - rect.top() - crate::views_code::CARD_HEADER_H - 5.0)
            / CELL_H)
            .floor()
            .max(0.0) as u16;
        (col, row)
    }

    /// A press inside a session card's body — the card's focus plus the
    /// terminal semantics a canvas widget body has: xterm clears the
    /// selection on press, mouse-reporting programs get the button event,
    /// and Shift bypasses reporting into a drag selection.
    pub(crate) fn code_body_down(
        &mut self,
        session_id: &str,
        button: MouseButton,
        pointer: Pos2,
        modifiers: gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.code_sessions.iter().position(|s| s.id == session_id) else {
            return;
        };
        let terminal_id = self.code_sessions[index].terminal_id.clone();
        self.code_focused = Some(session_id.to_owned());
        self.active = Some(terminal_id.clone());
        let _ = self.manager.focus_input(&terminal_id, true);
        let Some((_, rect)) = self.code_hit(pointer).filter(|(i, _)| *i == index) else {
            return;
        };
        let offset = self.code_grid_offset();
        // A press on the card header focuses but never selects or reports —
        // same split as a widget's header vs body.
        if pointer.y - offset.y - rect.top() <= crate::views_code::CARD_HEADER_H {
            return;
        }
        let (col, row) = Self::code_cell(&rect, offset, pointer);
        let term_button = match button {
            MouseButton::Middle => TermMouseButton::Middle,
            MouseButton::Right => TermMouseButton::Secondary,
            _ => TermMouseButton::Primary,
        };
        let reporting = self
            .snapshots
            .iter()
            .find(|s| s.id == terminal_id)
            .map(|s| s.mouse_reporting)
            .unwrap_or(false);
        let mods = KeyMods {
            shift: modifiers.shift,
            alt: modifiers.alt,
            ctrl: modifiers.control,
            mac_cmd: modifiers.platform,
        };
        match button {
            MouseButton::Left => {
                self.manager.selection_clear(&terminal_id);
                if reporting && !modifiers.shift {
                    let _ = self.manager.mouse_input(
                        &terminal_id,
                        MouseKind::Press(TermMouseButton::Primary),
                        col,
                        row,
                        mods,
                    );
                } else {
                    let _ = self.manager.selection_begin(&terminal_id, col, row);
                    self.selection_drag = Some(terminal_id.clone());
                    self.refresh_selection(&terminal_id);
                }
            }
            _ => {
                if reporting {
                    let _ = self.manager.mouse_input(
                        &terminal_id,
                        MouseKind::Press(term_button),
                        col,
                        row,
                        mods,
                    );
                }
            }
        }
        cx.notify();
    }

    /// The Code view's verbs — `views_code::code_pane` dispatches here
    /// (see its module doc for the payload contract).
    pub(crate) fn code_command(
        &mut self,
        action: &str,
        payload: Value,
        cx: &mut Context<Self>,
    ) {
        match action {
            "launcher" => self.code_launcher_open = !self.code_launcher_open,
            "add_agent" => {
                // The "+ Other" chip — the command bar is the app's only
                // text input, so adding a CLI is a `/agent` line there.
                let prefix = self.settings.command_prefix();
                let sigil = if prefix == "any" { "/" } else { prefix };
                self.command_bar = Some(format!("{sigil}agent "));
            }
            "toggle_worktree" => self.code_worktree = !self.code_worktree,
            "pin_session" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    if let Some(session) =
                        self.code_sessions.iter_mut().find(|s| s.id == id)
                    {
                        session.pinned = !session.pinned;
                    }
                }
            }
            "settle_session" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    if let Some(session) =
                        self.code_sessions.iter_mut().find(|s| s.id == id)
                    {
                        session.settled = true;
                        session.pinned = false;
                        if self.code_focused.as_deref() == Some(id) {
                            self.code_focused = None;
                        }
                    }
                }
            }
            "restore_session" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    if let Some(session) =
                        self.code_sessions.iter_mut().find(|s| s.id == id)
                    {
                        session.settled = false;
                    }
                }
            }
            "layout" => {
                if let Ok(mode) = serde_json::from_value::<slate_app::code_layout::CodeLayout>(
                    payload.get("mode").cloned().unwrap_or(Value::Null),
                ) {
                    self.code_layout = mode;
                    // `orcspace-code-layout-mode` survived restarts — same
                    // here via `native-ui.json`.
                    self.prefs.layout = mode;
                    let _ = self
                        .prefs
                        .save(&slate_app::ui_preferences::UiPreferences::path());
                }
            }
            "launch" => {
                let agent = payload
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("custom");
                let command = payload
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // `browser` opened a webview widget, not a PTY — point at
                // the canvas widget instead. `custom` (empty command)
                // launches a bare shell.
                if agent == "browser" {
                    self.notice("browser isn't a terminal — spawn the browser widget");
                } else {
                    self.launch_code_session(agent, command, None, self.code_worktree, cx);
                }
            }
            "pick_folder" => {
                // The empty state's "Change" — `workspace.pickDir` in the
                // original: a directory chooser adopted on pick.
                let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("Workspace folder".into()),
                });
                cx.spawn(async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = picked.await {
                        if let Some(path) = paths.first() {
                            let dir = path.to_string_lossy().into_owned();
                            let _ = this.update(cx, |this, cx| {
                                this.switch_workspace(dir.clone());
                                this.notice(format!("workspace → {dir}"));
                                cx.notify();
                            });
                        }
                    }
                })
                .detach();
            }
            "toggle_agent" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    let id = id.to_owned();
                    if let Some(at) = self.code_selected_agents.iter().position(|a| *a == id) {
                        self.code_selected_agents.remove(at);
                        if self.code_active_agent.as_deref() == Some(id.as_str()) {
                            self.code_active_agent = self.code_selected_agents.first().cloned();
                        }
                    } else {
                        self.code_selected_agents.push(id.clone());
                        self.code_active_agent = Some(id);
                    }
                }
            }
            "set_count" => {
                if let (Some(id), Some(count)) = (
                    self.code_active_agent.clone(),
                    payload.get("count").and_then(Value::as_u64),
                ) {
                    self.code_agent_counts.insert(id, count.max(1) as u32);
                }
            }
            "launch_selected" => {
                // `launchWorkspace` — each picked agent at its own count,
                // fan-out in one click.
                let picks = self.code_selected_agents.clone();
                for id in picks {
                    let count = self.code_agent_counts.get(&id).copied().unwrap_or(1);
                    let command = crate::views_code::CODE_AGENTS
                        .iter()
                        .find(|a| a.id == id)
                        .map(|a| a.command.to_owned())
                        .or_else(|| {
                            self.prefs
                                .custom_agents
                                .iter()
                                .find(|a| a.id == id)
                                .map(|a| a.command.clone())
                        });
                    let Some(command) = command else {
                        continue;
                    };
                    for _ in 0..count {
                        self.launch_code_session(&id, &command, None, self.code_worktree, cx);
                    }
                }
                self.code_selected_agents.clear();
                self.code_agent_counts.clear();
                self.code_active_agent = None;
            }
            "resume_toggle" => {
                if let Some(key) = payload.get("key").and_then(Value::as_str) {
                    if !self.code_resume_selected.remove(key) {
                        self.code_resume_selected.insert(key.to_owned());
                    }
                }
            }
            "resume_select_all" => {
                // Ticking rows that are folded away would be a count with
                // nothing behind it — expand so the ticks are visible.
                let dismissed = &self.code_resume_dismissed;
                let all_selected = self
                    .code_conversations
                    .iter()
                    .filter(|c| !dismissed.contains(&slate_app::code_resume::conversation_key(c.agent_id, &c.id)))
                    .all(|c| self.code_resume_selected.contains(&slate_app::code_resume::conversation_key(c.agent_id, &c.id)));
                if all_selected {
                    self.code_resume_selected.clear();
                } else {
                    self.code_resume_selected = self
                        .code_conversations
                        .iter()
                        .filter(|c| !dismissed.contains(&slate_app::code_resume::conversation_key(c.agent_id, &c.id)))
                        .map(|c| slate_app::code_resume::conversation_key(c.agent_id, &c.id))
                        .collect();
                    if self.code_resume_selected.len() > slate_app::code_resume::COLLAPSED_ROWS {
                        self.code_resume_expanded = true;
                    }
                }
            }
            "resume_expand" => self.code_resume_expanded = !self.code_resume_expanded,
            "resume_dismiss" => {
                if let Some(key) = payload.get("key").and_then(Value::as_str) {
                    self.code_resume_dismissed.insert(key.to_owned());
                    self.code_resume_selected.remove(key);
                    self.save_resume_dismissed();
                }
            }
            "resume_dismiss_all" => {
                self.code_resume_dismissed = self
                    .code_conversations
                    .iter()
                    .map(|c| slate_app::code_resume::conversation_key(c.agent_id, &c.id))
                    .collect();
                self.code_resume_selected.clear();
                self.save_resume_dismissed();
            }
            "resume_restore_all" => {
                self.code_resume_dismissed.clear();
                self.save_resume_dismissed();
            }
            "resume_refresh" => {
                self.code_resume_dir = None;
            }
            "resume_copy" => {
                if let Some(command) = payload.get("command").and_then(Value::as_str) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(command.to_owned()));
                }
            }
            "resume" => {
                // `resumeConversation` — the same agent started with its
                // own resume command, the conversation title as the card's.
                let agent = payload
                    .get("agent")
                    .and_then(Value::as_str)
                    .unwrap_or("custom");
                let command = payload
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let title = payload
                    .get("title")
                    .and_then(Value::as_str)
                    .map(|t| t.trim().chars().take(80).collect::<String>())
                    .filter(|t| !t.is_empty());
                if !command.is_empty() {
                    self.launch_code_session(agent, command, title, false, cx);
                }
            }
            "resume_all" => {
                // The bulk button: a ticked selection resumes verbatim, an
                // empty one takes the newest `RESUME_ALL_MAX`.
                let visible: Vec<_> = self
                    .code_conversations
                    .iter()
                    .filter(|c| {
                        !self.code_resume_dismissed.contains(
                            &slate_app::code_resume::conversation_key(c.agent_id, &c.id),
                        )
                    })
                    .cloned()
                    .collect();
                let remaining = 32usize.saturating_sub(self.code_sessions.len());
                for conversation in slate_app::code_resume::conversations_to_resume(
                    &visible,
                    &self.code_resume_selected,
                    remaining,
                ) {
                    let title = {
                        let t = conversation.title.trim().chars().take(80).collect::<String>();
                        if t.is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    };
                    self.launch_code_session(conversation.agent_id, &conversation.command, title, false, cx);
                }
                self.code_resume_selected.clear();
            }
            "focus_session" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    if let Some(session) =
                        self.code_sessions.iter().find(|s| s.id == id)
                    {
                        self.code_focused = Some(session.id.clone());
                        self.active = Some(session.terminal_id.clone());
                        let _ = self.manager.focus_input(&session.terminal_id, true);
                    }
                }
            }
            "close_session" => {
                if let Some(id) = payload.get("id").and_then(Value::as_str) {
                    if let Some(at) = self.code_sessions.iter().position(|s| s.id == id) {
                        let session = self.code_sessions.remove(at);
                        let _ = self.manager.dispose(&session.terminal_id);
                        // A worktree-backed session returns its tree and
                        // `slate/<id>` branch — `git worktree remove
                        // --force` matches t3code's discard semantics.
                        if let Some(tree) = &session.worktree {
                            if let Some(dir) = self.workspace_dir.clone() {
                                let branch = format!("slate/{}", session.id);
                                let _ = std::process::Command::new("git")
                                    .args(["-C", &dir, "worktree", "remove", "--force", tree])
                                    .output();
                                let _ = std::process::Command::new("git")
                                    .args(["-C", &dir, "branch", "-D", &branch])
                                    .output();
                            }
                        }
                        // Drop the session's bookkeeping too — a stale
                        // term_cells entry suppresses the next resize, and
                        // queued commands would write into nothing.
                        self.term_cells.remove(&session.terminal_id);
                        self.term_resize_pending.remove(&session.terminal_id);
                        self.pending_commands
                            .retain(|p| p.terminal_id != session.terminal_id);
                        if self.active.as_deref() == Some(session.terminal_id.as_str()) {
                            self.active = None;
                        }
                        if self.code_focused.as_deref() == Some(id) {
                            self.code_focused = None;
                        }
                        if self.code_sessions.is_empty() {
                            self.code_resume_dir = None;
                        }
                    }
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// `launchSessions` for one agent — a shell PTY on the manager with
    /// no canvas widget, the agent command queued for the first tick once
    /// the PTY reaches its read loop (the same `spawn + write_text` the
    /// canvas's agent menu runs).
    /// The resume scan's dismissal list, persisted for `workspace_dir` —
    /// the original's `writeDismissed` localStorage key.
    fn save_resume_dismissed(&self) {
        let Some(dir) = self.workspace_dir.clone() else { return };
        let live: std::collections::HashSet<String> = self
            .code_conversations
            .iter()
            .map(|c| slate_app::code_resume::conversation_key(c.agent_id, &c.id))
            .collect();
        slate_app::code_resume::save_dismissed(dir.as_str(), &self.code_resume_dismissed, &live);
    }

    /// Kick a background scan when the empty grid is showing — the
    /// `ResumeAgents` mount in the original. `code_resume_dir` arms once
    /// per folder; a refresh or a workspace switch re-arms it.
    fn maybe_scan_resume(&mut self, cx: &mut Context<Self>) {
        if !self.code_sessions.is_empty()
            || self.code_resume_loading
        {
            return;
        }
        let Some(dir) = self.workspace_dir.clone() else { return };
        if self.code_resume_dir.as_deref() == Some(dir.as_str()) {
            return;
        }
        self.code_resume_dir = Some(dir.clone());
        self.code_resume_loading = true;
        cx.spawn(async move |this, cx| {
            let scan = cx
                .background_executor()
                .spawn(async move { slate_app::code_resume::list(&dir, None, None) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.code_conversations = scan;
                this.code_resume_loading = false;
                // The folder's persisted dismissals join the fresh scan;
                // keys for conversations that no longer exist drop out.
                this.code_resume_dismissed = this
                    .workspace_dir
                    .as_deref()
                    .map(slate_app::code_resume::load_dismissed)
                    .unwrap_or_default();
                this.code_resume_selected.clear();
                cx.notify();
            });
        })
        .detach();
    }

    fn launch_code_session(
        &mut self,
        agent: &str,
        command: &str,
        title: Option<String>,
        worktree: bool,
        cx: &mut Context<Self>,
    ) {
        // `MAX_CODE_SESSIONS` in the shell — 32 cards was the hard cap.
        if self.code_sessions.len() >= 32 {
            self.notice("code session limit reached (32)");
            cx.notify();
            return;
        }
        self.counter += 1;
        let id = format!("code-{}", self.counter);
        let terminal_id = self.fresh_id("code-term");
        // t3code's "New worktree" — the session gets
        // `.slate-trees/<id>` on branch `slate/<id>` inside the workspace
        // repo, so parallel agents never touch the same checkout.
        let mut worktree_dir: Option<String> = None;
        if worktree {
            match self.workspace_dir.clone() {
                Some(dir) => {
                    let branch = format!("slate/{id}");
                    let tree = format!("{dir}/.slate-trees/{id}");
                    match std::process::Command::new("git")
                        .args(["-C", &dir, "worktree", "add", &tree, "-b", &branch])
                        .output()
                    {
                        Ok(out) if out.status.success() => {
                            worktree_dir = Some(tree);
                        }
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            self.notice(format!(
                                "worktree failed: {}",
                                stderr.lines().next().unwrap_or("unknown")
                            ));
                        }
                        Err(error) => self.notice(format!("worktree: {error}")),
                    }
                }
                None => self.notice("worktree needs a workspace folder"),
            }
        }
        let cwd = worktree_dir.clone().or_else(slate_app::workspace::ensure);
        let live = match mount_spawn(
            &self.manager,
            self.workspace_dir.as_deref(),
            &terminal_id,
            160,
            40,
            cwd,
            None,
        ) {
            Ok(live) => live,
            Err(error) => {
                self.notice(format!("code session spawn failed: {error}"));
                cx.notify();
                return;
            }
        };
        // `session.title || agent.label` — the header reads "Claude Code",
        // not `claude`; a custom launch is "Other CLI", not `custom`. A
        // resumed conversation wears its first-prompt title instead.
        let agent_label = crate::views_code::CODE_AGENTS
            .iter()
            .find(|a| a.id == agent)
            .map(|a| a.label)
            .or_else(|| {
                self.prefs
                    .custom_agents
                    .iter()
                    .find(|a| a.id == agent)
                    .map(|a| a.label.as_str())
            })
            .unwrap_or(agent)
            .to_owned();
        let label = title.unwrap_or_else(|| agent_label.clone());
        // The toolbar's terminal roster and `slate tell` resolve by name —
        // a session's PTY should answer to its agent label.
        self.manager.set_name(&terminal_id, &agent_label);
        self.code_sessions.push(crate::views_code::CodeSession {
            id: id.clone(),
            agent_id: agent.to_owned(),
            label,
            command: command.to_owned(),
            terminal_id: terminal_id.clone(),
            alive: true,
            pinned: false,
            settled: false,
            worktree: worktree_dir,
        });
        self.code_focused = Some(id);
        self.active = Some(terminal_id.clone());
        self.code_launcher_open = false;
        if !command.is_empty() {
            self.pending_commands.push(PendingCommand {
                terminal_id,
                command: command.to_owned(),
                attempts: 0,
                watch: true,
                live,
            });
        }
        cx.notify();
    }

    /// Keep each session PTY matched to its card — the code grid has no
    /// widget frames doing it, so the same `arrange` the pane runs sizes
    /// the PTYs here (resize-on-change through `term_cells`, like the
    /// widget path).
    fn resize_code_terminals(&mut self, viewport: Vec2) {
        let count = self.code_sessions.len();
        if count == 0 {
            return;
        }
        // `views_code`'s HEADER_H + LAUNCHER_H and the card's
        // 24px header — duplicated so the pane file stays standalone.
        let chrome = crate::views_code::HEADER_H
            + if self.code_launcher_open {
                crate::views_code::LAUNCHER_H
            } else {
                0.0
            };
        let height = (viewport.y - canvas::TITLE_BAR_HEIGHT - chrome).max(0.0);
        let area = canvas::Rect::from_min_size(
            canvas::Pos2::ZERO,
            canvas::Vec2::new(viewport.x, height),
        );
        let focus = self
            .code_focused
            .as_deref()
            .and_then(|id| self.code_sessions.iter().position(|s| s.id == id))
            .unwrap_or(0);
        let rects =
            slate_app::code_layout::arrange(self.code_layout, area, count, focus);
        // The card header takes 24px; the terminal body pads 4px a side
        // and its grid advances CELL_W×CELL_H px — the painter's metrics.
        // Collect first — the iterator borrows `code_sessions`, and the
        // debounced sync below needs `&mut self`.
        let pending: Vec<(String, u16, u16)> = self
            .code_sessions
            .iter()
            .zip(rects)
            .map(|(session, rect)| {
                let cols = ((rect.width() - 8.0) / CELL_W).floor().clamp(20.0, 500.0) as u16;
                let rows = ((rect.height() - 32.0) / CELL_H)
                    .floor()
                    .clamp(4.0, 300.0) as u16;
                (session.terminal_id.clone(), cols, rows)
            })
            .collect();
        for (id, cols, rows) in pending {
            self.sync_term_size(&id, cols, rows);
        }
    }

    /// The shell's 150ms resize debounce (the fit timer in TerminalWidget):
    /// cell-size changes coalesce and the PTY gets one fit+SIGWINCH after
    /// they settle, not one per rendered frame of a drag. A terminal's
    /// first fit applies immediately — a fresh mount should not wait.
    fn sync_term_size(&mut self, id: &str, cols: u16, rows: u16) {
        let target = (cols, rows);
        if self.term_cells.get(id) == Some(&target) {
            self.term_resize_pending.remove(id);
            return;
        }
        let pending = self
            .term_resize_pending
            .entry(id.to_owned())
            .or_insert((target, Instant::now()));
        if pending.0 != target {
            *pending = (target, Instant::now());
        }
        let settled = pending.1.elapsed() >= Duration::from_millis(150);
        if (!self.term_cells.contains_key(id) || settled)
            && self.manager.resize(id, cols, rows).is_ok()
        {
            self.term_cells.insert(id.to_owned(), target);
            self.term_resize_pending.remove(id);
        }
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

/// Cells a terminal body of this world size displays at this zoom: the
/// renderer scales the text with the frame, so the grid shrinks as you zoom
/// out — same contract the Electron app had with xterm + FitAddon.
/// CellRgb (r,g,b) packs into the u32 gpui's rgb() takes.
fn pack((r, g, b): slate_app::terminal_screen::CellRgb) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// The pane strip's height — the fixed tab row above the work area.
const PANE_STRIP_H: f32 = 30.0;

/// The work area's screen rect — beside the rail, under the title bar and
/// the pane strip. The shown widget fills it; there is no camera.
fn pane_rect(rail_w: f32, viewport: Vec2) -> canvas::Rect {
    canvas::Rect::from_min_size(
        Pos2::new(rail_w, canvas::TITLE_BAR_HEIGHT + PANE_STRIP_H),
        Vec2::new(
            (viewport.x - rail_w).max(0.0),
            (viewport.y - canvas::TITLE_BAR_HEIGHT - PANE_STRIP_H).max(0.0),
        ),
    )
}

fn terminal_cells(w: f64, h: f64, zoom: f64) -> (u16, u16) {
    // The same metrics `terminal_cell` (hit-test) and the painter use —
    // CELL_W advance / CELL_H row, 1px frame + 4px p_1 padding per side — so
    // the PTY grid fills the card the glyph layout actually draws into.
    let body_w = (w * zoom - 10.0).max(40.0) as f32;
    let body_h =
        (h * zoom - theme::geometry::HEADER_HEIGHT as f64 * zoom - 10.0).max(20.0) as f32;
    let cols = (body_w / (CELL_W * zoom as f32).max(2.0)).clamp(20.0, 500.0) as u16;
    let rows = (body_h / (CELL_H * zoom as f32).max(2.0)).clamp(4.0, 300.0) as u16;
    (cols, rows)
}

/// Pointer position → terminal cell. The rendered metrics, not the grid
/// sizing's guesses: rows stack CELL_H·zoom apart and JetBrains Mono
/// advances 0.6em (CELL_W·zoom at FONT_PX), so a click lands on the
/// cell the glyph actually occupies. The 4px body padding (p_1) and 1px
/// frame border come off the top-left first.
fn terminal_cell(rect: canvas::Rect, zoom: f32, pointer: Pos2) -> (u16, u16) {
    let zoom = zoom.max(0.1);
    let body_x = rect.min.x + 5.0;
    let body_y = rect.min.y + 1.0 + theme::geometry::HEADER_HEIGHT * zoom + 4.0;
    let col = ((pointer.x - body_x) / (CELL_W * zoom)).floor().max(0.0);
    let row = ((pointer.y - body_y) / (CELL_H * zoom)).floor().max(0.0);
    (col.min(999.0) as u16, row.min(999.0) as u16)
}

/// `media.IMAGE_EXTENSIONS` — the wallpaper picker's allowed extensions
/// (unknown ext → stored as `.png`).
const WALLPAPER_EXTENSIONS: [&str; 12] = [
    "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "svg", "ico", "tif", "tiff", "heic",
];
/// `media.MAX_MEDIA_BYTES` — the pick refuses anything over 256 MB.
const MAX_WALLPAPER_BYTES: u64 = 256 * 1024 * 1024;

/// `media.mimeTypeForPath` for the wallpaper read-back.
fn wallpaper_mime(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        _ => "application/octet-stream",
    }
}

/// Read a wallpaper file into a `data:` URL — the shell's
/// `backgroundDataUrl()` equivalent for `backgroundImage` paths.
fn encode_wallpaper_path(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_WALLPAPER_BYTES {
        return None;
    }
    use base64::Engine as _;
    Some(format!(
        "data:{};base64,{}",
        wallpaper_mime(path),
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
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

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// The shell's CSS `blur(Npx)` on the wallpaper, run once off the UI
/// thread — gpui's img() has no filter support, so hand it an
/// already-blurred JPEG instead. The work happens at (roughly) display
/// resolution under object-fit: cover — capped at 1600px so a 4K source
/// isn't blurred pixel-by-pixel when nothing finer survives anyway — so
/// `sigma` carries the CSS-pixel scale with it.
/// Returns the encoded JPEG bytes — `gpui::ImageSource` isn't Send, so
/// the caller wraps them on the UI thread.
fn blur_wallpaper(url: &str, blur: u64, viewport: Vec2) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:image/")?;
    let (_, payload) = rest.split_once(";base64,")?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let decoded = image::load_from_memory(&bytes).ok()?;
    let (iw, ih) = (decoded.width() as f32, decoded.height() as f32);
    if iw < 1.0 || ih < 1.0 {
        return None;
    }
    let cover = (viewport.x / iw).max(viewport.y / ih).max(0.0);
    let (disp_w, disp_h) = (iw * cover, ih * cover);
    let shrink = (1600.0 / disp_w).min(1600.0 / disp_h).min(1.0);
    let work_w = (disp_w * shrink).round().max(1.0) as u32;
    let work_h = (disp_h * shrink).round().max(1.0) as u32;
    let working = if work_w < decoded.width() || work_h < decoded.height() {
        decoded.resize(work_w, work_h, image::imageops::FilterType::Triangle)
    } else {
        decoded
    };
    // Chrome's blur(N) ≈ a Gaussian with std-dev ~N/2.
    let sigma = (blur as f32 / 2.0 * shrink).max(0.0);
    let blurred = if sigma > 0.0 {
        image::imageops::blur(&working.to_rgba8(), sigma)
    } else {
        working.to_rgba8()
    };
    let mut out = Vec::new();
    let rgb = image::DynamicImage::ImageRgba8(blurred).into_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85)
        .encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )
        .ok()?;
    Some(out)
}

#[allow(clippy::too_many_arguments)]
fn widget_body(
    widget: &Widget,
    snapshots: &[TerminalSnapshot],
    zoom: f32,
    terminals_flipped: bool,
    // The browser pane's slice of CanvasView state — `entity().read()`
    // mid-render would re-borrow the view and panic.
    browser_editing: Option<&str>,
    browser_address: &str,
    // fix-browser: the omnibox's caret and selection anchor — the DOM
    // input's `selectionStart`/`selectionEnd` pair for the buffer.
    browser_cursor: usize,
    browser_sel: Option<usize>,
    // The canvas region free of floating chrome — the X11 webview child is
    // not clipped by GPUI, so its bounds are intersected with this rect (and
    // the paint-time content mask) before they're handed to wry.
    browser_clip: Bounds<Pixels>,
    // The files pane's jail root — `CanvasView::files_root`, resolved by the
    // caller since this function can't borrow the view.
    files_root: &std::path::Path,
    // fix-files: the pane's keyboard/preview slice of CanvasView — carried
    // in for the same no-read-mid-render reason as the browser fields.
    files_editing: Option<&crate::views_files::FilesEditing>,
    files_preview: Option<&crate::views_files::FilesPreview>,
    // fix-planner: the planner pane's open field and create draft, carried in
    // like the files slice.
    planner_editing: Option<&crate::views_planner::PlannerEdit>,
    planner_draft: &crate::views_planner::PlannerDraft,
    // fix-orchestration: the pane's field/draft/error slice, carried in for
    // the same no-read-mid-render reason.
    orc: crate::views_orchestration::OrcState<'_>,
    // fix-panes: the notes pane's open field and its buffer.
    notes_editing: Option<(&str, &str)>,
    notes_buffer: &str,
    cx: &mut Context<CanvasView>,
) -> AnyElement {
    let font = px(FONT_PX * zoom);
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
                .font_family(FONT_FAMILY)
                .text_size(font)
                .line_height(gpui::px(CELL_H * zoom))
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
                    let mut line = div()
                        .h(px(CELL_H * zoom))
                        .flex()
                        .flex_row()
                        .flex_none();
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
            // The drag-selection highlight — xterm's selectionBackground,
            // hue-shifted to the sand accent so it sits in the palette.
            // Cells ride the body's p_1 padding, so
            // the overlay starts at the padding edge; the cell geometry is
            // `terminal_cell`'s inverse (CELL_W·zoom cols, CELL_H·zoom rows).
            let mut container = div().size_full().relative().child(body);
            if let Some(snapshot) = snapshot {
                if !snapshot.selection.is_empty() {
                    let cell_w = px(CELL_W * zoom);
                    let cell_h = px(CELL_H * zoom);
                    let mut overlay = div().absolute().left(px(4.0)).top(px(4.0));
                    for &(row, start, end) in &snapshot.selection {
                        overlay = overlay.child(
                            div()
                                .absolute()
                                .left(cell_w * start as f32)
                                .top(cell_h * row as f32)
                                .w(cell_w * (end - start) as f32)
                                .h(cell_h)
                                .bg(rgba(0xD4A37359)),
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
            .child(crate::views_planner::planner_pane(
                widget,
                planner_editing,
                planner_draft,
                cx,
            ))
            .into_any_element(),
        Some("files") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_files::files_pane(
                files_root,
                widget,
                files_editing,
                files_preview,
                cx,
            ))
            .into_any_element(),
        Some("orchestration") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_orchestration::orchestration_pane(
                &load_orchestration_store(),
                widget,
                orc,
                cx,
            ))
            .into_any_element(),
        Some("browser") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_browser::browser_pane(
                widget,
                zoom,
                browser_editing,
                browser_address,
                browser_cursor,
                browser_sel,
                browser_clip,
                cx,
            ))
            .into_any_element(),
        Some("notes") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_notes::notes_pane(widget, notes_editing, notes_buffer, cx))
            .into_any_element(),
        Some("timer") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_timer::timer_pane(&widget.id, cx))
            .into_any_element(),
        Some("sys-monitor") => div()
            .size_full()
            .text_size(font)
            .child(crate::views_sysmon::sysmon_pane(&widget.id, cx))
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
        self.viewport = viewport_size;
        // The work area: beside the rail, under the title bar and the pane
        // strip. Exactly one widget renders into it.
        let rail_w = if self.prefs.sidebar_open {
            theme::geometry::SIDEBAR_EXPANDED
        } else {
            theme::geometry::RAIL_WIDTH
        };
        let pane = pane_rect(rail_w, viewport_size);
        let shown_id = self.shown_widget_id();

        // The StartupWordmark intro: a particle overlay paints over
        // everything for ~9.2s, driven off this render clock. Any
        // deliberate input dismisses it (on_key plus the overlay's own
        // mouse listeners); prefers-reduced-motion jumps straight to the
        // end state, which in the original is an empty frame — gone.
        let mut splash_t = None;
        if let Some(started) = self.splash_started {
            if cx.reduce_motion() {
                self.splash_started = None;
            } else {
                let t = started.elapsed().as_secs_f32();
                // Done once the post-ready fade has run (the 200ms hold +
                // 900ms `.is-ready` transition, off `splash_ready_at`).
                // A fixed clock used to end this — now it ends whenever
                // real boot work plus the intro beat have both finished.
                let done = self
                    .splash_ready_at
                    .map(|at| at.elapsed().as_secs_f32() >= 1.2)
                    .unwrap_or(false);
                if !done {
                    splash_t = Some(t);
                    if self.splash_points.is_empty() {
                        self.splash_points =
                            Rc::new(splash_field(viewport_size.x, viewport_size.y));
                    }
                    window.request_animation_frame();
                } else {
                    self.splash_started = None;
                    self.splash_points = Rc::new(Vec::new());
                }
            }
        }

        // The shown terminal's PTY grid tracks the pane rect; hidden panes
        // keep their stored grid (and their PTY size) until shown again.
        let mut sizes: Vec<(String, u16, u16)> = Vec::new();
        for widget in self.canvas.widgets.values() {
            if is_terminal_kind(widget.kind.as_deref()) {
                let (w, h) = if shown_id.as_deref() == Some(widget.id.as_str())
                    || widget.maximized
                {
                    (pane.width() as f64, pane.height() as f64)
                } else {
                    (widget.w, widget.h)
                };
                let (cols, rows) = terminal_cells(w, h, 1.0);
                sizes.push((widget.id.clone(), cols, rows));
            }
        }
        for (id, cols, rows) in sizes {
            self.sync_term_size(&id, cols, rows);
        }
        let mut root = div()
            .id("slate-canvas")
            .size_full()
            .relative()
            // The UI face — graysite's Plus Jakarta Sans; descendants that
            // want mono override with "Departure Mono".
            .font_family("Plus Jakarta Sans")
            .overflow_hidden()
            .bg(rgb(theme::hex(theme::monochrome::BASE)))
            .key_context("canvas")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _window, cx| {
                this.on_key(event, viewport_size, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                    window.focus(&this.focus, cx);
                    // A click outside the input closes the command bar; the
                    // gesture itself still runs, like the shell's blur.
                    // The floating menus (arrange popover, agent menu)
                    // dismissed on an outside pointerdown too — leaving
                    // them up over the canvas was a trap.
                    this.command_bar = None;
                    this.agent_menu = None;
                    this.arrange_menu = false;
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
                    cx.notify();
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
                    // The menu opens on the pane background only — a click
                    // inside the shown widget stays with the widget.
                    let size = window.viewport_size();
                    let viewport = Vec2::new(f32::from(size.width), f32::from(size.height));
                    let rail_w = if this.prefs.sidebar_open {
                        theme::geometry::SIDEBAR_EXPANDED
                    } else {
                        theme::geometry::RAIL_WIDTH
                    };
                    let over_widget = this.shown_widget_id().is_some()
                        && pane_rect(rail_w, viewport).contains(pointer);
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
                    if this.canvas.widgets.contains_key(&sel_id) {
                        let size = _window.viewport_size();
                        let viewport = Vec2::new(f32::from(size.width), f32::from(size.height));
                        let rail_w = if this.prefs.sidebar_open {
                            theme::geometry::SIDEBAR_EXPANDED
                        } else {
                            theme::geometry::RAIL_WIDTH
                        };
                        let rect = pane_rect(rail_w, viewport);
                        let (col, row) = terminal_cell(rect, 1.0, pointer);
                        let _ = this.manager.selection_update(&sel_id, col, row);
                        this.refresh_selection(&sel_id);
                        cx.notify();
                    } else if let Some(index) = this
                        .code_sessions
                        .iter()
                        .position(|s| s.terminal_id == sel_id)
                    {
                        // A code session's drag tracks outside its card too —
                        // xterm keeps the selection running until mouseup.
                        let offset = this.code_grid_offset();
                        if let Some((_, rect)) = this
                            .code_hit(pointer)
                            .filter(|(i, _)| *i == index)
                            .or_else(|| {
                                // Outside every card: still update against
                                // the session's own rect so the drag keeps
                                // tracking like a widget body does.
                                let offset = this.code_grid_offset();
                                let width = (this.viewport.x - offset.x).max(0.0);
                                let height = (this.viewport.y - offset.y).max(0.0);
                                let focus = this
                                    .code_sessions
                                    .iter()
                                    .position(|s| Some(&s.id) == this.code_focused.as_ref())
                                    .unwrap_or(0);
                                slate_app::code_layout::arrange(
                                    this.code_layout,
                                    canvas::Rect::from_min_size(
                                        Pos2::ZERO,
                                        Vec2::new(width, height),
                                    ),
                                    this.code_sessions.len(),
                                    focus,
                                )
                                .into_iter()
                                .nth(index)
                                .map(|r| (index, r))
                            })
                        {
                            let (col, row) = Self::code_cell(&rect, offset, pointer);
                            let _ = this.manager.selection_update(&sel_id, col, row);
                            this.refresh_selection(&sel_id);
                            cx.notify();
                        }
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    if this.selection_drag.take().is_some() {
                        cx.notify();
                        return;
                    }
                    cx.notify();
                }),
            )
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                let pointer = pos(event.position);
                let size = window.viewport_size();
                let viewport = Vec2::new(f32::from(size.width), f32::from(size.height));
                // The shown pane owns the wheel: a terminal gets scrollback
                // or button-64/65 reports; anything else scrolls natively.
                let rail_w = if this.prefs.sidebar_open {
                    theme::geometry::SIDEBAR_EXPANDED
                } else {
                    theme::geometry::RAIL_WIDTH
                };
                let rect = pane_rect(rail_w, viewport);
                let Some(id) = this.shown_widget_id() else {
                    return;
                };
                if !rect.contains(pointer) {
                    return;
                }
                let Some(widget) = this.canvas.widgets.get(&id) else {
                    return;
                };
                if !is_terminal_kind(widget.kind.as_deref()) {
                    return;
                }
                let reporting = this
                    .snapshots
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.mouse_reporting)
                    .unwrap_or(false);
                if reporting {
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
                    let (col, row) = terminal_cell(rect, 1.0, pointer);
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
                        let _ = this.manager.mouse_input(&id, kind, col, row, mods);
                    }
                } else {
                    let mut pixels = match event.delta {
                        ScrollDelta::Pixels(p) => f32::from(p.y),
                        ScrollDelta::Lines(p) => p.y * CELL_H,
                    };
                    // `onWheel`'s fast-scroll: altKey ×4.
                    if event.modifiers.alt {
                        pixels *= 4.0;
                    }
                    let _ = this.manager.scroll_pixels(&id, pixels, CELL_H);
                }
                cx.notify();
            }));


        // The wallpaper — the shell's `.wallpaper-container`: a full-bleed
        // image under everything (settings `background` holds a data:image
        // URL; `backgroundImage` holds the Electron managed path, resolved
        // by `wallpaper_url`), blurred by `backgroundBlur` (the shell's CSS
        // `blur(Npx)` — done on the CPU off-thread, gpui has no image
        // filter) and dimmed by `backgroundDim`'s rgba(8,8,8,d) overlay.
        if let Some(url) = self.wallpaper_url() {
            let blur = self.settings.background_blur().min(90);
            let key = (hash_str(&url), blur);
            // The blurred variant, when this (url, blur) pair is ready.
            let mut image = self
                .wallpaper_cache
                .as_ref()
                .filter(|(h, b, _)| *h == key.0 && *b == key.1)
                .map(|(_, _, src)| src.clone());
            if blur > 0 && image.is_none() && self.wallpaper_blurring != Some(key) {
                self.wallpaper_blurring = Some(key);
                let url_for_job = url.clone();
                let viewport_for_job = viewport_size;
                cx.spawn(async move |this, cx| {
                    let source = cx
                        .background_executor()
                        .spawn(async move {
                            blur_wallpaper(&url_for_job, blur, viewport_for_job)
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        // `wallpaper_blurring` stays set — it marks the key
                        // as attempted so a failed job never respawns.
                        // On failure park the plain decode under the key:
                        // the wallpaper still paints, just unblurred.
                        let source = source
                            .map(|bytes| {
                                let image: gpui::ImageSource = std::sync::Arc::new(
                                    gpui::Image::from_bytes(gpui::ImageFormat::Jpeg, bytes),
                                )
                                .into();
                                image
                            })
                            .or_else(|| {
                                this.wallpaper_raw
                                    .as_ref()
                                    .filter(|(h, _)| *h == key.0)
                                    .map(|(_, src)| src.clone())
                            });
                        if let Some(source) = source {
                            this.wallpaper_cache = Some((key.0, key.1, source));
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            // Fallback while the blur job runs (and the `backgroundBlur:
            // 0` path): the plain decode, cached so a multi-MB data URL
            // isn't re-decoded every frame.
            if image.is_none() {
                if !matches!(&self.wallpaper_raw, Some((h, _)) if *h == key.0) {
                    self.wallpaper_raw = decode_data_image(&url).map(|src| (key.0, src));
                }
                image = self.wallpaper_raw.as_ref().map(|(_, src)| src.clone());
            }
            if let Some(image) = image {
                root = root.child(
                    div().absolute().inset_0().child(
                        gpui::img(image)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Cover),
                    ),
                );
                let dim =
                    (self.settings.background_dim.min(100) as f32 / 100.0 * 255.0) as u32;
                if dim > 0 {
                    root = root.child(div().absolute().inset_0().bg(rgba(0x08080800 | dim)));
                }
            }
        } else {
            self.wallpaper_cache = None;
            self.wallpaper_raw = None;
        }


        // The X11 webview child can't be clipped by GPUI compositing, so
        // the shown browser pane gets an explicit window-space clip — the
        // work area. While a full-window overlay (splash, settings) owns
        // the screen the clip collapses and the child hides.
        let browser_clip = Bounds::default();

        // One widget shows at a time, filling the work area like the
        // shell's maximized overlay did. Hidden panes keep running — but a
        // browser's native webview child must be told to stop painting or
        // X11 leaves it on screen.
        for widget in self.canvas.widgets.values().cloned().collect::<Vec<Widget>>() {
            if shown_id.as_deref() != Some(widget.id.as_str()) {
                if widget.kind.as_deref() == Some("browser") {
                    slate_app::webview::hide(&widget.id);
                }
                continue;
            }
            // The pane fills the work area — the fixed rect a maximized
            // frame used to take.
            let rect = pane;
            let zoom = 1.0f32;
            let id = widget.id.clone();
            let id_for_body = widget.id.clone();
            let id_for_close = widget.id.clone();
            let id_for_title = widget.id.clone();
            let id_for_copy = widget.id.clone();
            let id_for_agent = widget.id.clone();
            let is_active = self.active.as_deref() == Some(widget.id.as_str());
            let is_terminal = is_terminal_kind(widget.kind.as_deref());
            let renaming_this = self.renaming.as_deref() == Some(widget.id.as_str());
            let name_copied = self.name_copied_at.as_ref().is_some_and(|(cid, at)| {
                cid == &widget.id && at.elapsed() < Duration::from_millis(1200)
            });
            let header_h = theme::geometry::HEADER_HEIGHT;

            // WidgetFrame's `widget-header-shell`: h-34, `gap-1`,
            // `pl-2.5 pr-0` — the right-edge controls run flush.
            let header = div()
                .id(("slate-widget-header", widget.z as u64))
                .h(px(header_h))
                .w_full()
                .flex()
                .flex_row()
                .items_center()
                .pl(px(10.0 * zoom))
                .pr_0()
                .gap(px(4.0 * zoom))
                // `.is-terminal .widget-header-shell` is
                // `terminalHeaderGlass` — rgba(18,18,18,.35); the blurred
                // wallpaper behind stands in for the backdrop-filter.
                .bg(if is_terminal {
                    rgba(0x12121259)
                } else {
                    rgba(theme::hex(theme::monochrome::ELEVATED) << 8 | 0xFF)
                })
                // `.widget-header-shell`'s `inset 0 -1px 0 line-faint`
                // separator — suppressed on terminals
                // (`is-terminal … { boxShadow: none }`).
                .when(!is_terminal, |el| {
                    el.border_b_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event: &MouseDownEvent, window, cx| {
                        window.focus(&this.focus, cx);
                        // Clicking another widget commits an open rename —
                        // the shell's input blurred.
                        if this.renaming.is_some() {
                            this.commit_rename(cx);
                        }
                        this.active = Some(id.clone());
                        // Pressing the frame gave it DOM focus in the shell —
                        // Delete/Escape now target this pane.
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
                        // `text-xs text-text` — the shell's title is full
                        // white, not dim.
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
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
                    // button, always-on here (no group-hover trick). `h-4
                    // w-4` with Check/Copy at 10px; shows the shell's
                    // Check briefly after a click.
                    header.child(
                        div()
                            .id(("slate-widget-copy", widget.z as u64))
                            .w(px(16.0 * zoom))
                            .h(px(16.0 * zoom))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .cursor_pointer()
                            .child(crate::assets::icon(
                                if name_copied { "check" } else { "copy" },
                                10.0 * zoom,
                                theme::hex(theme::text::FAINT),
                            ))
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
                    // The shell's "Select Agent" pencil: `h-6 w-6
                    // rounded-pill` with Pencil at 11px; opens the small
                    // picker parked under this header's right edge.
                    header.child(
                        div()
                            .id(("slate-widget-agent", widget.z as u64))
                            .w(px(24.0 * zoom))
                            .h(px(24.0 * zoom))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .cursor_pointer()
                            .hover(|el| {
                                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                            })
                            .child(crate::assets::icon(
                                "pencil-thin",
                                11.0 * zoom,
                                theme::hex(theme::text::FAINT),
                            ))
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
                // `ml-1 flex h-[34px]` — the close button runs the header's
                // full height at `w-8`, keeping the shell's red hover and
                // its `rounded-tr-panel` corner.
                .child(
                    div()
                        .ml(px(4.0 * zoom))
                        .h_full()
                        .flex_none()
                        .flex()
                        .flex_row()
                        .child(
                            div()
                                .id(("slate-widget-close", widget.z as u64))
                                .w(px(32.0 * zoom))
                                .h_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                                .cursor_pointer()
                                .hover(|el| {
                                    el.bg(rgb(0xE04343)).text_color(rgb(0xFFFFFF))
                                })
                                .child(crate::assets::icon(
                                    "x",
                                    13.0 * zoom,
                                    theme::hex(theme::text::DIM),
                                ))
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        // The shell's onClose ran the same
                                        // confirm for terminals that
                                        // frame-Delete does.
                                        if is_terminal_kind(
                                            this.widget_kind(&id_for_close).as_deref(),
                                        ) {
                                            this.pending_close = Some(id_for_close.clone());
                                            cx.notify();
                                        } else {
                                            this.close_widget(&id_for_close, cx);
                                        }
                                    }),
                                ),
                        ),
                );

            let id_body_m = id_for_body.clone();
            let id_body_r = id_for_body.clone();
            let id_body_ul = id_for_body.clone();
            let id_body_um = id_for_body.clone();
            let id_body_ur = id_for_body.clone();
            let id_body_mv = id_for_body.clone();
            // Terminal bodies stay transparent — `is-canvas-terminal`
            // paints `terminalGlass` on the shell instead of a solid fill,
            // and the (now CPU-blurred) wallpaper behind does the rest.
            let body = div()
                .flex_1()
                .w_full()
                .overflow_hidden()
                .when(!is_terminal, |el| {
                    el.bg(rgb(theme::hex(theme::monochrome::BASE)))
                })
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
                    // A sibling stacked above a browser widget would be
                    // One pane at a time means nothing can overlap the
                    // browser's native webview child — clip to the work
                    // area (the element hides it under overlays anyway).
                    let clip = browser_clip;
                    let body = widget_body(
                        &widget,
                        &self.snapshots,
                        zoom,
                        self.prefs.terminals_flipped,
                        self.browser_editing.as_deref(),
                        &self.browser_address,
                        self.browser_cursor,
                        self.browser_sel,
                        clip,
                        &self.files_root(),
                        // fix-files:
                        self.files_editing.as_ref(),
                        self.files_preview.as_ref(),
                        // fix-planner:
                        self.planner_editing.as_ref(),
                        &self.planner_draft,
                        // fix-orchestration:
                        crate::views_orchestration::OrcState {
                            editing: self.orc_editing.as_ref().map(|e| e.message_id()),
                            drafts: &self.orc_drafts,
                            error: self.orc_error.as_deref(),
                        },
                        // fix-panes:
                        self.notes_editing
                            .as_ref()
                            .map(|(note, field)| (note.as_str(), field.as_str())),
                        &self.notes_buffer,
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

            // `widget-shell`: `rounded-panel overflow-hidden` — 12px
            // corners in world space, clipped so the square-cornered
            // terminal body can't poke out.
            let mut frame = div()
                .absolute()
                .left(px(rect.min.x))
                .top(px(rect.min.y))
                .w(px(rect.width()))
                .h(px(rect.height()))
                .flex()
                .flex_col()
                .rounded(px(12.0 * zoom))
                .overflow_hidden()
                // `is-canvas-terminal` wears `terminalGlass`
                // (rgba(8,8,8,.20)); there is no backdrop-filter in gpui,
                // but the wallpaper behind it is CPU-blurred, which
                // carries the same frosted read.
                .bg(if is_terminal {
                    rgba(0x08080833)
                } else {
                    rgba(theme::hex(theme::monochrome::SURFACE) << 8 | 0xFF)
                })
                // `is-canvas-terminal` has no ring at rest; active gets
                // `0 0 0 1px rgba(169,171,176,.42)`. Other widgets keep the
                // hairline-soft ring, brighter when active.
                .border_1()
                .border_color(if is_terminal {
                    if is_active {
                        rgba(0xA9ABB06B)
                    } else {
                        rgba(0x00000000)
                    }
                } else {
                    // hairline.soft and hairline.active are the same
                    // #1F1F1F — the ring never brightens.
                    rgba(theme::hex(theme::hairline::SOFT) << 8 | 0xFF)
                })
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

            root = root.child(frame);
        }

        // The pane strip — a fixed tab row above the work area, one chip
        // per widget in z order. Clicking shows that pane; the × on the
        // active chip closes it like the frame's own button.
        if !self.canvas.widgets.is_empty() {
            let mut strip = div()
                .id("slate-pane-strip")
                .absolute()
                .left(px(rail_w))
                .right_0()
                .top(px(canvas::TITLE_BAR_HEIGHT))
                .h(px(PANE_STRIP_H))
                .flex()
                .flex_row()
                .items_center()
                .gap_px()
                .px_1()
                .overflow_hidden()
                .bg(rgba(0x0C0C0CDD))
                .border_b_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT)));
            let mut ordered: Vec<Widget> =
                self.canvas.widgets.values().cloned().collect();
            ordered.sort_by(|a, b| a.z.total_cmp(&b.z));
            for w in ordered {
                let wid = w.id.clone();
                let wid_close = w.id.clone();
                let current = shown_id.as_deref() == Some(wid.as_str());
                let mut tab = div()
                    .id(format!("slate-pane-tab-{}", w.id))
                    .h_full()
                    .px_2()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .max_w(px(200.0))
                    .min_w_0()
                    .border_r_1()
                    .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                    .cursor_pointer()
                    .text_size(px(11.0))
                    .font_family("Departure Mono")
                    .text_color(rgb(theme::hex(if current {
                        theme::text::NORMAL
                    } else {
                        theme::text::FAINT
                    })))
                    .when(current, |el| {
                        el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                    })
                    .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                    .child(w.title.clone())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.active = Some(wid.clone());
                            cx.notify();
                        }),
                    );
                if current {
                    tab = tab.child(
                        div()
                            .id(format!("slate-pane-tab-close-{}", w.id))
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(10.0))
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .hover(|el| el.text_color(rgb(theme::hex(theme::text::NORMAL))))
                            .child("×")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    if is_terminal_kind(
                                        this.widget_kind(&wid_close).as_deref(),
                                    ) {
                                        this.pending_close = Some(wid_close.clone());
                                        cx.notify();
                                    } else {
                                        this.close_widget(&wid_close, cx);
                                    }
                                }),
                            ),
                    );
                }
                strip = strip.child(tab);
            }
            // "+" opens the same spawn menu right-click used to — the work
            // area is always a pane now, so the menu needs a visible handle.
            strip = strip.child(
                div()
                    .id("slate-pane-add")
                    .h_full()
                    .px_2()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .hover(|el| el.text_color(rgb(theme::hex(theme::text::NORMAL))))
                    .child("+")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _w, cx| {
                            cx.stop_propagation();
                            this.menu_sel = 0;
                            this.menu = Some(pos(event.position));
                            cx.notify();
                        }),
                    ),
            );
            root = root.child(strip);
        }


        // The agent picker — WidgetFrame's "Select Agent" dropdown, parked
        // under the header's right edge of the terminal that opened it.
        // Picking one types its launch command into the PTY with Enter, the
        // shell's terminal.write(command + '\r').
        if let Some(menu_id) = self.agent_menu.clone() {
            if self.canvas.widgets.contains_key(&menu_id) {
                let menu_w = 170.0_f32;
                let menu_x = (pane.max.x - menu_w).max(4.0);
                let menu_y = pane.min.y + theme::geometry::HEADER_HEIGHT + 2.0;
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
                for (index, (agent_id, label, command)) in
                    agent_launchers(&self.prefs).into_iter().enumerate()
                {
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
                            .child(label.clone())
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.agent_menu = None;
                                    // `Fd` + `Wd`: persist the row and the
                                    // launched id, then type the command.
                                    this.agent_picks.pick(&menu_id, index, &agent_id);
                                    if let Err(error) = this.agent_picks.save() {
                                        this.notice(format!("agent pick: {error}"));
                                    }
                                    match this.manager.write_text(&menu_id, &command, true) {
                                        // The shell armed its startup probe
                                        // the moment the agent command was
                                        // typed — same here.
                                        Ok(_) => this.arm_startup_watch(&menu_id, &command),
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

        // The empty-state card — drawn only when no panes are open. "Add
        // terminal" is the same addWidget path the button called.
        if self.canvas.widgets.is_empty() {
            let viewport = self.viewport;
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .right_0()
                    // Center on the unobscured canvas — the sidebar
                    // floats over the left edge, so inset_0 would park
                    // the card a half-rail left of the visible center.
                    .left(px(rail_w))
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
                            // bg-bg-panel/80 — colorBgPanel (elevated) at
                            // 80% alpha; the DOM version's backdrop-blur
                            // has no gpui equivalent.
                            .bg(rgba(0x181818cc))
                            .border_1()
                            .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                            .shadow_xl()
                            .child(
                                div()
                                    .font_family("Brygada 1918")
                                    .text_size(px(22.0))
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                    .child("Nothing open yet".to_owned()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                                    .child(
                                        "Use the command bar below: /terminal, /chat, .files, @planner, or plain terminal"
                                            .to_owned(),
                                    ),
                            )
                            .child(
                                div()
                                    .id("slate-empty-add-terminal")
                                    .px_3()
                                    .py_1()
                                    // The one inverse chip on the canvas —
                                    // paper fill, ink text, mono caps like
                                    // graysite's primary buttons.
                                    .bg(rgb(theme::hex(theme::text::NORMAL)))
                                    .hover(|s| s.bg(rgb(theme::hex(theme::accent::SAND_LO))))
                                    .text_size(px(11.0))
                                    .font_family("Departure Mono")
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(rgb(theme::hex(theme::monochrome::BASE)))
                                    .cursor_pointer()
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            this.spawn_terminal(viewport, cx);
                                        }),
                                    )
                                    .child("ADD TERMINAL".to_owned()),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_size(px(10.0))
                                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                                    .child(kbd_chip("T"))
                                    .child(kbd_chip("N"))
                                    .child(kbd_hint("Tab", " switch", "Tab cycles panes"))
                                    .child(kbd_hint("Del", " close", "Del closes the active pane")),
                            ),
                    ),
            );
        }

        // The workview — the Code pane covers the canvas world while its
        // tab is up: an opaque cover between the title bar and the status
        // bar, so the canvas (widgets, camera, PTYs) is untouched
        // underneath. The cover's own press/scroll handlers stop
        // propagation — a click on it must never pan or select the world
        // it hides. The shell did the same by unmounting the canvas DOM;
        // keeping it mounted is cheaper (no re-layout on the way back)
        // and the cover makes the world invisible anyway.
            // The cover sits beside the open rail — its own width is
            // viewport minus rail_w, so the workview lays out against the
            // narrowed width (a full-width grid would slide rail_w under
            // the sidebar and clip cards at the left edge).
            let workview = Size {
                width: px((f32::from(viewport.width) - rail_w).max(0.0)),
                height: viewport.height,
            };
                self.resize_code_terminals(Vec2::new(
                    f32::from(workview.width),
                    f32::from(workview.height),
                ));
            let pane: AnyElement = {
                    // `ResumeAgents` mounts with the empty board — scan once
                    // per folder, on the background executor.
                    self.maybe_scan_resume(cx);
                    crate::views_code::code_pane(
                        &crate::views_code::CodeState {
                            sessions: {
                                // t3code's pinned section — pinned threads
                                // sort to the top, order stable otherwise.
                                let mut sessions = self.code_sessions.clone();
                                sessions.sort_by_key(|s| !s.pinned);
                                sessions
                            },
                            focused: self.code_focused.clone(),
                            viewport: workview,
                            launcher_open: self.code_launcher_open,
                            layout: self.code_layout,
                            workspace_name: self
                                .workspace_dir
                                .as_deref()
                                .and_then(|dir| std::path::Path::new(dir).file_name())
                                .map(|name| name.to_string_lossy().into_owned()),
                            workspace_dir: self.workspace_dir.clone(),
                            conversations: self.code_conversations.clone(),
                            resume_loading: self.code_resume_loading,
                            resume_expanded: self.code_resume_expanded,
                            resume_selected: self.code_resume_selected.clone(),
                            resume_dismissed: self.code_resume_dismissed.clone(),
                            selected_agents: self.code_selected_agents.clone(),
                            agent_counts: self.code_agent_counts.clone(),
                            active_agent: self.code_active_agent.clone(),
                            custom_agents: self.prefs.custom_agents.clone(),
                            agent_states: self.agent_states.clone(),
                            worktree: self.code_worktree,
                        },
                        &self.snapshots,
                        cx,
                    )
                    .into_any_element()
            };
            root = root.child(
                div()
                    .id("slate-workview")
                    .absolute()
                    .top(px(canvas::TITLE_BAR_HEIGHT))
                    // Beside the open rail, not under it — the sidebar
                    // floats over the canvas, and a cover at left 0 put the
                    // launcher row and the first grid column behind it.
                    .left(px(rail_w))
                    .right(px(0.0))
                    .bottom(px(0.0))
                    .bg(rgb(theme::hex(theme::monochrome::BASE)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this, _event, _window, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|_this, _event, _window, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|_this, _event, _window, cx| {
                            cx.stop_propagation();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _window, cx| {
                        // A code session under the pointer owns the wheel —
                        // the same scrollback / mouse-report routing the
                        // canvas's wheel handler runs for terminal widgets.
                            let pointer = pos(event.position);
                            if let Some((index, rect)) = this.code_hit(pointer) {
                                let offset = this.code_grid_offset();
                                let session = &this.code_sessions[index];
                                // Over the card header the card owns the
                                // event but nothing scrolls — same rule as
                                // a canvas widget's header.
                                if pointer.y - offset.y - rect.top()
                                    > crate::views_code::CARD_HEADER_H
                                {
                                    let reporting = this
                                        .snapshots
                                        .iter()
                                        .find(|s| s.id == session.terminal_id)
                                        .map(|s| s.mouse_reporting)
                                        .unwrap_or(false);
                                    if reporting {
                                        let (dy, dx) = match event.delta {
                                            ScrollDelta::Pixels(p) => {
                                                (f32::from(p.y), f32::from(p.x))
                                            }
                                            ScrollDelta::Lines(p) => (p.y, p.x),
                                        };
                                        let mods = KeyMods {
                                            shift: event.modifiers.shift,
                                            alt: event.modifiers.alt,
                                            ctrl: event.modifiers.control,
                                            mac_cmd: event.modifiers.platform,
                                        };
                                        let (col, row) =
                                            Self::code_cell(&rect, offset, pointer);
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
                                            let _ = this.manager.mouse_input(
                                                &session.terminal_id,
                                                kind,
                                                col,
                                                row,
                                                mods,
                                            );
                                        }
                                    } else {
                                        let pixels = match event.delta {
                                            ScrollDelta::Pixels(p) => f32::from(p.y),
                                            ScrollDelta::Lines(p) => p.y * CELL_H,
                                        };
                                        let _ = this.manager.scroll_pixels(
                                            &session.terminal_id,
                                            pixels,
                                            CELL_H,
                                        );
                                    }
                                }
                            }
                        cx.stop_propagation();
                    }))
                    // Button releases land here when the press went to a
                    // reporting program (selection drags skip the release,
                    // like the widget body's `selection_drag.is_none()`
                    // gate); held-button moves become the drag report.
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, _cx| {
                        let Some(button) = event.pressed_button else {
                            return;
                        };
                        let pointer = pos(event.position);
                        if let Some((index, rect)) = this.code_hit(pointer) {
                            let session = &this.code_sessions[index];
                            if this.selection_drag.as_deref()
                                == Some(session.terminal_id.as_str())
                            {
                                return;
                            }
                            let offset = this.code_grid_offset();
                            let (col, row) = Self::code_cell(&rect, offset, pointer);
                            let button = match button {
                                MouseButton::Middle => TermMouseButton::Middle,
                                MouseButton::Right => TermMouseButton::Secondary,
                                _ => TermMouseButton::Primary,
                            };
                            let mods = KeyMods {
                                shift: event.modifiers.shift,
                                alt: event.modifiers.alt,
                                ctrl: event.modifiers.control,
                                mac_cmd: event.modifiers.platform,
                            };
                            let _ = this.manager.mouse_input(
                                &session.terminal_id,
                                MouseKind::Move(Some(button)),
                                col,
                                row,
                                mods,
                            );
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                            if this.selection_drag.is_some() {
                                return;
                            }
                            let pointer = pos(event.position);
                            if let Some((index, rect)) = this.code_hit(pointer) {
                                let session = &this.code_sessions[index];
                                let offset = this.code_grid_offset();
                                let (col, row) = Self::code_cell(&rect, offset, pointer);
                                let mods = KeyMods {
                                    shift: event.modifiers.shift,
                                    alt: event.modifiers.alt,
                                    ctrl: event.modifiers.control,
                                    mac_cmd: event.modifiers.platform,
                                };
                                let _ = this.manager.mouse_input(
                                    &session.terminal_id,
                                    MouseKind::Release(TermMouseButton::Primary),
                                    col,
                                    row,
                                    mods,
                                );
                            }
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                            let pointer = pos(event.position);
                            if let Some((index, rect)) = this.code_hit(pointer) {
                                let session = &this.code_sessions[index];
                                let offset = this.code_grid_offset();
                                let (col, row) = Self::code_cell(&rect, offset, pointer);
                                let mods = KeyMods {
                                    shift: event.modifiers.shift,
                                    alt: event.modifiers.alt,
                                    ctrl: event.modifiers.control,
                                    mac_cmd: event.modifiers.platform,
                                };
                                let _ = this.manager.mouse_input(
                                    &session.terminal_id,
                                    MouseKind::Release(TermMouseButton::Middle),
                                    col,
                                    row,
                                    mods,
                                );
                            }
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseUpEvent, _window, _cx| {
                            let pointer = pos(event.position);
                            if let Some((index, rect)) = this.code_hit(pointer) {
                                let session = &this.code_sessions[index];
                                let offset = this.code_grid_offset();
                                let (col, row) = Self::code_cell(&rect, offset, pointer);
                                let mods = KeyMods {
                                    shift: event.modifiers.shift,
                                    alt: event.modifiers.alt,
                                    ctrl: event.modifiers.control,
                                    mac_cmd: event.modifiers.platform,
                                };
                                let _ = this.manager.mouse_input(
                                    &session.terminal_id,
                                    MouseKind::Release(TermMouseButton::Secondary),
                                    col,
                                    row,
                                    mods,
                                );
                            }
                        }),
                    )
                    .child(pane),
            );

        // The command bar — a summon-only overlay now, not the Electron
        // strip: it renders only while `command_bar` owns the keyboard
        // (`/`, `.`, `@` open it, Escape drops it) and carries just the
        // input echo, `↵`, and the suggestion listbox.

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

        // The arrange dropdown — the shell's `top-[36px] right-0 w-[210px]`
        // menu under the TitleBar's right island: label + hint rows, `✓`
        // on the active mode. The shell anchors `right-0` on the arrange
        // button's `relative mr-1` wrapper, so the popover's right edge
        // sits `git-pill + divider + controls + 4px` in from the window's
        // right edge — measured, since the chip width varies with the
        // branch name.
        if self.arrange_menu {
            let git_w =
                crate::views_titlebar::git_chip_width(&self.git_chip, viewport_size.x, window);
            // `!vt`: divider `mx-0.5` + w-px and the three 46px controls —
            // on tiling WMs the cluster collapses to the close cell alone.
            let chrome_w = if cfg!(target_os = "macos") {
                0.0
            } else if crate::views_titlebar::tiling_wm() {
                46.0
            } else {
                5.0 + 3.0 * 46.0
            };
            let mut popup = div()
                .id("slate-arrange-menu")
                .absolute()
                .top(px(36.0))
                .right(px(4.0 + git_w + chrome_w))
                .w(px(210.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .py_1()
                .shadow_2xl();
                    for mode in slate_app::code_layout::CodeLayout::ALL {
                        let current = mode == self.code_layout;
                        popup = popup.child(
                            arrange_menu_row(
                                format!("slate-arrange-{}", mode.label().to_lowercase()),
                                mode.label(),
                                mode.hint(),
                                current,
                            )
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _e, _w, cx| {
                                    cx.stop_propagation();
                                    this.arrange_menu = false;
                                    this.code_command(
                                        "layout",
                                        json!({ "mode": serde_json::to_value(mode).unwrap_or_default() }),
                                        cx,
                                    );
                                }),
                            ),
                        );
                    }
            root = root.child(popup);
        }

        // The left rail — the shell's `<Sidebar>` was `w-rail` on the
        // canvas (56px collapsed, 200px open), always mounted. Collapsed
        // keeps the » reopen tab, the widget/plan counts and `+`;
        // expanded shows the full panel with the account footer.
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
                .bottom_0()
                .child(crate::views_sidebar::sidebar_pane(
                    &self.canvas,
                    &planner_items,
                    self.prefs.sidebar_open,
                    // `userName` defaults to "you" when no settings.json
                    // exists — the OS login reads better than that.
                    if self.settings.user_name() == "you" {
                        std::env::var("USER").unwrap_or_else(|_| "you".into())
                    } else {
                        self.settings.user_name().to_owned()
                    }
                    .as_str(),
                    cx,
                )),
        );

        // The TitleBar — the shell's fixed 40px top strip (history
        // arrows, view tabs, flip, git chip, window controls), floating
        // over the canvas exactly like the Electron layer order: canvas →
        // chrome. Always mounted (`App.tsx:260`) — without it the workspace
        // tab is unreachable whenever workspaceDir is unset.
        {
            root = root.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(canvas::TITLE_BAR_HEIGHT))
                    .child(crate::views_titlebar::titlebar_pane(
                        &crate::views_titlebar::TitlebarState {
                            can_undo: !self.undo_stack.is_empty(),
                            can_redo: !self.redo_stack.is_empty(),
                            terminals_flipped: self.prefs.terminals_flipped,
                            git: &self.git_chip,
                            window_width: viewport_size.x,
                            window_maximized: window.is_maximized(),
                        },
                        cx,
                    )),
            );
        }

        // The settings modal — `orcspace:open-settings` from the gear.
        if self.settings_open {
            root = root.child(crate::views_settings::settings_modal(&self.settings, cx));
        }

        // The StartupWordmark screen — the last child so it sits over the
        // title bar and rail, like the original's z-100000 startup-screen.
        // `occlude` keeps the canvas inert underneath it; any deliberate
        // press dismisses the intro early.
        if let Some(t) = splash_t {
            // Boot readiness, measured not assumed: each respawned PTY
            // stays pending until a snapshot shows its first screen (a
            // shell prompt, a restored TUI) or it exits; a missing
            // snapshot means the engine already dropped it. The 12s cap
            // is a wedged-PTY escape so the loader never hangs. Undelivered
            // boot commands (still retrying) hold it too.
            if !self.splash_pending.is_empty() {
                let snapshots = &self.snapshots;
                self.splash_pending.retain(|id| {
                    if t >= 12.0 {
                        return false;
                    }
                    match snapshots.iter().find(|s| &s.id == id) {
                        Some(s) => s.alive && s.screen.trim().is_empty(),
                        None => false,
                    }
                });
            }
            if self.splash_pending.is_empty()
                && self.pending_commands.is_empty()
                && self.splash_ready_at.is_none()
            {
                self.splash_ready_at = Some(Instant::now());
            }
            // `.is-ready`'s fade: the screen holds opaque until 200ms after
            // ready latches, then eases out over 900ms. There is no
            // minimum intro length — a fast boot ends fast.
            let ready = self.splash_ready_at.is_some();
            let fade = self
                .splash_ready_at
                .map(|at| at.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            let screen = 1.0 - smooth01((fade - 0.2) / 0.9);
            let points = self.splash_points.clone();
            let (width, height) = (viewport_size.x, viewport_size.y);
            // The caption bits appear at t=2 (startup-loader-in 300ms 2s);
            // the track sweeps on a 1600ms loop until the "ready" beat at
            // t=7 leaves it full and static, like `.is-ready span`.
            let loader_in = smooth01((t - 2.0) / 0.3);
            let track_w = (width * 0.6).min(280.0);
            let sweep_w = track_w * 0.3;
            let cycle = ((t - 2.0) / 1.6).max(0.0).fract();
            // translateX(-100% → 334%) of the 30%-wide span, eased with the
            // closest smoothstep stand-in for cubic-bezier(.65,0,.35,1).
            let sweep_x = (-1.0 + 4.34 * smooth01(cycle)) * sweep_w;
            // The cloud coalesces into the product name: it fades in
            // through the burst (t≈5.9) and holds until the screen fades.
            let word_alpha = smooth01((t - 5.6) / 0.8);
            root = root.child(
                div()
                    .id("slate-startup")
                    .absolute()
                    .inset_0()
                    .occlude()
                    .opacity(screen)
                    .bg(rgb(theme::hex(theme::monochrome::BASE)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _e, _w, cx| {
                            this.splash_started = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(|this, _e, _w, cx| {
                            this.splash_started = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, _e, _w, cx| {
                            this.splash_started = None;
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|_this, _e, _w, cx| {
                        // The screen is modal while it plays — a wheel
                        // press never reaches the pan/zoom handler.
                        cx.stop_propagation();
                    }))
                    .child(
                        gpui::canvas(
                            move |_, _, _| (points.clone(), t),
                            move |_bounds, (points, t), window, _cx| {
                                if t < SPLASH_PARTICLES_END {
                                    paint_splash_particles(window, &points, t);
                                }
                            },
                        )
                        .absolute()
                        .inset_0(),
                    )
                    // The wordmark reveal at (w/2, h*.47) — the spot the
                    // particles converge on.
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(px((height * 0.47 - 34.0).max(0.0)))
                            .flex()
                            .justify_center()
                            .opacity(word_alpha)
                            .child(
                                div()
                                    .text_size(px(52.0))
                                    .font_family("Brygada 1918")
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                    .child("Slate"),
                            ),
                    )
                    // The caption — `.startup-track` under the wordmark box
                    // plus `.startup-loading-label` beneath it.
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(px((height * 0.5 + 150.0).min(height - 60.0)))
                            .flex()
                            .flex_col()
                            .items_center()
                            .opacity(loader_in)
                            .child(
                                div()
                                    .relative()
                                    .w(px(track_w))
                                    .h(px(3.0))
                                    .overflow_hidden()
                                    .bg(rgb(theme::hex(theme::hairline::FAINT)))
                                    .child(
                                        div()
                                            .absolute()
                                            .left(px(if ready { 0.0 } else { sweep_x }))
                                            .top_0()
                                            .h_full()
                                            .w(px(if ready { track_w } else { sweep_w }))
                                            .bg(rgb(theme::hex(theme::text::NORMAL))),
                                    ),
                            )
                            .child(
                                div()
                                    .mt_3()
                                    .text_xs()
                                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                                    .child(if ready {
                                        "Ready."
                                    } else {
                                        "Preparing your workspace…"
                                    }),
                            ),
                    ),
            );
        }
        root
    }
}

impl CanvasView {
    fn on_key(&mut self, event: &KeyDownEvent, viewport: Vec2, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let mods = &keystroke.modifiers;

        // The startup splash owns input while it plays — the Electron shell
        // set #root inert and let any deliberate press end the intro, but a
        // keydown carrying Ctrl/Alt/Meta was ignored so a held modifier
        // never skipped it. Either way the keystroke is swallowed.
        if self.splash_started.is_some() {
            if !(mods.control || mods.alt || mods.platform) {
                self.splash_started = None;
                self.splash_points = Rc::new(Vec::new());
            }
            cx.notify();
            return;
        }

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
        if (self.agent_menu.is_some() || self.arrange_menu)
            && keystroke.key.as_str() == "escape"
        {
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

        // fix-files: the files pane's preview overlay is modal while it is
        // up — the shell's portal dialog ran a window-capture keydown plus
        // a focus trap, so Escape closes it and every other key is
        // swallowed before the command bar or a field ever sees it.
        if self.files_preview.is_some() {
            // Self-heal: a preview whose widget is gone drops on the next
            // keypress instead of holding the keyboard forever.
            let alive = self
                .files_preview
                .as_ref()
                .map(|p| self.canvas.widgets.contains_key(&p.id))
                .unwrap_or(false);
            if alive {
                if keystroke.key.as_str() == "escape" {
                    self.files_preview = None;
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            self.files_preview = None;
        }

        // The command bar owns the keyboard while it is open: Escape closes,
        // Enter runs, Backspace edits, and anything printable appends. This
        // sits before the terminal branch on purpose — a bar that is open
        // keeps its keys even over a focused widget.
        if self.command_bar.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.command_bar = None;
                    // Sticky message mode released on Escape — the shell's
                    // mode reset when the input dismissed.
                    self.command_message = false;
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
                    self.submit_command_bar(viewport, cx);
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

        // fix-planner: the planner pane's open field owns the keyboard — the
        // omnibox's editing keys without a selection. Enter commits (a create
        // field adds the task), Escape reverts, arrows/Home/End move the caret,
        // ⌘/Ctrl+V pastes text or a clipboard photo.
        if self.planner_editing.is_some() {
            let paste = (mods.control || mods.platform) && keystroke.key == "v";
            match keystroke.key.as_str() {
                "escape" => self.planner_editing = None,
                "enter" => self.planner_submit(cx),
                "backspace" => self.planner_edit_backspace(),
                "delete" => self.planner_edit_delete(),
                "left" => self.planner_edit_step(false),
                "right" => self.planner_edit_step(true),
                "home" => self.planner_edit_edge(false),
                "end" => self.planner_edit_edge(true),
                _ if paste => self.planner_paste(cx),
                _ => {
                    if !mods.control && !mods.alt && !mods.platform {
                        let text = keystroke.key_char.clone().or_else(|| {
                            match keystroke.key.as_str() {
                                "space" => Some(" ".to_owned()),
                                k if k.chars().count() == 1 => Some(k.to_owned()),
                                _ => None,
                            }
                        });
                        if let Some(text) = text {
                            self.planner_edit_insert(&text);
                        }
                    }
                }
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // fix-panes: the notes editor owns the keyboard while a field is open.
        // Enter commits it as a note.update, Escape drops it, printable keys
        // and Backspace edit the buffer (no caret — the notes field is short).
        if self.notes_editing.is_some() {
            match keystroke.key.as_str() {
                "escape" => {
                    self.notes_editing = None;
                    self.notes_buffer.clear();
                }
                "enter" => self.notes_commit_edit(),
                "backspace" => {
                    self.notes_buffer.pop();
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
                            self.notes_buffer.push_str(&text);
                        }
                    }
                }
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // The browser omnibox owns the keyboard while it is editing — the
        // same shape as rename plus the DOM input's editing keys: Enter
        // navigates, Escape reverts, arrows/Home/End move the caret (⇧
        // extends the selection), ⌘/Ctrl+A selects all, ⌘/Ctrl+V pastes,
        // printable keys replace the selection at the caret.
        if let Some(wid) = self.browser_editing.clone() {
            let select_all = (mods.control || mods.platform) && keystroke.key == "a";
            let paste = (mods.control || mods.platform) && keystroke.key == "v";
            match keystroke.key.as_str() {
                "escape" => {
                    self.browser_editing = None;
                    self.browser_address.clear();
                    self.browser_cursor = 0;
                    self.browser_sel = None;
                }
                "enter" => {
                    let url = std::mem::take(&mut self.browser_address);
                    self.browser_editing = None;
                    self.browser_cursor = 0;
                    self.browser_sel = None;
                    self.widget_command(
                        &wid,
                        json!({ "op": "browser_navigate", "url": url }),
                        cx,
                    );
                }
                "backspace" => self.browser_edit_backspace(),
                "delete" => self.browser_edit_delete(),
                "left" => self.browser_edit_move(false, mods.shift),
                "right" => self.browser_edit_move(true, mods.shift),
                "home" => self.browser_edit_edge(false, mods.shift),
                "end" => self.browser_edit_edge(true, mods.shift),
                _ if select_all => {
                    // `e.target.select()` — the whole buffer highlighted.
                    self.browser_sel = Some(0);
                    self.browser_cursor = self.browser_address.len();
                }
                _ if paste => {
                    if let Some(item) = cx.read_from_clipboard() {
                        if let Some(text) = item.text() {
                            self.browser_edit_insert(&text);
                        }
                    }
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
                            self.browser_edit_insert(&text);
                        }
                    }
                }
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // fix-orchestration: an armed reply/note field claims the keyboard
        // the same way the files fields below do.
        if self.orc_edit_key(keystroke, mods) {
            cx.notify();
            cx.stop_propagation();
            return;
        }

        // fix-files: the files pane's fields own the keyboard with the same
        // shape as the omnibox. `Search` commits each keystroke to
        // `state.search` — the original's `setSearch` filtered live and
        // reset `visibleCount` — so its Enter/Escape only release; `Rename`
        // is the `renamingName` buffer: Enter dispatches `file_rename`,
        // Escape is `renameCancelled`. An edit whose widget vanished drops
        // itself rather than holding the keys forever.
        if let Some(edit) = self.files_editing.take() {
            if !self.canvas.widgets.contains_key(edit.widget_id()) {
                cx.notify();
                cx.stop_propagation();
                return;
            }
            match edit {
                crate::views_files::FilesEditing::Search { id } => {
                    let keep = match keystroke.key.as_str() {
                        "enter" | "escape" => false,
                        _ => {
                            let mut query = self
                                .canvas
                                .widgets
                                .get(&id)
                                .and_then(|w| w.state.as_ref())
                                .and_then(|s| s.get("search"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_owned();
                            let edited = if keystroke.key.as_str() == "backspace" {
                                query.pop().is_some()
                            } else if !mods.control && !mods.alt && !mods.platform {
                                let text = keystroke.key_char.clone().or_else(|| {
                                    match keystroke.key.as_str() {
                                        "space" => Some(" ".to_owned()),
                                        k if k.chars().count() == 1 => Some(k.to_owned()),
                                        _ => None,
                                    }
                                });
                                if let Some(text) = text {
                                    query.push_str(&text);
                                    // The shell's saved-search cap.
                                    query = query.chars().take(500).collect();
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            // `setSearch` — the live filter plus the
                            // 200-row window reset; untouched keys
                            // (arrows, Tab) journal nothing.
                            if edited {
                                self.widget_command(
                                    &id,
                                    json!({ "op": "set", "state": {
                                        "search": query,
                                        "limit": 200,
                                    } }),
                                    cx,
                                );
                            }
                            true
                        }
                    };
                    if keep {
                        self.files_editing =
                            Some(crate::views_files::FilesEditing::Search { id });
                    }
                }
                crate::views_files::FilesEditing::Rename { id, path, mut buffer } => {
                    match keystroke.key.as_str() {
                        "escape" => {
                            // `renameCancelled` — the edit drops with no
                            // dispatch.
                        }
                        "enter" => {
                            self.widget_command(
                                &id,
                                json!({ "op": "file_rename", "path": path, "name": buffer }),
                                cx,
                            );
                        }
                        "backspace" => {
                            buffer.pop();
                            self.files_editing =
                                Some(crate::views_files::FilesEditing::Rename {
                                    id,
                                    path,
                                    buffer,
                                });
                        }
                        _ => {
                            if !mods.control && !mods.alt && !mods.platform {
                                let text = keystroke.key_char.clone().or_else(|| {
                                    match keystroke.key.as_str() {
                                        "space" => Some(" ".to_owned()),
                                        k if k.chars().count() == 1 => Some(k.to_owned()),
                                        _ => None,
                                    }
                                });
                                if let Some(text) = text {
                                    buffer.push_str(&text);
                                }
                            }
                            self.files_editing =
                                Some(crate::views_files::FilesEditing::Rename {
                                    id,
                                    path,
                                    buffer,
                                });
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
        // image lands in a widget, not the PTY). A workview covers the
        // canvas — the widget would land out of sight.

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

        // A focused terminal owns the keyboard, like the xterm widget did —
        // a Code view session's PTY counts while that view is up.
        if let Some(id) = self.active.clone() {
            if self.terminal_targetable(&id) {
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
                                    // `saveClipboard` → `saveBytes`: the
                                    // clipboard image lands in the media
                                    // store content-addressed, and its path
                                    // attaches like a dropped file's.
                                    let media_dir = slate_app::ipc::user_data_dir().join("media");
                                    if let Ok(path) = slate_app::attachments::save_bytes(
                                        &image.bytes,
                                        image.format.extension(),
                                        &media_dir,
                                    ) {
                                        if let Ok(token) = slate_app::attachments::path_token(
                                            &path.to_string_lossy(),
                                        ) {
                                            tokens.push_str(&token);
                                        }
                                        image_saved = true;
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
                    // scrollOnUserInput covers paste in xterm — a paste at a
                    // scrolled-back pane snaps to the bottom like typing does.
                    let _ = self.manager.scroll_to_bottom(&id);
                    cx.stop_propagation();
                    return;
                }
                // Ctrl+Shift+C/X copies the selection then clears it — the
                // shell's non-mac copy path. A bare Ctrl+C still sends ^C,
                // and with no selection the chord falls through to encode
                // as ^C too (an empty copy used to eat it).
                if mods.control
                    && mods.shift
                    && matches!(keystroke.key.as_str(), "c" | "x" | "C" | "X")
                {
                    if let Some(text) = self.manager.selection_text(&id) {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                        self.manager.selection_clear(&id);
                        self.refresh_selection(&id);
                        cx.stop_propagation();
                        return;
                    }
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

        // A workview replaces the canvas — a focused Code session already
        // took the terminal branch; every canvas shortcut below stops here.
        // Exception: Escape dismisses the floating layers — menus first,
        // then the Code launcher row (the original's CodeLauncher dialog
        // closed on Escape/backdrop).
            if keystroke.key.as_str() == "escape" {
                if self.arrange_menu {
                    self.arrange_menu = false;
                } else if self.code_launcher_open {
                    self.code_launcher_open = false;
                }
            }
            cx.notify();
            cx.stop_propagation();

    }

    // fix-files: release the files pane's keyboard owner — the original's
    // real inputs unmounted on navigation (an armed rename's row scrolls
    // away with the folder); our buffer has to be told, or an invisible
    // field keeps capturing keys.
    pub(crate) fn files_edit_release(&mut self, id: &str) {
        if self.files_editing.as_ref().map(|e| e.widget_id()) == Some(id) {
            self.files_editing = None;
        }
    }

    // fix-browser: the omnibox's DOM-input editing model — a caret
    // (`browser_cursor`) plus an optional selection anchor (`browser_sel`),
    // byte offsets always snapped to char boundaries.

    /// The highlighted range `(lo, hi)`, or `None` for a bare caret.
    fn browser_edit_selection(&self) -> Option<(usize, usize)> {
        let anchor = self.browser_sel?;
        if anchor == self.browser_cursor {
            return None;
        }
        Some((
            anchor.min(self.browser_cursor),
            anchor.max(self.browser_cursor),
        ))
    }

    /// Snap a byte offset to a char boundary — `replace_range` demands it.
    fn browser_edit_snap(&self, mut at: usize) -> usize {
        at = at.min(self.browser_address.len());
        while at > 0 && !self.browser_address.is_char_boundary(at) {
            at -= 1;
        }
        at
    }

    fn browser_edit_next(&self, at: usize) -> usize {
        self.browser_address[at..]
            .chars()
            .next()
            .map(|c| at + c.len_utf8())
            .unwrap_or(at)
    }

    fn browser_edit_prev(&self, at: usize) -> usize {
        self.browser_address[..at]
            .chars()
            .next_back()
            .map(|c| at - c.len_utf8())
            .unwrap_or(at)
    }

    /// Arrow keys: a bare caret steps a char; a selection collapses to the
    /// arrow's edge (the DOM input's unmodified-arrow rule). ⇧ keeps the
    /// anchor and moves the caret — the selection grows.
    fn browser_edit_move(&mut self, forward: bool, shift: bool) {
        self.browser_cursor = self.browser_edit_snap(self.browser_cursor);
        if shift {
            if self.browser_sel.is_none() {
                self.browser_sel = Some(self.browser_cursor);
            }
        } else if let Some((lo, hi)) = self.browser_edit_selection() {
            self.browser_cursor = if forward { hi } else { lo };
            self.browser_sel = None;
            return;
        } else {
            self.browser_sel = None;
        }
        self.browser_cursor = if forward {
            self.browser_edit_next(self.browser_cursor)
        } else {
            self.browser_edit_prev(self.browser_cursor)
        };
    }

    /// Home/End — caret to the buffer edge, ⇧ selecting to it.
    fn browser_edit_edge(&mut self, end: bool, shift: bool) {
        self.browser_cursor = self.browser_edit_snap(self.browser_cursor);
        if shift {
            if self.browser_sel.is_none() {
                self.browser_sel = Some(self.browser_cursor);
            }
        } else {
            self.browser_sel = None;
        }
        self.browser_cursor = if end {
            self.browser_address.len()
        } else {
            0
        };
    }

    /// Typed/pasted text replaces the selection (or inserts at the caret).
    fn browser_edit_insert(&mut self, text: &str) {
        let (lo, hi) = self.browser_edit_selection().unwrap_or_else(|| {
            let at = self.browser_edit_snap(self.browser_cursor);
            (at, at)
        });
        self.browser_address.replace_range(lo..hi, text);
        self.browser_cursor = lo + text.len();
        self.browser_sel = None;
    }

    /// Backspace — the selection first, else the char before the caret.
    fn browser_edit_backspace(&mut self) {
        if let Some((lo, hi)) = self.browser_edit_selection() {
            self.browser_address.replace_range(lo..hi, "");
            self.browser_cursor = lo;
        } else {
            let at = self.browser_edit_snap(self.browser_cursor);
            let prev = self.browser_edit_prev(at);
            if prev != at {
                self.browser_address.replace_range(prev..at, "");
            }
            self.browser_cursor = prev;
        }
        self.browser_sel = None;
    }

    /// Delete — the selection first, else the char after the caret.
    fn browser_edit_delete(&mut self) {
        if let Some((lo, hi)) = self.browser_edit_selection() {
            self.browser_address.replace_range(lo..hi, "");
            self.browser_cursor = lo;
        } else {
            let at = self.browser_edit_snap(self.browser_cursor);
            let next = self.browser_edit_next(at);
            if next != at {
                self.browser_address.replace_range(at..next, "");
            }
            self.browser_cursor = at;
        }
        self.browser_sel = None;
    }

    /// fix-planner: focus a field (the original's input focus). The field that
    /// was open flushes first (`planner_flush`), then `field` opens with `seed`
    /// as its buffer and the caret at the end.
    pub(crate) fn planner_begin(
        &mut self,
        wid: String,
        field: crate::views_planner::PlannerField,
        seed: String,
        cx: &mut Context<Self>,
    ) {
        // A click inside the field that already owns the caret keeps the caret.
        if self.planner_editing.as_ref().is_some_and(|edit| edit.field == field) {
            return;
        }
        self.planner_flush(cx);
        self.planner_editing = Some(crate::views_planner::PlannerEdit {
            wid,
            field,
            cursor: seed.len(),
            buffer: seed.clone(),
            seed,
        });
        cx.notify();
    }

    /// Closes the open field. A create field stores its buffer in the draft;
    /// a row field commits as the existing `plan_update` op with a `patch`
    /// (blur and Enter both land here). An unchanged field commits nothing, and
    /// a malformed date is refused with a notice — the fold would abort on it.
    fn planner_flush(&mut self, cx: &mut Context<Self>) {
        use crate::views_planner::PlannerField;
        let Some(edit) = self.planner_editing.take() else {
            return;
        };
        let text = edit.buffer;
        match &edit.field {
            PlannerField::Title(id) | PlannerField::Note(id) | PlannerField::Day(id) => {
                if text == edit.seed {
                    return;
                }
                let patch = match &edit.field {
                    PlannerField::Title(_) => {
                        let title = text.trim();
                        if title.is_empty() {
                            return;
                        }
                        json!({ "title": title })
                    }
                    PlannerField::Note(_) => json!({ "note": text }),
                    _ => {
                        let today = slate_app::planner::today_local();
                        match slate_app::planner::normalize_day(
                            Some(&json!(text.trim())),
                            &today,
                        ) {
                            Ok(day) => json!({ "day": day }),
                            Err(error) => {
                                self.notice(error.0);
                                return;
                            }
                        }
                    }
                };
                let action = json!({ "op": "plan_update", "id": id, "patch": patch });
                self.widget_command(&edit.wid, action, cx);
            }
            field => {
                if let Some(slot) = self.planner_draft.slot(field) {
                    *slot = text;
                }
            }
        }
    }

    /// Enter in the open field: a create field adds the task and reopens
    /// itself empty (the original keeps focus in the input); a row field
    /// commits and closes.
    fn planner_submit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.planner_editing.clone() else {
            return;
        };
        if !edit.field.is_create() {
            self.planner_flush(cx);
            return;
        }
        self.planner_add(&edit.wid, cx);
        let seed = self.planner_draft.value(&edit.field);
        self.planner_begin(edit.wid, edit.field, seed, cx);
    }

    /// The create form's Add (Enter in a create field too): flush the open
    /// field into the draft, then dispatch the scope-aware `plan_create`. A
    /// blank title is a no-op; a bad date is a notice and keeps the draft.
    pub(crate) fn planner_add(&mut self, wid: &str, cx: &mut Context<Self>) {
        self.planner_flush(cx);
        let state = self.canvas.widgets.get(wid).and_then(|w| w.state.clone());
        match crate::views_planner::create_action(state.as_ref(), &self.planner_draft) {
            Ok(Some(action)) => {
                self.widget_command(wid, action, cx);
                self.planner_draft.title.clear();
                self.planner_draft.attachments.clear();
            }
            Ok(None) => {}
            Err(error) => self.notice(error),
        }
        cx.notify();
    }

    /// The × on a thumb. The draft drops the photo locally; an item journals
    /// its remaining list as `plan_update` — an empty list clears the field.
    pub(crate) fn planner_remove_attachment(
        &mut self,
        target: crate::views_planner::PlannerAttachTarget,
        index: usize,
        cx: &mut Context<Self>,
    ) {
        use crate::views_planner::PlannerAttachTarget;
        match target {
            PlannerAttachTarget::Draft => {
                if index < self.planner_draft.attachments.len() {
                    self.planner_draft.attachments.remove(index);
                }
            }
            PlannerAttachTarget::Item(id) => {
                let mut kept = crate::views_planner::planner_items()
                    .0
                    .into_iter()
                    .find(|row| row.id == id)
                    .map(|row| row.attachments)
                    .unwrap_or_default();
                if index >= kept.len() {
                    return;
                }
                kept.remove(index);
                self.commit(
                    "plan.update",
                    &format!("plan:{id}"),
                    json!({ "attachments": kept }),
                );
            }
        }
        cx.notify();
    }

    /// Saved photo paths land on their target: staged on the draft, or
    /// appended to the item's list. `merge_attachments` caps both at 12.
    fn planner_attach(
        &mut self,
        target: crate::views_planner::PlannerAttachTarget,
        paths: Vec<std::path::PathBuf>,
        cx: &mut Context<Self>,
    ) {
        use crate::views_planner::{merge_attachments, PlannerAttachTarget};
        if paths.is_empty() {
            return;
        }
        let added: Vec<String> = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        match target {
            PlannerAttachTarget::Draft => {
                self.planner_draft.attachments =
                    merge_attachments(&self.planner_draft.attachments, &added);
            }
            PlannerAttachTarget::Item(id) => {
                let current = crate::views_planner::planner_items()
                    .0
                    .into_iter()
                    .find(|row| row.id == id)
                    .map(|row| row.attachments)
                    .unwrap_or_default();
                let next = merge_attachments(&current, &added);
                self.commit(
                    "plan.update",
                    &format!("plan:{id}"),
                    json!({ "attachments": next }),
                );
            }
        }
        cx.notify();
    }

    /// The paperclip: the native file prompt, then each picked image is
    /// imported into the media store off the UI thread and attached.
    pub(crate) fn planner_pick_photos(
        &mut self,
        target: crate::views_planner::PlannerAttachTarget,
        cx: &mut Context<Self>,
    ) {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach photo".into()),
        });
        let media = Self::planner_media_dir();
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let saved = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .iter()
                        .filter(|path| {
                            slate_app::attachments::classify_media(path)
                                == slate_app::attachments::MediaKind::Image
                        })
                        .filter_map(|path| {
                            slate_app::attachments::import_media(path, &media).ok()
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| this.planner_attach(target, saved, cx));
        })
        .detach();
    }

    /// ⌘/Ctrl+V into the open field: a clipboard photo attaches to the field's
    /// target (the draft, or the row it belongs to); text inserts at the caret.
    fn planner_paste(&mut self, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        let images: Vec<(Vec<u8>, String)> = item
            .entries()
            .iter()
            .filter_map(|entry| match entry {
                gpui::ClipboardEntry::Image(image) => Some((
                    image.bytes().to_vec(),
                    format!("{:?}", image.format()).to_lowercase(),
                )),
                _ => None,
            })
            .collect();
        if !images.is_empty() {
            let Some(target) = self
                .planner_editing
                .as_ref()
                .map(|edit| edit.field.attach_target())
            else {
                return;
            };
            let dir = Self::planner_media_dir();
            let saved: Vec<std::path::PathBuf> = images
                .iter()
                .filter_map(|(bytes, ext)| {
                    slate_app::attachments::save_bytes(bytes, ext, &dir).ok()
                })
                .collect();
            self.planner_attach(target, saved, cx);
        } else if let Some(text) = item.text() {
            self.planner_edit_insert(&text);
        }
        cx.notify();
    }

    /// The media store (`{user data}/media`) that attachments are saved into.
    fn planner_media_dir() -> std::path::PathBuf {
        slate_app::ipc::user_data_dir().join("media")
    }

    // Caret edits on the open field. `cursor` is always a char boundary: every
    // step walks by `char`, so the byte slicing below cannot split one.

    fn planner_edit_insert(&mut self, text: &str) {
        if let Some(edit) = self.planner_editing.as_mut() {
            edit.buffer.insert_str(edit.cursor, text);
            edit.cursor += text.len();
        }
    }

    fn planner_edit_backspace(&mut self) {
        if let Some(edit) = self.planner_editing.as_mut() {
            if let Some((prev, _)) = edit.buffer[..edit.cursor].char_indices().next_back() {
                edit.buffer.replace_range(prev..edit.cursor, "");
                edit.cursor = prev;
            }
        }
    }

    fn planner_edit_delete(&mut self) {
        if let Some(edit) = self.planner_editing.as_mut() {
            if let Some(next) = edit.buffer[edit.cursor..].chars().next() {
                let end = edit.cursor + next.len_utf8();
                edit.buffer.replace_range(edit.cursor..end, "");
            }
        }
    }

    fn planner_edit_step(&mut self, forward: bool) {
        if let Some(edit) = self.planner_editing.as_mut() {
            edit.cursor = if forward {
                edit.buffer[edit.cursor..]
                    .chars()
                    .next()
                    .map_or(edit.cursor, |c| edit.cursor + c.len_utf8())
            } else {
                edit.buffer[..edit.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(edit.cursor, |(i, _)| i)
            };
        }
    }

    fn planner_edit_edge(&mut self, end: bool) {
        if let Some(edit) = self.planner_editing.as_mut() {
            edit.cursor = if end { edit.buffer.len() } else { 0 };
        }
    }

    /// Fire-and-toast control write for panes without their own error
    /// surface — the notes pane's PATCH/POSTs land here.
    fn control_post(&mut self, method: &str, path: &str, body: Option<Value>) {
        if let Err(error) = slate_app::ipc::control_request(method, path, body.as_ref()) {
            self.notice(error);
        }
    }

    // fix-orchestration: a control write that reports back. A rejected POST
    // becomes the pane's banner (the widget's `actionError`) instead of a
    // toast; a success clears it.
    fn orc_post(&mut self, method: &str, path: &str, body: Option<Value>) -> bool {
        match slate_app::ipc::control_request(method, path, body.as_ref()) {
            Ok(_) => {
                self.orc_error = None;
                true
            }
            Err(error) => {
                self.orc_error = Some(error);
                false
            }
        }
    }

    // fix-orchestration: a sent reply or decision drops its draft and
    // releases the field that typed it.
    fn orc_forget(&mut self, id: &str) {
        self.orc_drafts.remove(id);
        if self.orc_editing.as_ref().map(|e| e.message_id()) == Some(id) {
            self.orc_editing = None;
        }
    }

    // fix-orchestration: the reply/decision message to an ask, addressed back
    // to the asker — the lookup and payload `slate reply` / `slate allow`
    // build.
    fn orc_reply(&mut self, ask_id: &str, body: String, subject: &str) {
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
        if self.orc_post("POST", "/orchestration/messages", Some(payload)) {
            self.orc_forget(ask_id);
        }
    }

    // fix-orchestration: `orc_ask_reply` — empty text is refused, the way the
    // widget's Send button is disabled until there is something to send.
    fn orc_send_ask(&mut self, ask_id: &str, body: &str) {
        if !body.trim().is_empty() {
            self.orc_reply(ask_id, body.to_owned(), "reply");
        }
    }

    // fix-orchestration: keystrokes while a reply/note field is armed. Typing
    // appends, Backspace trims, Enter sends a reply (a note waits for its
    // Allow/Deny button), Escape releases. An armed field whose message is no
    // longer pending drops itself so it never holds keys for a card that's
    // gone. Returns false when no field owns the keyboard.
    fn orc_edit_key(&mut self, keystroke: &Keystroke, mods: &Modifiers) -> bool {
        let Some(edit) = self.orc_editing.clone() else {
            return false;
        };
        let id = edit.message_id().to_owned();
        if !self.orc_field_live(&id) {
            self.orc_editing = None;
            return false;
        }
        match keystroke.key.as_str() {
            "escape" => self.orc_editing = None,
            "enter" => {
                self.orc_editing = None;
                if let crate::views_orchestration::OrcEdit::Reply(_) = edit {
                    let body = self.orc_drafts.get(&id).cloned().unwrap_or_default();
                    self.orc_send_ask(&id, &body);
                }
            }
            "backspace" => {
                if let Some(draft) = self.orc_drafts.get_mut(&id) {
                    draft.pop();
                }
            }
            _ => {
                if !mods.control && !mods.alt && !mods.platform {
                    let text = keystroke.key_char.clone().or_else(|| {
                        match keystroke.key.as_str() {
                            "space" => Some(" ".to_owned()),
                            k if k.chars().count() == 1 => Some(k.to_owned()),
                            _ => None,
                        }
                    });
                    if let Some(text) = text {
                        self.orc_drafts.entry(id).or_default().push_str(&text);
                    }
                }
            }
        }
        true
    }

    // fix-orchestration: an orchestration pane is still open and the message
    // is still on the store and unanswered — the same test the pane's cards
    // use to list it.
    fn orc_field_live(&self, id: &str) -> bool {
        if !self
            .canvas
            .widgets
            .values()
            .any(|w| w.kind.as_deref() == Some("orchestration"))
        {
            return false;
        }
        let store = load_orchestration_store();
        store
            .message_by_id(id)
            .is_some_and(|m| crate::views_orchestration::pending(m, &store))
    }

    /// fix-panes: Enter in the notes editor. The field goes out as a PATCH to
    /// `/notes/{id}` — the `note.update` the REST route journals. An empty
    /// category sends JSON null, which the route reads as "clear it".
    fn notes_commit_edit(&mut self) {
        let Some((note, field)) = self.notes_editing.take() else {
            return;
        };
        let value = std::mem::take(&mut self.notes_buffer);
        let patch = match field.as_str() {
            "title" => json!({ "title": value }),
            "body" => json!({ "body": value }),
            "tags" => json!({
                "tags": value
                    .split(',')
                    .map(str::trim)
                    .filter(|tag| !tag.is_empty())
                    .collect::<Vec<_>>(),
            }),
            "category" => json!({ "category": Some(value.trim()).filter(|name| !name.is_empty()) }),
            _ => return,
        };
        self.control_post("PATCH", &format!("/notes/{note}"), Some(patch));
    }
}
