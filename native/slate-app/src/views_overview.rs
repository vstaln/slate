//! Overview — the native port of the Electron app's third Workspace View
//! (`activeView === 'overview'`).
//!
//! In the original (`src/renderer/src/App.tsx` + `components/Sidebar.tsx`,
//! 1593 lines) picking the Overview tab hid the canvas `<main>` and mounted
//! the Sidebar's *expanded* render — `expanded = activeView !== 'canvas'` —
//! a 200px panel holding the workspace-folder model: the `folders` rail
//! flyout's recent-directories list (`entry.name` over `entry.path`, pin
//! and remove buttons on the row, `Open Folder…` at the foot) plus the
//! code-workspace groups (`folder` header + indented `Code2` rows with
//! hover rename/delete).
//!
//! This port keeps the folders half of that model as the Overview's left
//! column and adds the thing the shell's overview never drew for itself —
//! a canvas minimap — in the main area: world bounds of every widget,
//! padded ~10%, scaled into the panel preserving aspect, each widget a
//! square rect (terminals brighter, other kinds dimmer) with a tiny title
//! on terminals that have room. A widget roster under the map lists the
//! same set with kind icon and world position, and the header row carries
//! the `N widgets · M terminals · K open tasks` summary.
//!
//! Deviations and why, flagged for the coordinator:
//!
//! - **No code-workspace groups yet.** The Rust side has no counterpart of
//!   `window.api.workspace.codeWorkspaceGroups()`; the recents list is the
//!   `workspace::WorkspaceState::recent` model (`sorted()` order — pinned
//!   first), and groups land when a command exists to feed them.
//! - **Rename emits a command, not a dialog.** The original opened a modal
//!   (`rename-workspace-dialog`) that POSTed `workspace.rename`; gpui has
//!   the verb (`workspace::WorkspaceState::rename_recent`) but no prompt
//!   surface, so the pencil emits `rename_recent` with the path and the
//!   coordinator decides how to gather the name.
//! - **Pin rides the vendored `pin` glyph** — sand while pinned, a faint
//!   outline that surfaces on row hover when not.
//!
//! Emitted through `CanvasView::overview_command(action, json!({}), cx)`:
//!
//! - `"open_recent"`    `{"path": "…"}` — a recent row press; also the
//!   verb a code-workspace row would send after the folder hop.
//! - `"pin_recent"`     `{"path": "…"}` — the unpinned dot.
//! - `"unpin_recent"`   `{"path": "…"}` — the pinned dot.
//! - `"forget_recent"`  `{"path": "…"}` — the hover `×`.
//! - `"rename_recent"`  `{"path": "…"}` — the hover pencil; the handler
//!   gathers the new name (the original's modal) and calls
//!   `workspace.rename` — or ignores the verb until then.
//! - `"pick_folder"`    `{}` — the `Open Folder…` footer button, the
//!   original's `onPickDir` (`workspace.pickDir`).
//! - `"focus_widget"`   `{"id": "…"}` — a minimap rect or roster row
//!   press: raise + reveal that widget and return to the canvas view.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::json;
use slate_app::theme;

/// One `RecentDir` from `workspace.json` (`workspace::RecentDir`), folded
/// to the three fields the row renders. `name` is the display label the
/// user can rename; `path` is the identity every command sends back.
pub struct OverviewRecent {
    pub path: String,
    pub name: String,
    pub pinned: bool,
}

