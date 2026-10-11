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
use slate_app::code_resume::{self, AgentConversation};
use slate_app::theme;
use std::collections::{HashMap, HashSet};

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
    /// t3code's pinned thread — floats to the top of the grid.
    pub pinned: bool,
    /// t3code's settled thread — folded into the SETTLED shelf.
    pub settled: bool,
    /// The `git worktree` path this session owns, when launched with the
    /// worktree option (t3code's "New worktree" thread).
    pub worktree: Option<String>,
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
    /// Its full path — the resume scan's `dir`.
    pub workspace_dir: Option<String>,
    /// `ResumeAgents` — the conversations scanned for `workspace_dir`,
    /// newest first (dismissed rows are filtered at draw).
    pub conversations: Vec<AgentConversation>,
    /// A scan is in flight for `workspace_dir`.
    pub resume_loading: bool,
    /// `expanded` — collapsed shows `code_resume::COLLAPSED_ROWS`.
    pub resume_expanded: bool,
    /// Ticked rows — `conversation_key`s. Empty means "no choice made".
    pub resume_selected: HashSet<String>,
    /// Keys the user waved away for this folder; persisted per dir.
    pub resume_dismissed: HashSet<String>,
    /// `selectedAgentIds` — the ticked agent chips, in pick order. The
    /// empty-state grid and the launcher row edit the same set.
    pub selected_agents: Vec<String>,
    /// `agentCounts` — how many of each picked agent to open.
    pub agent_counts: HashMap<String, u32>,
    /// `activeAgentId` — the chip the count row edits (last picked).
    pub active_agent: Option<String>,
    /// The user's `/agent`-added CLIs (`UiPreferences::custom_agents`),
    /// rendered after the built-ins in both pick lists.
    pub custom_agents: Vec<slate_app::ui_preferences::CustomAgent>,
    /// herdr's pane classifier, keyed by `terminal_id` — the card dots'
    /// `{working, blocked, idle, unknown}` instead of the binary `alive`.
    pub agent_states: HashMap<String, slate_app::agent_state::AgentState>,
    /// The launcher's worktree toggle — each launched session gets its
    /// own `git worktree`, t3code's "New worktree" default.
    pub worktree: bool,
}

/// `CODE_LAUNCH_COUNTS` — the "How many" row.
pub(crate) const LAUNCH_COUNTS: [u32; 7] = [1, 2, 4, 6, 8, 10, 12];

/// `INLINE_AGENTS` — the pickable grid: the built-ins plus the user's
/// custom entries, which the callers append from `CodeState`.
pub(crate) fn inline_agents() -> impl Iterator<Item = &'static CodeAgent> {
    CODE_AGENTS.iter()
}

fn agent_icon(agent_id: &str) -> &'static str {
    CODE_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| a.icon)
        .or_else(|| {
            KNOWN_CLI
                .iter()
                .find(|(id, _, _)| *id == agent_id)
                .map(|(_, _, icon)| *icon)
        })
        .unwrap_or("terminal")
}

