//! Code view — the dedicated agent-terminal screen that replaces the
//! canvas, ported from `src/renderer/src/components/CodeView.tsx`,
//! `CodeLauncher.tsx` and `src/renderer/src/lib/codeLayout.ts`.
//!
//! In the shell the view was an `absolute inset-0 pt-10` overlay beside the
//! sidebar: a CSS-grid of `SessionCard`s (`data-testid="code-session"`),
//! each a 24px `code-session-header` (agent icon, title, Finished pill,
//! launcher `+`, maximize, `✕`) over an xterm body, plus the `CodeLauncher`
//! dialog (`role="dialog"`, `aria-label="Launch Code Session"`) that picked
//! an agent from `CODE_AGENTS` and a launch count. The grid's `auto` layout
//! was `autoCodeLayout` — a hand-tuned shape per session count; that logic
//! already lives natively in `slate_app::code_layout::arrange`, which this
//! file drives with pixel rects instead of `gridTemplateColumns`.
//!
//! The v1 port keeps the frame — title strip, launcher row, the card grid,
//! per-card header (agent monogram + status dot + `✕`) and the styled-rows
//! terminal body — and defers, flagged for a v2:
//!
//! - **Drag-to-swap / the 3-way split's draggable separators** — the
//!   original's `text/session-id` drag-and-drop and `threeWaySplit`
//!   separators; `code_layout::three_way` already takes the ratios.
//! - **Maximize/restore** — the original's `absolute inset-0` card and
//!   double-click header.
//! - **Rename, copy-name, per-card `+`, Finished pill text** — the header
//!   keeps the dot only.
//! - **Launcher dialog parity** — launch counts (`CODE_LAUNCH_COUNTS`),
//!   the custom-command input, memory warnings and the recent-folders
//!   picker. The port's launcher row launches one session per click.
//! - **Restore/resume** — `pendingRestore`, `ResumeAgents`, session
//!   persistence.
//!
//! Dispatched through `CanvasView::code_command(action, json!, cx)`:
//!
//! - `"launcher"` `{}` — toggles `CodeState::launcher_open` (the header's
//!   `+` button).
//! - `"launch"` `{"agent": "<id>", "command": "<cmd>"}` — opens one
//!   session running `command`. `command` is `""` for the `custom` agent
//!   ("Other CLI") — the coordinator should prompt for it, the original's
//!   `customCommand` input. `agent == "browser"` opened a webview widget
//!   in the original, not a PTY.
//! - `"layout"` `{"mode": "auto"|"grid"|"columns"|"rows"|"focus"}` — the
//!   title bar's `CodeLayoutMode` picker; the strip button cycles.
//! - `"close_session"` `{"id": "<session-id>"}` — the `✕`, kills the PTY
//!   and drops the card.
//! - `"focus_session"` `{"id": "<session-id>"}` — click on a card;
//!   `CodeState::focused` draws the accent hairline and (in `focus`
//!   layout) picks the enlarged card.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::json;
use slate_app::code_layout::{self, CodeLayout};
use slate_app::theme;

/// One card on the grid — the original's `Session { id, agent, title,
/// status }`, flattened: `agent` kept the launch record (id/label/command)
/// and `alive` maps `status === 'active'`.
#[derive(Clone, Debug)]
pub struct CodeSession {
    /// `code-<ts>-<n>` — identifies the card; the `close_session` /
    /// `focus_session` payload key.
    pub id: String,
    /// `agent.id` — `claude`, `codex`, `browser`, `custom`, …
    pub agent_id: String,
    /// `session.title || agent.label` — the header's truncated text.
    pub label: String,
    /// The command the session was launched with (`agent.command` or the
    /// custom entry); kept so a relaunch doesn't re-ask.
    pub command: String,
    /// The `TerminalSnapshot::id` the body paints.
    pub terminal_id: String,
    /// `status === 'active'` — the green/gray header dot.
    pub alive: bool,
}