/// One canvas widget, folded out of `projection::Widget` (f64 world
/// geometry there, f32 here — the minimap only needs paint precision).
/// `kind` is the widget kind string (`"terminal"`, `"files"`, …);
/// `x`/`y`/`w`/`h` are the world-space rect.
pub struct OverviewWidget {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Everything the Overview renders, filled by `CanvasView` each frame —
/// the same plain-data snapshot contract as `ToolbarState`/`SidebarState`.
pub struct OverviewState {
    /// `workspaceDir` — the adopted workspace folder, or `None` before
    /// `workspace::ensure()` lands one. `None` shows a "No folder" card.
    pub workspace_dir: Option<String>,
    /// The workspace's display name (`rename_recent` targets it). The
    /// caller may leave it empty — the card falls back to the dir's
    /// basename, then the raw path, matching the sidebar's read.
    pub workspace_name: String,
    /// `WorkspaceState::sorted()` — pinned first, then most recent.
    pub recents: Vec<OverviewRecent>,
    /// Every live widget, any order — the roster lists them as given.
    pub widgets: Vec<OverviewWidget>,
    /// Unchecked planner items — the `K open tasks` summary digit.
    pub planner_open: usize,
    /// Checked planner items — kept for the summary line's future
    /// "K/K+M tasks" form; v1 renders only `planner_open`.
    pub planner_done: usize,
    /// The overview area's size — the canvas `<main>`'s bounds the pane
    /// replaces. Drives the minimap's pixel box; zero-sized falls back to
    /// a 960×600 assumption so the view still lays out.
    pub viewport: Size<Pixels>,
}

/// The left column — wider than the original's 200px `sidebarExpanded`
/// because the Overview panel has a whole canvas worth of room.
const LEFT_W: f32 = 272.0;
/// `h-11` — the caption header height the expanded sidebar used.
const HEADER_H: f32 = 44.0;
/// Outer padding around the main column's content.
const PAGE_PAD: f32 = 16.0;
/// The roster section's share of the viewport height, clamped.
const ROSTER_FRAC: f32 = 0.30;
const ROSTER_MIN: f32 = 120.0;
const ROSTER_MAX: f32 = 260.0;
/// Smallest painted minimap rect — a widget must stay a click target even
/// when the world is huge.
const MINI_RECT: f32 = 6.0;
/// Degenerate worlds (one widget, a straight line) still get this much
/// span so the scale stays sane.
const MIN_WORLD_SPAN: f32 = 480.0;
/// ~10% breathing room around the world bounds — the port's pad.
const BOUNDS_PAD: f32 = 0.10;
/// Viewport fallback when the caller hasn't measured yet.
const FALLBACK_W: f32 = 960.0;
const FALLBACK_H: f32 = 600.0;

/// The section caption — the original's
/// `text-[11px] font-semibold tracking-[0.08em] uppercase text-text-faint`
/// (gpui has no letter-spacing; the uppercase text carries it). Same
/// recipe as `views_sidebar::caption`.
fn caption(label: &str, count: Option<usize>) -> Div {
    let mut row = div()
        .flex()
        .flex_row()
        .items_baseline()
        .justify_between()
        .px_1()
        .child(
            div()
                .font_family("Departure Mono")
                .text_size(px(10.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(label.to_uppercase()),
        );
    if let Some(count) = count {
        row = row.child(
            div()
                .text_size(px(9.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(count.to_string()),
        );
    }
    row
}

/// A vendored Lucide stand-in per widget kind — the shell had real
/// per-kind glyphs, the vendored set is small, so kinds share approximations.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "terminal" => "square",
        "files" => "folder-open",
        "sys-monitor" => "cpu",
        "timer" => "minus",
        "planner" => "check",
        "orchestration" => "git-branch",
        "browser" => "expand",
        "image" => "copy",
        "links" => "arrow-right",
        "notes" => "pencil",
        "calendar" => "ellipsis",
        "kanban" => "layout-grid",
        _ => "square",
    }
}

/// The pin affordance — the vendored `pin` glyph. Pinned wears the sand
/// accent and is always visible; unpinned is a faint mark that only
/// surfaces on row hover. Click sends `pin_recent`/`unpin_recent` — the
/// original's `pinRecent` toggle.
fn pin_button(
    entry_path: &str,
    pinned: bool,
    group: SharedString,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let path = entry_path.to_owned();
    let mut dot = div()
        .id(format!("overview-pin-{entry_path}"))
        .flex_none()
        .w(px(20.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
        .child(crate::assets::icon(
            "pin",
            11.0,
            theme::hex(if pinned {
                theme::accent::SAND
            } else {
                theme::text::FAINT
            }),
        ))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.overview_command(
                    if pinned { "unpin_recent" } else { "pin_recent" },
                    json!({ "path": path }),
                    cx,
                );
            }),
        );
    if !pinned {
        dot = dot.invisible().group_hover(group, |el| el.visible());
    }
    dot
}

/// One recent-folder row — the flyout's `group flex items-center` entry:
/// `name` bold over `path` faint, current workspace on `bg-bg-hover`,
/// pin disc, hover `pencil` (rename) and `×` (forget). Press on the text
/// emits `open_recent` (the original's `open(entry.path)`).
fn recent_row(
    index: usize,
    entry: &OverviewRecent,
    is_current: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let group: SharedString = format!("overview-recent-{index}").into();
    let open_path = entry.path.clone();
    let rename_path = entry.path.clone();
    let forget_path = entry.path.clone();
    let row = div()
        .id(format!("overview-recent-{index}"))
        .group(group.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .w_full()
        .px_1()
        .py_1()
        .cursor_pointer()
        .when(is_current, |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
        })
        .when(!is_current, |el| {
            el.hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        })
        .child(
            // The text block owns the open press — its flex-1 leaves the
            // action buttons to their own handlers.
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .px_1()
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(entry.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(entry.path.clone()),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.overview_command("open_recent", json!({ "path": open_path }), cx);
                    }),
                ),
        )
        .child(pin_button(&entry.path, entry.pinned, group.clone(), cx))
        // Rename — the original's `Pencil` hover button opening
        // `rename-workspace-dialog`. The verb exists in the port's
        // workspace model; where the name comes from is the handler's job.
        .child(
            div()
                .id(format!("overview-rename-{index}"))
                .flex_none()
                .w(px(20.0))
                .h(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .invisible()
                .group_hover(group.clone(), |el| el.visible())
                .cursor_pointer()
                .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                .child(crate::assets::icon(
                    "pencil-thin",
                    11.0,
                    theme::hex(theme::text::FAINT),
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.overview_command(
                            "rename_recent",
                            json!({ "path": rename_path }),
                            cx,
                        );
                    }),
                ),
        )
        // Forget — the flyout's `X` button, `title="Remove from recent"`.
        .child(
            div()
                .id(format!("overview-forget-{index}"))
                .flex_none()
                .w(px(20.0))
                .h(px(20.0))
                .flex()
                .items_center()
                .justify_center()
                .invisible()
                .group_hover(group, |el| el.visible())
                .cursor_pointer()
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                })
                .child(crate::assets::icon(
                    "x",
                    12.0,
                    theme::hex(theme::text::FAINT),
                ))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.overview_command(
                            "forget_recent",
                            json!({ "path": forget_path }),
                            cx,
                        );
                    }),
                ),
        );
    row
}