/// Label lookup across the built-ins, the user's custom chips, and the
/// resume-only `KNOWN_CLI` marks — in that order.
fn agent_label(state: &CodeState, agent_id: &str) -> String {
    CODE_AGENTS
        .iter()
        .find(|a| a.id == agent_id)
        .map(|a| a.label.to_owned())
        .or_else(|| {
            state
                .custom_agents
                .iter()
                .find(|a| a.id == agent_id)
                .map(|a| a.label.clone())
        })
        .or_else(|| {
            KNOWN_CLI
                .iter()
                .find(|(id, _, _)| *id == agent_id)
                .map(|(_, label, _)| (*label).to_owned())
        })
        .unwrap_or_else(|| {
            if agent_id == "custom" {
                "Other".to_owned()
            } else {
                agent_id.to_owned()
            }
        })
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

/// `CODE_AGENTS` — the built-in pick list. Exactly one harness ships:
/// this machine's own `gray`. Every other CLI is a user-added
/// `UiPreferences::custom_agents` entry, made through the "+ Other" chip
/// (the `/agent` command-bar verb) — T3 Code's provider model, kept at
/// PTY fidelity.
pub(crate) const CODE_AGENTS: &[CodeAgent] = &[CodeAgent {
    id: "gray",
    label: "Gray",
    command: "gray",
    icon: "agent-gray",
}];

/// Display marks for CLIs the resume scan can still surface — these are
/// not launch chips, only the badge a scanned conversation row wears.
/// `(id, label, icon)`.
pub(crate) const KNOWN_CLI: &[(&str, &str, &str)] = &[
    ("claude", "Claude Code", "agent-claude"),
    ("codex", "Codex", "agent-codex"),
    ("antigravity", "Antigravity", "agent-antigravity"),
    ("grok", "Grok Build", "agent-grok"),
    ("opencode", "OpenCode", "agent-opencode"),
    ("kimi", "Kimi Code", "agent-kimi"),
    ("cursor", "Cursor Agent", "agent-cursor"),
    ("browser", "Browser", "globe"),
];

/// The title strip's height — the view's own chrome under the title bar.
pub(crate) const HEADER_H: f32 = 34.0;
/// The launcher panel's height while `launcher_open` — two lines: the
/// pickable agent chips, then the count row and Launch button.
pub(crate) const LAUNCHER_H: f32 = 92.0;
/// `code-session-header` — `h-6` in the original.
pub(crate) const CARD_HEADER_H: f32 = 24.0;
/// Card header buttons — `h-[18px] w-[18px]`.
const CARD_BTN: f32 = 18.0;
/// Terminal font — `widget_body`'s FONT_PX at zoom 1.
const TERMINAL_FONT: f32 = slate_app::terminal_screen::FONT_PX;
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
    let scale = font_px / slate_app::terminal_screen::FONT_PX;
    let cell_h = px(slate_app::terminal_screen::CELL_H * scale);
    let mut body = div()
        .size_full()
        .p_1()
        .font_family(slate_app::terminal_screen::FONT_FAMILY)
        .text_size(font)
        .line_height(cell_h)
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
            let mut line = div().h(cell_h).flex().flex_row().flex_none();
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
    // `terminal_cell`'s inverse at font_px/FONT_PX scale.
    let mut container = div().size_full().relative().child(body);
    if let Some(snapshot) = snapshot {
        if !snapshot.selection.is_empty() {
            let cell_w = px(slate_app::terminal_screen::CELL_W * scale);
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

/// One selectable agent chip — the empty-state grid's `INLINE_AGENTS`
/// button and the launcher row's entries: icon + label, a text-colored dot
/// when ticked (the original's `bg-text` pip). Click dispatches
/// `"toggle_agent"` — selection, not a launch.
fn agent_chip(
    agent_id: &str,
    label: &str,
    icon: &'static str,
    selected: bool,
    wide: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let id = agent_id.to_owned();
    let scope = if wide { "grid" } else { "row" };
    let mut chip = div()
        .id(format!("slate-code-agent-{scope}-{id}"))
        .flex_none()
        .h(px(38.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .border_1()
        .px_3()
        .cursor_pointer()
        .child(crate::assets::icon(
            icon,
            15.0,
            if selected {
                theme::hex(theme::text::NORMAL)
            } else {
                theme::hex(theme::text::DIM)
            },
        ))
        .child(
            div()
                .text_size(px(13.0))
                .text_color(rgb(if selected {
                    theme::hex(theme::text::NORMAL)
                } else {
                    theme::hex(theme::text::DIM)
                }))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(label.to_owned()),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.code_command("toggle_agent", json!({ "id": id }), cx);
            }),
        );
    if wide {
        // `grid-cols-3` in a 640px column with gap-2 — equal thirds.
        chip = chip.w(px(208.0));
    }
    if selected {
        chip = chip
            .border_color(rgb(theme::hex(theme::accent::SAND)))
            .bg(rgb(theme::hex(theme::monochrome::RAISED)))
            .child(
                div()
                    .ml_auto()
                    .w(px(7.0))
                    .h(px(7.0))
                    .rounded_full()
                    .flex_none()
                    .bg(rgb(theme::hex(theme::text::NORMAL))),
            );
    } else {
        chip = chip
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            });
    }
    chip
}

/// A caps section label — `text-[11px] font-semibold tracking-[0.08em]
/// text-text-faint uppercase`, the original's "AGENT" / "HOW MANY" / "WILL
/// LAUNCH" headers.
fn section_label(text: &'static str) -> Div {
    div()
        .mb(px(10.0))
        .font_family("Departure Mono")
        .text_size(px(11.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::hex(theme::text::FAINT)))
        .child(text.to_uppercase())
}

/// One count square in the "How many" row — `grid h-10 w-10` in the
/// original. Current = the active agent's stored count.
fn count_chip(
    count: u32,
    current: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut chip = div()
        .id(format!("slate-code-count-{count}"))
        .w(px(40.0))
        .h(px(40.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .text_size(px(13.0))
        .cursor_pointer()
        .child(count.to_string())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.code_command("set_count", json!({ "count": count }), cx);
            }),
        );
    if current {
        chip = chip
            .border_color(rgb(theme::hex(theme::accent::SAND)))
            .bg(rgb(theme::hex(theme::monochrome::RAISED)))
            .text_color(rgb(theme::hex(theme::text::NORMAL)));
    } else {
        chip = chip
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            });
    }
    chip
}