/// Everything the pane renders, filled by `CanvasView` each frame — same
/// role as `views_toolbar::ToolbarState`.
pub struct CodeState {
    /// The cards, in grid order (`stableCardOrder` in the original).
    pub sessions: Vec<CodeSession>,
    /// The focused session's `id` — accent hairline + the `focus`
    /// layout's enlarged card.
    pub focused: Option<String>,
    /// Window size — the grid area derives from it (`viewport` minus the
    /// fixed header rows).
    pub viewport: Size<Pixels>,
    /// The `+` launcher row's open flag (`launcherOpen`).
    pub launcher_open: bool,
    /// The title-bar layout mode (`layoutMode`); `Auto` is
    /// `autoCodeLayout`.
    pub layout: CodeLayout,
    /// The workspace folder's basename for the empty state — the
    /// original's `workspaceDir` line. `None` while unread.
    pub workspace_name: Option<String>,
}

/// `CodeAgent` from `CodeLauncher.tsx` — the launcher's pick list.
/// `icon` names the vendored brand mark (`assets::icon`), the same SVG
/// paths the shell's `agent-icons.tsx` drew; `browser`/`custom` fall back
/// to the Lucide `globe`/`terminal` the original's `Icon` field used.
pub(crate) struct CodeAgent {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) command: &'static str,
    pub(crate) icon: &'static str,
}

/// `CODE_AGENTS` — same order as the dialog's `choices`, with this
/// machine's own `gray` harness pinned at the top of the picker.
pub(crate) const CODE_AGENTS: &[CodeAgent] = &[
    CodeAgent { id: "gray", label: "Gray", command: "gray", icon: "agent-gray" },
    CodeAgent { id: "claude", label: "Claude Code", command: "claude", icon: "agent-claude" },
    CodeAgent { id: "codex", label: "Codex", command: "codex", icon: "agent-codex" },
    CodeAgent { id: "antigravity", label: "Antigravity", command: "agy", icon: "agent-antigravity" },
    CodeAgent { id: "grok", label: "Grok Build", command: "grok", icon: "agent-grok" },
    CodeAgent { id: "opencode", label: "OpenCode", command: "opencode", icon: "agent-opencode" },
    CodeAgent { id: "kimi", label: "Kimi Code", command: "kimi", icon: "agent-kimi" },
    CodeAgent { id: "cursor", label: "Cursor Agent", command: "cursor-agent", icon: "agent-cursor" },
    CodeAgent { id: "browser", label: "Browser", command: "browser", icon: "globe" },
    CodeAgent { id: "custom", label: "Other CLI", command: "", icon: "terminal" },
];

/// The title strip's height — the view's own chrome under the title bar.
pub(crate) const HEADER_H: f32 = 34.0;
/// The launcher row's height while `launcher_open`.
pub(crate) const LAUNCHER_H: f32 = 44.0;
/// `code-session-header` — `h-6` in the original.
pub(crate) const CARD_HEADER_H: f32 = 24.0;
/// Card header buttons — `h-[18px] w-[18px]`.
const CARD_BTN: f32 = 18.0;
/// Terminal font — `widget_body`'s 12px at zoom 1.
const TERMINAL_FONT: f32 = 12.0;
/// `code-terminal-shell` — the card fill (`bg-bg-panel` token).
const CARD_BG: u32 = 0x121212;

/// CellRgb (r,g,b) packs into the u32 gpui's rgb() takes — the same
/// helper `canvas_view::pack` is.
fn pack((r, g, b): slate_app::terminal_screen::CellRgb) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | b as u32
}