/// The WORKSPACE card — name over dir, `folder-open` glyph, the flyout's
/// `Folders · current:` tooltip flattened into a panel. Read-only; folder
/// changes go through the `Open Folder…` button below the recents.
fn workspace_card(state: &OverviewState) -> impl IntoElement {
    let (name, dir) = match &state.workspace_dir {
        Some(dir) => {
            let name = if state.workspace_name.is_empty() {
                std::path::Path::new(dir)
                    .file_name()
                    .map(|part| part.to_string_lossy().into_owned())
                    .filter(|part| !part.is_empty())
                    .unwrap_or_else(|| dir.clone())
            } else {
                state.workspace_name.clone()
            };
            (name, dir.clone())
        }
        None => ("No folder".to_owned(), "Pick a workspace folder".to_owned()),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        .px_2()
        .py_2()
        .child(
            div()
                .flex_none()
                .child(crate::assets::icon(
                    "folder-open",
                    14.0,
                    theme::hex(theme::text::DIM),
                )),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(name),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(dir),
                ),
        )
}

/// World bounds of `widgets`, padded [`BOUNDS_PAD`] on every side and
/// floored at [`MIN_WORLD_SPAN`] so a lone widget still maps cleanly.
/// Returns `(origin_x, origin_y, width, height)` in world units.
fn world_bounds(widgets: &[OverviewWidget]) -> Option<(f32, f32, f32, f32)> {
    if widgets.is_empty() {
        return None;
    }
    let mut min_x = f32::MAX;
    let mut min_y = f32::MAX;
    let mut max_x = f32::MIN;
    let mut max_y = f32::MIN;
    for w in widgets {
        min_x = min_x.min(w.x);
        min_y = min_y.min(w.y);
        max_x = max_x.max(w.x + w.w.max(1.0));
        max_y = max_y.max(w.y + w.h.max(1.0));
    }
    let span_w = (max_x - min_x).max(MIN_WORLD_SPAN);
    let span_h = (max_y - min_y).max(MIN_WORLD_SPAN);
    // Keep the span's centre on the real bounds' centre when the floor
    // kicks in, so a single terminal sits mid-map rather than cornered.
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    Some((
        cx - span_w / 2.0 - span_w * BOUNDS_PAD,
        cy - span_h / 2.0 - span_h * BOUNDS_PAD,
        span_w * (1.0 + 2.0 * BOUNDS_PAD),
        span_h * (1.0 + 2.0 * BOUNDS_PAD),
    ))
}