/// The "How many" row + "Will launch" summary + the Launch button — the
/// empty state's bottom half, shared shape with the launcher row's second
/// line.
fn selection_controls(
    state: &CodeState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Vec<AnyElement> {
    let mut out: Vec<AnyElement> = Vec::new();
    let active = state.active_agent.as_deref();
    let current_count = active
        .and_then(|id| state.agent_counts.get(id).copied())
        .unwrap_or(1);

    // "How many" — `CODE_LAUNCH_COUNTS` buttons + "for <label>".
    let mut counts = div()
        .mb_8()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .flex_wrap();
    for count in LAUNCH_COUNTS {
        counts = counts.child(count_chip(count, count == current_count, cx));
    }
    counts = counts.child(
        div()
            .ml_2()
            .text_size(px(12.0))
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .child(format!(
                "for {}",
                active.map(|id| agent_label(state, id)).unwrap_or_else(|| "CLI".to_owned())
            )),
    );
    out.push(section_label("How many").into_any_element());
    out.push(counts.into_any_element());

    // "Will launch" — one row per picked agent: count · icon · label.
    let mut rows = div().mb_8().flex().flex_col().gap(px(6.0));
    for id in &state.selected_agents {
        rows = rows.child(
            div()
                .h(px(36.0))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .w(px(20.0))
                        .flex_none()
                        .text_align(gpui::TextAlign::Right)
                        .text_size(px(13.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(
                            state
                                .agent_counts
                                .get(id)
                                .copied()
                                .unwrap_or(1)
                                .to_string(),
                        ),
                )
                .child(crate::assets::icon(
                    agent_icon(id),
                    15.0,
                    theme::hex(theme::text::DIM),
                ))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(agent_label(state, id)),
                ),
        );
    }
    out.push(section_label("Will launch").into_any_element());
    out.push(rows.into_any_element());
    out
}

/// The full-width launch button — `bg-text text-bg` in the original: the
/// paper button that opens the picked agents at their counts.
fn launch_button(
    state: &CodeState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let total: u32 = state
        .selected_agents
        .iter()
        .map(|id| state.agent_counts.get(id).copied().unwrap_or(1))
        .sum();
    let enabled = !state.selected_agents.is_empty() && total > 0;
    let label = if state.selected_agents.is_empty() {
        "Choose an agent".to_owned()
    } else {
        format!("Launch {total} session{}", if total == 1 { "" } else { "s" })
    };
    let mut button = div()
        .id("slate-code-launch")
        .h(px(48.0))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(rgb(theme::hex(theme::text::NORMAL)))
        .text_color(rgb(theme::hex(theme::monochrome::BASE)))
        .text_size(px(14.0))
        .font_weight(FontWeight::MEDIUM)
        .child(crate::assets::icon(
            "plus",
            16.0,
            theme::hex(theme::monochrome::BASE),
        ))
        .child(label);
    if enabled {
        button = button.cursor_pointer().hover(|el| el.opacity(0.9)).on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _event, _window, cx| {
                cx.stop_propagation();
                this.code_command("launch_selected", json!({}), cx);
            }),
        );
    } else {
        button = button.opacity(0.3);
    }
    button
}