/// The styled-rows painter — `canvas_view::widget_body`'s terminal branch
/// (lines ~3524–3595) verbatim, with `zoom` folded into `font_px`: the
/// code grid doesn't scale, the canvas did. Rows are `StyledRun` spans in
/// "Departure Mono"; all-whitespace runs (the cursor cell) get NBSPs so
/// the shaper keeps their width; the drag-selection overlay is the sand
/// accent at ~30% (xterm drew `rgba(120,160,255,0.35)`) over the same
/// `p_1`-inset cell geometry. A
/// terminal with no snapshot yet (or a blank screen) draws "…".
fn terminal_surface(
    snapshot: Option<&crate::engine::TerminalSnapshot>,
    font_px: f32,
) -> impl IntoElement {
    let font = px(font_px);
    let mut body = div()
        .size_full()
        .p_1()
        .font_family("Departure Mono")
        .text_size(font)
        .line_height(gpui::px(font_px * 17.0 / 12.0))
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
                // All-whitespace runs (notably the cursor cell) get NBSPs
                // so the text shaper keeps their width.
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
    // The drag-selection highlight — the sand accent where xterm's
    // selectionBackground was rgba(120,160,255,0.35). Cells ride the
    // body's p_1 padding, so the
    // overlay starts at the padding edge; the cell geometry is
    // `terminal_cell`'s inverse at font_px/12 scale.
    let mut container = div().size_full().relative().child(body);
    if let Some(snapshot) = snapshot {
        if !snapshot.selection.is_empty() {
            let scale = font_px / 12.0;
            let cell_w = px(7.64 * scale);
            let cell_h = px(12.0 * scale);
            let mut overlay = div().absolute().left(px(4.0)).top(px(4.0));
            for &(row, start, end) in &snapshot.selection {
                overlay = overlay.child(
                    div()
                        .absolute()
                        .left(cell_w * start as f32)
                        .top(cell_h * row as f32)
                        .w(cell_w * (end - start) as f32)
                        .h(cell_h)
                        .bg(rgba((theme::hex(theme::accent::SAND) << 8) | 0x4D)),
                );
            }
            container = container.child(overlay);
        }
    }
    container
}

/// The brand mark in its `grid h-4 w-4 place-items-center` badge — a
/// bordered square tile (graysite's blocky chip) holding the vendored
/// `agent-icons.tsx` SVG, `session.agent.Icon size={12}` in the shell.
fn agent_badge(icon_name: &'static str, size: f32) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(size))
        .h(px(size))
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
        .flex()
        .items_center()
        .justify_center()
        .child(crate::assets::icon(
            icon_name,
            size * 0.62,
            theme::hex(theme::text::DIM),
        ))
}

/// A card's badge icon: the `CODE_AGENTS` mark for a known `agent_id`,
/// else the `terminal` glyph a custom entry carried.
fn session_icon(session: &CodeSession) -> &'static str {
    CODE_AGENTS
        .iter()
        .find(|a| a.id == session.agent_id)
        .map(|a| a.icon)
        .unwrap_or("terminal")
}

/// One launch chip — the dialog's agent grid button compressed to a row
/// item: badge + label, `border-line-soft bg-bg-panel`, hover to
/// `bg-bg-hover`. Click dispatches `"launch"` with the agent's id and
/// command (empty for `custom`, the original's `customCommand` path).
fn agent_button(
    agent: &'static CodeAgent,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    div()
        .id(format!("slate-code-agent-{}", agent.id))
        .flex_none()
        .h(px(28.0))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
        .px(px(10.0))
        .font_family("Departure Mono")
        .text_size(px(11.0))
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .cursor_pointer()
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(agent_badge(agent.icon, 16.0))
        .child(agent.label.to_uppercase())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.code_command(
                    "launch",
                    json!({ "agent": agent.id, "command": agent.command }),
                    cx,
                );
            }),
        )
}