/// One widget on the minimap — a square rect in panel space. Terminals
/// draw brighter (raised fill, text-faint ring, a title when the rect has
/// room); other kinds sit one shade dimmer. Click → `focus_widget`.
fn minimap_rect(
    widget: &OverviewWidget,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let terminal = widget.kind == "terminal";
    let id = widget.id.clone();
    div()
        .id(format!("overview-map-{}", widget.id))
        .absolute()
        .left(px(x))
        .top(px(y))
        .w(px(w))
        .h(px(h))
        .border_1()
        .border_color(rgb(theme::hex(if terminal {
            theme::text::FAINT
        } else {
            theme::hairline::FAINT
        })))
        .bg(rgb(theme::hex(if terminal {
            theme::monochrome::RAISED
        } else {
            theme::monochrome::SURFACE
        })))
        .cursor_pointer()
        .overflow_hidden()
        .hover(|el| {
            el.border_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .when(terminal && w > 44.0 && h > 15.0, |el| {
            el.child(
                div()
                    .px(px(3.0))
                    .pt(px(1.0))
                    .truncate()
                    .text_size(px(8.0))
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .child(widget.title.clone()),
            )
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.overview_command("focus_widget", json!({ "id": id }), cx);
            }),
        )
}

/// The CANVAS MINIMAP panel — a `map_w × map_h` box the world bounds are
/// fit into preserving aspect, centred. Empty canvases get the muted
/// placeholder; the original canvas itself drew "Your canvas is clear".
fn minimap(
    state: &OverviewState,
    map_w: f32,
    map_h: f32,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut panel = div()
        .id("overview-minimap")
        .flex_none()
        .w(px(map_w))
        .h(px(map_h))
        .relative()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        // monochrome::BASE — the map is a window onto the canvas, which
        // is the base surface itself.
        .bg(rgb(theme::hex(theme::monochrome::BASE)))
        .overflow_hidden();

    let Some((ox, oy, world_w, world_h)) = world_bounds(&state.widgets) else {
        return panel.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child("canvas is empty"),
                ),
        );
    };

    let scale = (map_w / world_w).min(map_h / world_h);
    // Centre the scaled world inside the box when one axis doesn't fill.
    let off_x = (map_w - world_w * scale) / 2.0;
    let off_y = (map_h - world_h * scale) / 2.0;

    for widget in &state.widgets {
        let rx = off_x + (widget.x - ox) * scale;
        let ry = off_y + (widget.y - oy) * scale;
        let rw = (widget.w.max(1.0) * scale).max(MINI_RECT);
        let rh = (widget.h.max(1.0) * scale).max(MINI_RECT);
        panel = panel.child(minimap_rect(widget, rx, ry, rw, rh, cx));
    }
    panel
}