/// One grid card — `SessionCard` (`data-testid="code-session"`): a 24px
/// header (`code-session-header`) over the terminal body. The original's
/// header was also the drag handle and focused the session on click; the
/// port keeps the focus and drops the drag. `rect` is the cell
/// `code_layout::arrange` computed for this card.
fn session_card(
    state: &CodeState,
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
    // herdr's states over the plain live/dead flag — green while working,
    // amber when the agent is blocked on a human, dim at an idle prompt.
    let dot = if !alive {
        theme::text::FAINT
    } else {
        match state.agent_states.get(&session.terminal_id) {
            Some(slate_app::agent_state::AgentState::Working) => theme::status::OK,
            Some(slate_app::agent_state::AgentState::Blocked) => theme::status::WARN,
            Some(slate_app::agent_state::AgentState::Idle) => theme::text::DIM,
            _ => theme::text::FAINT,
        }
    };

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
                // A pinned thread wears the sand accent — the same mark
                // t3code's pinned rows get in the sidebar.
                .text_color(rgb(theme::hex(if session.pinned {
                    theme::accent::SAND
                } else {
                    theme::text::DIM
                })))
                .child(agent_badge(icon, 12.0))
                .when(session.worktree.is_some(), |el| {
                    el.child(crate::assets::icon(
                        "git-branch",
                        11.0,
                        theme::hex(theme::text::FAINT),
                    ))
                })
                .child(div().min_w_0().truncate().child(session.label.clone())),
        )
        .child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                // `h-1.5 w-1.5 rounded-pill` — the detected agent state
                // (herdr), the finished pill's faint dot while dead.
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(rgb(theme::hex(dot))),
                )
                // `pin` / `settle` — t3code's thread verbs, small
                // square affordances like the close button.
                .child(
                    div()
                        .id(format!("slate-code-pin-{id}"))
                        .flex_none()
                        .w(px(CARD_BTN))
                        .h(px(CARD_BTN))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                        .child(crate::assets::icon(
                            "pin",
                            11.0,
                            theme::hex(if session.pinned {
                                theme::accent::SAND
                            } else {
                                theme::text::FAINT
                            }),
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let id = id.clone();
                                move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.code_command(
                                        "pin_session",
                                        json!({ "id": id }),
                                        cx,
                                    );
                                }
                            }),
                        ),
                )
                .child(
                    div()
                        .id(format!("slate-code-settle-{id}"))
                        .flex_none()
                        .w(px(CARD_BTN))
                        .h(px(CARD_BTN))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                        .child(crate::assets::icon(
                            "check",
                            11.0,
                            theme::hex(theme::text::FAINT),
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener({
                                let id = id.clone();
                                move |this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.code_command(
                                        "settle_session",
                                        json!({ "id": id }),
                                        cx,
                                    );
                                }
                            }),
                        ),
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

/// One conversation row in the resume list — the checkbox, agent mark,
/// title and age on top, the `claude --resume …` command chip with its copy
/// and Resume buttons under it.
fn resume_row(
    state: &CodeState,
    conversation: &AgentConversation,
    picked: bool,
    now_ms: u64,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let key = code_resume::conversation_key(conversation.agent_id, &conversation.id);
    let key_pick = key.clone();
    let key_dismiss = key.clone();
    let name = if conversation.title.is_empty() {
        format!("{} session", agent_label(state, conversation.agent_id))
    } else {
        conversation.title.clone()
    };
    let command = conversation.command.clone();
    let command_copy = command.clone();
    let command_resume = command.clone();
    let agent_id = conversation.agent_id;
    let title = name.clone();
    let icon_name = agent_icon(conversation.agent_id);

    let mut check = div()
        .w(px(15.0))
        .h(px(15.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .border_1();
    if picked {
        check = check
            .border_color(rgb(theme::hex(theme::text::NORMAL)))
            .bg(rgb(theme::hex(theme::text::NORMAL)))
            .child(crate::assets::icon(
                "check",
                10.0,
                theme::hex(theme::monochrome::BASE),
            ));
    } else {
        check = check.border_color(rgb(theme::hex(theme::hairline::FAINT)));
    }

    div()
        .border_1()
        .border_color(rgb(if picked {
            theme::hex(theme::accent::SAND)
        } else {
            theme::hex(theme::hairline::FAINT)
        }))
        .bg(rgb(if picked {
            theme::hex(theme::monochrome::RAISED)
        } else {
            theme::hex(theme::monochrome::ELEVATED)
        }))
        .px_3()
        .py(px(10.0))
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .id(format!("slate-resume-pick-{key}"))
                        .w(px(20.0))
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .cursor_pointer()
                        .child(check)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "resume_toggle",
                                    json!({ "key": key_pick }),
                                    cx,
                                );
                            }),
                        ),
                )
                .child(crate::assets::icon(
                    icon_name,
                    14.0,
                    theme::hex(theme::text::DIM),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.0))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(name),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(code_resume::relative_time(
                            conversation.updated_at,
                            now_ms,
                        )),
                )
                .child(
                    div()
                        .id(format!("slate-resume-dismiss-{key}"))
                        .w(px(36.0))
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .cursor_pointer()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .hover(|el| {
                            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        })
                        .child(crate::assets::icon(
                            "trash-2",
                            13.0,
                            theme::hex(theme::text::FAINT),
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "resume_dismiss",
                                    json!({ "key": key_dismiss }),
                                    cx,
                                );
                            }),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .bg(rgb(theme::hex(theme::monochrome::BASE)))
                        .px(px(10.0))
                        .py(px(8.0))
                        .font_family(slate_app::terminal_screen::FONT_FAMILY)
                        .text_size(px(11.0))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(command.clone()),
                )
                .child(
                    div()
                        .id(format!("slate-resume-copy-{key}"))
                        .w(px(36.0))
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .rounded_full()
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                        .child(crate::assets::icon(
                            "copy",
                            13.0,
                            theme::hex(theme::text::FAINT),
                        ))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "resume_copy",
                                    json!({ "command": command_copy }),
                                    cx,
                                );
                            }),
                        ),
                )
                .child(
                    div()
                        .id(format!("slate-resume-open-{key}"))
                        .h(px(36.0))
                        .flex_none()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .px_3()
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                        .child(crate::assets::icon(
                            "play",
                            11.0,
                            theme::hex(theme::text::NORMAL),
                        ))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                .child("Resume"),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "resume",
                                    json!({
                                        "agent": agent_id,
                                        "command": command_resume,
                                        "title": title,
                                    }),
                                    cx,
                                );
                            }),
                        ),
                ),
        )
}