/// One grid card — `SessionCard` (`data-testid="code-session"`): a 24px
/// header (`code-session-header`) over the terminal body. The original's
/// header was also the drag handle and focused the session on click; the
/// port keeps the focus and drops the drag. `rect` is the cell
/// `code_layout::arrange` computed for this card.
fn session_card(
    session: &CodeSession,
    focused: bool,
    rect: slate_app::canvas::Rect,
    snapshots: &[crate::engine::TerminalSnapshot],
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let icon = session_icon(session);
    let id = session.id.clone();
    let close_id = session.id.clone();
    let snapshot = snapshots.iter().find(|s| s.id == session.terminal_id);
    // The snapshot's `alive` is the authoritative PTY read; the session
    // flag is the fallback until the first snapshot lands.
    let alive = snapshot.map(|s| s.alive).unwrap_or(session.alive);

    // `code-session-header` — `h-6 … gap-2 border-b border-line-soft
    // px-1.5`. Left: agent badge + truncated title; right: the status dot
    // (green live, gray finished — the original's `Finished` pill reduced
    // to its dot) and the `✕` close.
    let header = div()
        .flex_none()
        .h(px(CARD_HEADER_H))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .px(px(6.0))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .text_size(px(11.0))
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(agent_badge(icon, 12.0))
                .child(div().min_w_0().truncate().child(session.label.clone())),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                // `h-1.5 w-1.5 rounded-pill` — green while `alive`, the
                // finished pill's `bg-text-faint` dot otherwise.
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(rgb(theme::hex(if alive {
                            theme::status::OK
                        } else {
                            theme::text::FAINT
                        }))),
                )
                // `✕` — `grid h-[18px] w-[18px] place-items-center`
                // (rounded-pill in the shell, square here), hover
                // `bg-bg-hover`.
                .child(
                    div()
                        .id(format!("slate-code-close-{id}"))
                        .flex_none()
                        .w(px(CARD_BTN))
                        .h(px(CARD_BTN))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                        .child(crate::assets::icon(
                            "x",
                            11.0,
                            theme::hex(theme::text::FAINT),
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "close_session",
                                    json!({ "id": close_id }),
                                    cx,
                                );
                            }),
                        ),
                ),
        );

    // `flex h-full min-h-0 min-w-0 flex-col overflow-hidden rounded-panel
    // border` — square here, placed absolutely into the grid cell. The
    // focused card's border is the sand accent hairline; a card click
    // is the original header/body `onFocus`.
    div()
        .id(format!("slate-code-session-{id}"))
        .absolute()
        .left(px(rect.left()))
        .top(px(rect.top()))
        .w(px(rect.width().max(0.0)))
        .h(px(rect.height().max(0.0)))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_1()
        .border_color(rgb(theme::hex(if focused {
            theme::accent::SAND
        } else {
            theme::hairline::FAINT
        })))
        .bg(rgb(CARD_BG))
        .child(header)
        .child({
            // The body owns its presses — focus plus the xterm semantics
            // (selection drag / mouse-report buttons) run through
            // `code_body_down`, while a header click stays focus-only on
            // the card-level handler below.
            let body_id = session.id.clone();
            let body_id_m = session.id.clone();
            let body_id_r = session.id.clone();
            div()
                .flex_1()
                .min_h_0()
                .bg(rgb(theme::hex(theme::monochrome::BASE)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        this.code_body_down(
                            &body_id,
                            MouseButton::Left,
                            slate_app::canvas::Pos2::new(
                                f32::from(event.position.x),
                                f32::from(event.position.y),
                            ),
                            event.modifiers,
                            cx,
                        );
                    }),
                )
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        this.code_body_down(
                            &body_id_m,
                            MouseButton::Middle,
                            slate_app::canvas::Pos2::new(
                                f32::from(event.position.x),
                                f32::from(event.position.y),
                            ),
                            event.modifiers,
                            cx,
                        );
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _window, cx| {
                        cx.stop_propagation();
                        this.code_body_down(
                            &body_id_r,
                            MouseButton::Right,
                            slate_app::canvas::Pos2::new(
                                f32::from(event.position.x),
                                f32::from(event.position.y),
                            ),
                            event.modifiers,
                            cx,
                        );
                    }),
                )
                .child(terminal_surface(snapshot, TERMINAL_FONT))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.code_command("focus_session", json!({ "id": id }), cx);
            }),
        )
}