/// One roster row — kind glyph, title, world position in mono. The same
/// `focus_widget` press the map rect sends; the row is the accessible,
/// always-labelled half of the same list.
fn roster_row(
    index: usize,
    widget: &OverviewWidget,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let id = widget.id.clone();
    let title = if widget.title.trim().is_empty() {
        widget.kind.clone()
    } else {
        widget.title.clone()
    };
    div()
        .id(format!("overview-roster-{index}"))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .h(px(30.0))
        .px_2()
        .cursor_pointer()
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(
            div()
                .flex_none()
                .child(crate::assets::icon(
                    kind_icon(&widget.kind),
                    12.0,
                    theme::hex(theme::text::FAINT),
                )),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(title),
        )
        .child(
            div()
                .flex_none()
                .font_family("Departure Mono")
                .text_size(px(9.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(widget.kind.clone()),
        )
        .child(
            div()
                .flex_none()
                .font_family("Departure Mono")
                .text_size(px(9.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!("{:.0}, {:.0}", widget.x, widget.y)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.overview_command("focus_widget", json!({ "id": id }), cx);
            }),
        )
}

/// The full-area Overview pane — it replaces the canvas, so it takes the
/// whole `<main>` bounds. Left column: WORKSPACE card + RECENT FOLDERS +
/// `Open Folder…`. Main column: caption header with the summary line, the
/// minimap, then the widget roster.
pub fn overview_pane(
    state: &OverviewState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let vw = f32::from(state.viewport.width).max(FALLBACK_W);
    let vh = f32::from(state.viewport.height).max(FALLBACK_H);

    // ── Left column — the expanded Sidebar's folders model ──────────────
    let mut left = div()
        .flex()
        .flex_col()
        .w(px(LEFT_W))
        .flex_none()
        .h_full()
        .border_r_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
        .child(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .h(px(HEADER_H))
                .px_3()
                .border_b_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                .child(caption("Workspace", None)),
        );

    let mut col = div()
        .id("overview-left-scroll")
        .flex()
        .flex_col()
        .gap_3()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px_2()
        .py_2();

    col = col.child(workspace_card(state));

    let mut recents = div().flex().flex_col().gap_1();
    recents = recents.child(caption("Recent folders", Some(state.recents.len())));
    if state.recents.is_empty() {
        recents = recents.child(
            div()
                .px_2()
                .py_3()
                .text_size(px(11.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                // The flyout's own empty copy.
                .child("Recent folders list is empty — select your first folder"),
        );
    } else {
        for (index, entry) in state.recents.iter().enumerate() {
            let is_current = state.workspace_dir.as_deref() == Some(entry.path.as_str());
            recents = recents.child(recent_row(index, entry, is_current, cx));
        }
    }
    col = col.child(recents);
    left = left.child(col);

    // The flyout's accent `Open Folder…` — flattened to the bordered
    // footer button the expanded panel used (`New folder`).
    left = left.child(
        div()
            .flex_none()
            .px_2()
            .pb_2()
            .child(
                div()
                    .id("overview-pick-folder")
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .h(px(36.0))
                    .border_1()
                    .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                    .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .cursor_pointer()
                    .font_family("Departure Mono")
                    .text_size(px(11.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .child(crate::assets::icon(
                        "folder-open",
                        13.0,
                        theme::hex(theme::text::DIM),
                    ))
                    .child("OPEN FOLDER…")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.overview_command("pick_folder", json!({}), cx);
                        }),
                    ),
            ),
    );

    // ── Main column — header, minimap, roster ───────────────────────────
    let terminal_count = state
        .widgets
        .iter()
        .filter(|w| w.kind == "terminal")
        .count();
    let summary = format!(
        "{} widget{} · {} terminal{} · {} open task{}",
        state.widgets.len(),
        if state.widgets.len() == 1 { "" } else { "s" },
        terminal_count,
        if terminal_count == 1 { "" } else { "s" },
        state.planner_open,
        if state.planner_open == 1 { "" } else { "s" },
    );

    let header = div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(HEADER_H))
        .px(px(PAGE_PAD))
        .border_b_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .child(
            div()
                .font_family("Brygada 1918")
                .text_size(px(15.0))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child("OVERVIEW"),
        )
        .child(
            div()
                .font_family("Departure Mono")
                .text_size(px(10.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(summary),
        );

    // The minimap box: viewport minus the left column, the header, the
    // roster section and the page padding.
    let roster_h = (vh * ROSTER_FRAC).clamp(ROSTER_MIN, ROSTER_MAX);
    let map_w = (vw - LEFT_W - PAGE_PAD * 2.0).max(160.0);
    let map_h = (vh - HEADER_H - roster_h - PAGE_PAD * 2.0 - 8.0).max(160.0);

    let mut roster = div().flex().flex_col().gap_1();
    roster = roster.child(caption("Widgets", Some(state.widgets.len())));
    if state.widgets.is_empty() {
        roster = roster.child(
            div()
                .px_2()
                .py_2()
                .text_size(px(11.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child("No widgets on this canvas"),
        );
    } else {
        let mut rows = div()
            .id("overview-roster-scroll")
            .flex()
            .flex_col()
            .gap(px(1.0))
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (index, widget) in state.widgets.iter().enumerate() {
            rows = rows.child(roster_row(index, widget, cx));
        }
        roster = roster.child(rows);
    }

    let main = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .h_full()
        .child(header)
        .child(
            div()
                .flex_none()
                .px(px(PAGE_PAD))
                .pt(px(PAGE_PAD))
                .child(minimap(state, map_w, map_h, cx)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_none()
                .h(px(roster_h))
                .min_h_0()
                .px(px(PAGE_PAD))
                .pt_2()
                .child(roster),
        );

    div()
        .id("overview-pane")
        .size_full()
        .flex()
        .flex_row()
        .bg(rgb(theme::hex(theme::monochrome::BASE)))
        .child(left)
        .child(main)
        // `data-canvas-interactive`: presses on the overview never reach
        // the canvas pan/select handlers beneath it.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event, _window, cx| {
                cx.stop_propagation();
            }),
        )
}