/// `ResumeAgents` — the conversations the folder's CLIs kept, collapsed to
/// `COLLAPSED_ROWS` until "Show all". None while a scan is out or the
/// folder has no history; a slim line when every row is dismissed.
fn resume_section(
    state: &CodeState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Option<AnyElement> {
    if state.conversations.is_empty() {
        return None;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let visible: Vec<&AgentConversation> = state
        .conversations
        .iter()
        .filter(|c| {
            !state
                .resume_dismissed
                .contains(&code_resume::conversation_key(c.agent_id, &c.id))
        })
        .collect();
    let hidden = state.conversations.len() - visible.len();

    // Every row dismissed — the original folds to a quiet line with a way
    // back.
    if visible.is_empty() {
        return Some(
            div()
                .mb_8()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(crate::assets::icon(
                    "history",
                    13.0,
                    theme::hex(theme::text::FAINT),
                ))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!(
                            "{hidden} dismissed session{}",
                            if hidden == 1 { "" } else { "s" }
                        )),
                )
                .child(
                    div()
                        .id("slate-resume-restore")
                        .px_3()
                        .py_1()
                        .text_size(px(12.0))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .cursor_pointer()
                        .hover(|el| {
                            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        })
                        .child("Show them")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command("resume_restore_all", json!({}), cx);
                            }),
                        ),
                )
                .into_any_element(),
        );
    }

    let selected_count = visible
        .iter()
        .filter(|c| {
            state
                .resume_selected
                .contains(&code_resume::conversation_key(c.agent_id, &c.id))
        })
        .count();
    let all_selected = selected_count > 0 && selected_count == visible.len();
    let rows = if state.resume_expanded {
        visible.clone()
    } else {
        visible[..visible.len().min(code_resume::COLLAPSED_ROWS)].to_vec()
    };
    let remaining = 32usize.saturating_sub(state.sessions.len());
    let resumable = code_resume::conversations_to_resume(
        &visible.iter().map(|c| (*c).clone()).collect::<Vec<_>>(),
        &state.resume_selected,
        remaining,
    )
    .len();

    let mut list = div().p_3().flex().flex_col().gap_2();
    for conversation in &rows {
        let key = code_resume::conversation_key(conversation.agent_id, &conversation.id);
        list = list.child(resume_row(
            state,
            conversation,
            state.resume_selected.contains(&key),
            now_ms,
            cx,
        ));
    }

    // The footer — counts on the left, then Select all / Show all /
    // Dismiss all and the bulk Resume button.
    let mut footer_text = format!(
        "{} session{} in this folder",
        visible.len(),
        if visible.len() == 1 { "" } else { "s" }
    );
    if hidden > 0 {
        footer_text.push_str(&format!(" · {hidden} dismissed"));
    }
    if selected_count > 0 {
        footer_text.push_str(&format!(" · {selected_count} selected"));
    }
    let mut footer = div()
        .border_t_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .px_4()
        .py_3()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .flex_wrap()
        .child(
            div()
                .mr_auto()
                .text_size(px(11.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(footer_text),
        );
    let footer_button = |id: &'static str,
                         label: String,
                         cx: &mut Context<crate::canvas_view::CanvasView>| {
        div()
            .id(id)
            .px_3()
            .py(px(6.0))
            .text_size(px(12.0))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .child(label)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.code_command(
                        match id {
                            "slate-resume-selall" => "resume_select_all",
                            "slate-resume-showall" => "resume_expand",
                            "slate-resume-dismissall" => "resume_dismiss_all",
                            _ => "resume_all",
                        },
                        json!({}),
                        cx,
                    );
                }),
            )
    };
    if visible.len() > 1 {
        footer = footer.child(footer_button(
            "slate-resume-selall",
            if all_selected {
                "Clear selection".to_owned()
            } else {
                format!("Select all ({})", visible.len())
            },
            cx,
        ));
    }
    if visible.len() > code_resume::COLLAPSED_ROWS {
        footer = footer.child(footer_button(
            "slate-resume-showall",
            if state.resume_expanded {
                "Show less".to_owned()
            } else {
                format!("Show all ({})", visible.len())
            },
            cx,
        ));
    }
    footer = footer.child(
        div()
            .id("slate-resume-dismissall")
            .px_3()
            .py(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .text_size(px(12.0))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .child(crate::assets::icon("x", 12.0, theme::hex(theme::text::FAINT)))
            .child("Dismiss all")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.code_command("resume_dismiss_all", json!({}), cx);
                }),
            ),
    );
    let mut resume_all = div()
        .id("slate-resume-all")
        .px_3()
        .py(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(rgb(theme::hex(theme::text::NORMAL)))
        .child(crate::assets::icon("play", 12.0, theme::hex(theme::text::NORMAL)))
        .child(if selected_count > 0 {
            format!("Resume selected ({resumable})")
        } else {
            format!("Resume all ({resumable})")
        });
    if resumable > 0 {
        resume_all = resume_all
            .cursor_pointer()
            .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.code_command("resume_all", json!({}), cx);
                }),
            );
    } else {
        resume_all = resume_all.opacity(0.4);
    }
    footer = footer.child(resume_all);

    Some(
        div()
            .mb_8()
            .w_full()
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
            .flex()
            .flex_col()
            .child(
                div()
                    .border_b_1()
                    .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                    .px_4()
                    .py(px(14.0))
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_3()
                    .child(
                        div().mt(px(2.0)).flex_none().child(crate::assets::icon(
                            "history",
                            15.0,
                            theme::hex(theme::text::FAINT),
                        )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(13.0))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                            .child("Resume your agents"),
                                    )
                                    .child(
                                        div()
                                            .border_1()
                                            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                                            .rounded_full()
                                            .px(px(6.0))
                                            .text_size(px(10.0))
                                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                                            .child(visible.len().to_string()),
                                    ),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_size(px(12.0))
                                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                                    .child(
                                        "Conversations these CLIs kept in this folder. Resume one to pick it up where it stopped, or dismiss it to start fresh.",
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .id("slate-resume-refresh")
                            .w(px(36.0))
                            .h(px(36.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .cursor_pointer()
                            .hover(|el| {
                                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                            })
                            .child(crate::assets::icon(
                                "refresh-cw",
                                13.0,
                                theme::hex(theme::text::FAINT),
                            ))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    cx.stop_propagation();
                                    this.code_command("resume_refresh", json!({}), cx);
                                }),
                            ),
                    ),
            )
            .child(list)
            .child(footer)
            .into_any_element(),
    )
}