/// The empty grid — `sessions.length === 0` state, the original's
/// centred `max-w-[640px]` column reduced to its agent pick: heading,
/// hint, then the launch chips. (The workspace-folder gate and
/// `ResumeAgents` are v2.)
fn empty_state(
    state: &CodeState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut agents = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_2()
        .justify_center();
    for agent in CODE_AGENTS {
        agents = agents.child(agent_button(agent, cx));
    }
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .child(
            div()
                .w(px(420.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .font_family("Brygada 1918")
                        .text_size(px(20.0))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child("No code sessions"),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(match &state.workspace_name {
                            Some(name) => format!("Sessions open in {name}. Pick an agent."),
                            None => "Pick an agent to open a session.".to_owned(),
                        }),
                )
                .child(div().pt_2().child(agents)),
        )
}

/// The view itself — `data-testid="code-view"`: a fixed title strip over
/// the session grid. The original carried no strip of its own (title-bar
/// tabs did the work); the port adds one so the view is self-describing —
/// "Code", the session count, the layout-mode cycler and the `+` that
/// opens the launcher row.
pub fn code_pane(
    state: &CodeState,
    snapshots: &[crate::engine::TerminalSnapshot],
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let count = state.sessions.len();

    // ── Title strip ─────────────────────────────────────────────────────
    // Left: "Code" + `n sessions`. Right: the layout label — click cycles
    // `CodeLayout::ALL`, the title bar's mode picker — and the `+` button.
    let layout_index = CodeLayout::ALL
        .iter()
        .position(|mode| *mode == state.layout)
        .unwrap_or(0);
    let next_layout = CodeLayout::ALL[(layout_index + 1) % CodeLayout::ALL.len()];
    let header = div()
        .flex_none()
        .h(px(HEADER_H))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
        .px_3()
        .child(
            div()
                .font_family("Brygada 1918")
                .text_size(px(15.0))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child("Code"),
        )
        .child(
            div()
                .font_family("Departure Mono")
                .text_size(px(10.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!(
                    "{count} session{}",
                    if count == 1 { "" } else { "s" }
                )),
        )
        .child(
            div()
                .ml_auto()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                // The layout cycler — `code_command("layout", {"mode": _})`.
                .child(
                    div()
                        .id("slate-code-layout")
                        .flex_none()
                        .h(px(24.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .px_2()
                        .font_family("Departure Mono")
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .cursor_pointer()
                        .hover(|el| {
                            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        })
                        .child(crate::assets::icon(
                            "layout-grid",
                            12.0,
                            theme::hex(theme::text::FAINT),
                        ))
                        .child(state.layout.label().to_uppercase())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "layout",
                                    json!({ "mode": next_layout }),
                                    cx,
                                );
                            }),
                        ),
                )
                // `+` — `aria-label="Open another CLI or browser"` grown
                // to the strip's launcher toggle.
                .child(
                    div()
                        .id("slate-code-launcher")
                        .flex_none()
                        .w(px(24.0))
                        .h(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .when(state.launcher_open, |el| {
                            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        })
                        .when(!state.launcher_open, |el| {
                            el.text_color(rgb(theme::hex(theme::text::DIM))).hover(|el| {
                                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                            })
                        })
                        .child("+")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command("launcher", json!({}), cx);
                            }),
                        ),
                ),
        );

    // ── Launcher row — `launcherOpen`'s pick list, inline ───────────────
    // The original's `CodeLauncher` dialog in one row: every `CODE_AGENTS`
    // entry, one session per click (launch counts are v2).
    let mut root = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::hex(theme::monochrome::BASE)))
        .child(header);
    if state.launcher_open {
        let mut row = div()
            .flex_none()
            .h(px(LAUNCHER_H))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
            .px_3()
            .child(
                div()
                    .flex_none()
                    .font_family("Departure Mono")
                    .text_size(px(10.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("LAUNCH"),
            );
        for agent in CODE_AGENTS {
            row = row.child(agent_button(agent, cx));
        }
        root = root.child(row);
    }

    // ── The grid ────────────────────────────────────────────────────────
    // `relative grid … overflow-auto` — `code_layout::arrange` answers
    // each card's rect, the port of `autoCodeLayout`/`codeGridLayout`.
    if count == 0 {
        root.child(empty_state(state, cx))
    } else {
        let width = f32::from(state.viewport.width);
        let chrome = HEADER_H + if state.launcher_open { LAUNCHER_H } else { 0.0 };
        let height = (f32::from(state.viewport.height)
            - slate_app::canvas::TITLE_BAR_HEIGHT
            - chrome)
        .max(0.0);
        let focus_index = state
            .sessions
            .iter()
            .position(|s| Some(&s.id) == state.focused.as_ref())
            .unwrap_or(0);
        let area = slate_app::canvas::Rect::from_min_size(
            slate_app::canvas::Pos2::ZERO,
            slate_app::canvas::Vec2::new(width, height),
        );
        let rects = code_layout::arrange(state.layout, area, count, focus_index);
        let mut grid = div()
            .id("code-grid")
            .flex_1()
            .min_h_0()
            .relative()
            // The shell's `overflow-auto`: an arranged grid larger than the
            // pane scrolls instead of clipping cards unreachable.
            .overflow_scroll()
            .bg(rgb(theme::hex(theme::monochrome::RAISED)));
        for (session, rect) in state.sessions.iter().zip(rects) {
            grid = grid.child(session_card(
                session,
                Some(&session.id) == state.focused.as_ref(),
                rect,
                snapshots,
                cx,
            ));
        }
        root.child(grid)
    }
}