/// The empty grid — `sessions.length === 0` state: the original's centred
/// `max-w-[640px]` column — folder line, `ResumeAgents`, the multi-select
/// agent grid, "Other CLI or browser", then How many / Will launch /
/// Launch when agents are ticked.
fn empty_state(
    state: &CodeState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut column = div().w(px(640.0)).flex().flex_col();

    // The folder line — quiet once settled: icon + basename + Change.
    column = column.child(
        div()
            .mb_8()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(crate::assets::icon(
                "folder-open",
                15.0,
                theme::hex(theme::text::FAINT),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.0))
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        state
                            .workspace_name
                            .clone()
                            .unwrap_or_else(|| "this folder".to_owned()),
                    ),
            )
            .child(
                div()
                    .id("slate-code-change-dir")
                    .ml_auto()
                    .flex_none()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(12.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .cursor_pointer()
                    .hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .child("Change")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.code_command("pick_folder", json!({}), cx);
                        }),
                    ),
            ),
    );

    if let Some(section) = resume_section(state, cx) {
        column = column.child(section);
    }

    // The multi-select agent grid — `grid grid-cols-3 gap-2`.
    column = column.child(section_label("Agent"));
    let mut grid = div()
        .mb_8()
        .w_full()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_2();
    for agent in inline_agents() {
        grid = grid.child(agent_chip(
            agent.id,
            agent.label,
            agent.icon,
            state.selected_agents.iter().any(|id| id == agent.id),
            true,
            cx,
        ));
    }
    for custom in &state.custom_agents {
        grid = grid.child(agent_chip(
            &custom.id,
            &custom.label,
            "terminal",
            state.selected_agents.iter().any(|id| id == &custom.id),
            true,
            cx,
        ));
    }
    column = column.child(grid);

    // `code-open-launcher` — the fixed choices end at the built-in; every
    // other CLI is added through the command bar's `/agent` verb.
    column = column.child(
        div()
            .id("slate-code-other-cli")
            .mb_8()
            .h(px(40.0))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
            .text_size(px(12.0))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .child(crate::assets::icon(
                "terminal",
                14.0,
                theme::hex(theme::text::DIM),
            ))
            .child("Add an agent — /agent <name> <command>")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.code_command("add_agent", json!({}), cx);
                }),
            ),
    );

    if !state.selected_agents.is_empty() {
        for el in selection_controls(state, cx) {
            column = column.child(el);
        }
    }
    column = column.child(launch_button(state, cx));

    // Scroll container → full-width min-height wrapper → centered column.
    // Centering the scroll child directly (`items_center` on the scroll
    // div) leaves the column full-width and top-clipped when it grows
    // past the viewport; the wrapper gives the column real slack to
    // center in when content fits and honest top-anchored scroll when it
    // does not.
    let min_h = (f32::from(state.viewport.height) - HEADER_H).max(0.0);
    div()
        .id("slate-code-empty")
        .size_full()
        .overflow_y_scroll()
        .child(
            div()
                .w_full()
                .min_h(px(min_h))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p_6()
                .child(column),
        )
}

/// t3code's settled shelf — every `settled` session as a one-line row:
/// its title, a restore affordance, the close `✕`. Sits where the empty
/// state would be when every thread is settled.
fn settled_or_empty(
    state: &CodeState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> AnyElement {
    let settled: Vec<&CodeSession> = state.sessions.iter().filter(|s| s.settled).collect();
    if settled.is_empty() {
        return empty_state(state, cx).into_any_element();
    }
    let mut column = div()
        .id("code-settled")
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .overflow_scroll()
        .p_6();
    column = column.child(section_label("Settled"));
    for session in settled {
        let restore_id = session.id.clone();
        let close_id = session.id.clone();
        column = column.child(
            div()
                .h(px(36.0))
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .border_b_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.0))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .child(session.label.clone()),
                )
                .child(
                    div()
                        .id(format!("slate-code-restore-{restore_id}"))
                        .flex_none()
                        .h(px(24.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .px_2()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .cursor_pointer()
                        .hover(|el| {
                            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        })
                        .child(crate::assets::icon(
                            "refresh-cw",
                            10.0,
                            theme::hex(theme::text::FAINT),
                        ))
                        .child("Restore")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "restore_session",
                                    json!({ "id": restore_id }),
                                    cx,
                                );
                            }),
                        ),
                )
                .child(
                    div()
                        .id(format!("slate-code-settled-close-{close_id}"))
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
    }
    column.into_any_element()
}

/// The title strip's fleet rollup — herdr's "never hunt for the stuck
/// one": nonzero state counts across the live cards, amber when anything
/// is blocked on a human.
fn fleet_rollup(state: &CodeState) -> Div {
    let (mut working, mut blocked, mut idle) = (0u32, 0u32, 0u32);
    for session in &state.sessions {
        match state.agent_states.get(&session.terminal_id) {
            Some(slate_app::agent_state::AgentState::Working) => working += 1,
            Some(slate_app::agent_state::AgentState::Blocked) => blocked += 1,
            Some(slate_app::agent_state::AgentState::Idle) => idle += 1,
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if working > 0 {
        parts.push(format!("{working} working"));
    }
    if blocked > 0 {
        parts.push(format!("{blocked} blocked"));
    }
    if idle > 0 {
        parts.push(format!("{idle} idle"));
    }
    div()
        .font_family("Departure Mono")
        .text_size(px(10.0))
        .text_color(rgb(theme::hex(if blocked > 0 {
            theme::status::WARN
        } else {
            theme::text::FAINT
        })))
        .child(if parts.is_empty() {
            String::new()
        } else {
            format!("· {}", parts.join(" · "))
        })
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
        .child(fleet_rollup(state))
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

    // ── Launcher panel — `launcherOpen`, the dialog flattened inline ─────
    // Line one picks agents (the same selection the empty state edits);
    // line two is the count row and the Launch button — the original
    // `CodeLauncher` dialog's fan-out, so a board that's already up can
    // still open a bunch at once.
    let mut root = div()
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::hex(theme::monochrome::BASE)))
        .child(header);
    if state.launcher_open {
        let mut chips = div()
            .flex_none()
            .h(px(44.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
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
            chips = chips.child(agent_chip(
                agent.id,
                agent.label,
                agent.icon,
                state.selected_agents.iter().any(|id| id == agent.id),
                false,
                cx,
            ));
        }
        for custom in &state.custom_agents {
            chips = chips.child(agent_chip(
                &custom.id,
                &custom.label,
                "terminal",
                state.selected_agents.iter().any(|id| id == &custom.id),
                false,
                cx,
            ));
        }
        // "+ Other" — opens the command bar on the `/agent` verb; the add
        // flow lives there because it is the app's text input.
        chips = chips.child(
            div()
                .id("slate-code-agent-other")
                .flex_none()
                .h(px(38.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.0))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                .px_3()
                .cursor_pointer()
                .text_size(px(13.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                })
                .child("+ Other")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.code_command("add_agent", json!({}), cx);
                    }),
                ),
        );
        let panel = div()
            .flex_none()
            .h(px(LAUNCHER_H))
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
            .child(chips);

        // Second line: counts for the active agent + the Launch button.
        let mut controls = div()
            .flex_none()
            .h(px(48.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3();
        if state.selected_agents.is_empty() {
            controls = controls.child(
                div()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("Pick agents, then a count — Launch opens them all"),
            );
        } else {
            let active = state.active_agent.as_deref();
            let current = active
                .and_then(|id| state.agent_counts.get(id).copied())
                .unwrap_or(1);
            for count in LAUNCH_COUNTS {
                controls = controls.child(
                    div()
                        .id(format!("slate-code-launch-count-{count}"))
                        .w(px(32.0))
                        .h(px(32.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_1()
                        .border_color(rgb(if count == current {
                            theme::hex(theme::accent::SAND)
                        } else {
                            theme::hex(theme::hairline::FAINT)
                        }))
                        .bg(rgb(if count == current {
                            theme::hex(theme::monochrome::RAISED)
                        } else {
                            theme::hex(theme::monochrome::SURFACE)
                        }))
                        .text_size(px(12.0))
                        .text_color(rgb(if count == current {
                            theme::hex(theme::text::NORMAL)
                        } else {
                            theme::hex(theme::text::DIM)
                        }))
                        .cursor_pointer()
                        .child(count.to_string())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.code_command(
                                    "set_count",
                                    json!({ "count": count }),
                                    cx,
                                );
                            }),
                        ),
                );
            }
            controls = controls.child(
                div()
                    .ml_1()
                    .text_size(px(12.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(format!("for {}", active.map(|id| agent_label(state, id)).unwrap_or_else(|| "CLI".to_owned()))),
            );
            // t3code's "New worktree" — each launched session gets its own
            // `git worktree` + `slate/<id>` branch under the workspace.
            controls = controls.child(
                div()
                    .id("slate-code-worktree")
                    .ml_2()
                    .h(px(32.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px_3()
                    .border_1()
                    .border_color(rgb(if state.worktree {
                        theme::hex(theme::accent::SAND)
                    } else {
                        theme::hex(theme::hairline::FAINT)
                    }))
                    .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                    .text_size(px(12.0))
                    .text_color(rgb(if state.worktree {
                        theme::hex(theme::text::NORMAL)
                    } else {
                        theme::hex(theme::text::DIM)
                    }))
                    .cursor_pointer()
                    .child(crate::assets::icon(
                        "git-branch",
                        12.0,
                        theme::hex(if state.worktree {
                            theme::accent::SAND
                        } else {
                            theme::text::FAINT
                        }),
                    ))
                    .child("worktree")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.code_command("toggle_worktree", json!({}), cx);
                        }),
                    ),
            );
            let total: u32 = state
                .selected_agents
                .iter()
                .map(|id| state.agent_counts.get(id).copied().unwrap_or(1))
                .sum();
            controls = controls.child(
                div()
                    .id("slate-code-launch-row")
                    .ml_auto()
                    .h(px(32.0))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px_3()
                    .bg(rgb(theme::hex(theme::text::NORMAL)))
                    .text_color(rgb(theme::hex(theme::monochrome::BASE)))
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|el| el.opacity(0.9))
                    .child(crate::assets::icon(
                        "play",
                        11.0,
                        theme::hex(theme::monochrome::BASE),
                    ))
                    .child(format!(
                        "Launch {total} session{}",
                        if total == 1 { "" } else { "s" }
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.code_command("launch_selected", json!({}), cx);
                        }),
                    ),
            );
        }
        root = root.child(panel.child(controls));
    }

    // ── The grid ────────────────────────────────────────────────────────
    // `relative grid … overflow-auto` — `code_layout::arrange` answers
    // each card's rect, the port of `autoCodeLayout`/`codeGridLayout`.
    // Settled threads leave the grid for the shelf below — `count` and
    // the layout rects both track the live set.
    let live: Vec<&CodeSession> = state.sessions.iter().filter(|s| !s.settled).collect();
    if live.is_empty() {
        root.child(settled_or_empty(state, cx))
    } else {
        let width = f32::from(state.viewport.width);
        let chrome = HEADER_H + if state.launcher_open { LAUNCHER_H } else { 0.0 };
        let height = (f32::from(state.viewport.height)
            - slate_app::canvas::TITLE_BAR_HEIGHT
            - chrome)
        .max(0.0);
        let focus_index = live
            .iter()
            .position(|s| Some(&s.id) == state.focused.as_ref())
            .unwrap_or(0);
        let area = slate_app::canvas::Rect::from_min_size(
            slate_app::canvas::Pos2::ZERO,
            slate_app::canvas::Vec2::new(width, height),
        );
        let rects = code_layout::arrange(state.layout, area, live.len(), focus_index);
        let mut grid = div()
            .id("code-grid")
            .flex_1()
            .min_h_0()
            .relative()
            // The shell's `overflow-auto`: an arranged grid larger than the
            // pane scrolls instead of clipping cards unreachable.
            .overflow_scroll()
            .bg(rgb(theme::hex(theme::monochrome::RAISED)));
        for (session, rect) in live.iter().zip(rects) {
            grid = grid.child(session_card(
                state,
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
