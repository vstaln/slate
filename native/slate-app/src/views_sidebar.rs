//! Sidebar — the native port of the Electron `Sidebar` component, the
//! app's left rail.
//!
//! The original (`src/renderer/src/components/Sidebar.tsx`, 1593 lines
//! including the settings modal it also held) was a rail that widened
//! into a panel:
//!
//! - **Collapsed** (`w-rail` = 56px, `railWidth` in tokens.ts): a `pt-10`
//!   drag pad on top, a vertically centred cluster of draggable rail
//!   `IconButton`s — 38px pills, optional accent badge — (the final build
//!   shipped only `folders`, which opened the workspace-recents flyout),
//!   an account avatar floating at `bottom-[52px]`, and a settings icon at
//!   the foot.
//! - **Expanded** (`w-[200px]` = `sidebarExpanded`, the Code view's
//!   state): an `h-11` caption header ("Code"), a scrollable list of
//!   workspace groups (folder row + indented child rows whose rename and
//!   delete buttons surfaced on hover), a pinned `+ New folder` button,
//!   and a `border-t` footer with the account and settings rows.
//!
//! The port keeps the shell — 56px rail ⇄ 200px panel, `surface` fill,
//! the right hairline, pill icon buttons, caption-led sections — and maps
//! its content onto the five verbs `CanvasView::sidebar_command` accepts:
//!
//! - `"focus"`       `{"id": <widget>}` — a widget row press raises and
//!   reveals that widget.
//! - `"close"`       `{"id": <widget>}` — the row's hover `×`.
//! - `"spawn"`       `{"kind": <kind>}` — the Add chips, the pinned
//!   "+ New terminal" button, and the workspace row (opens Files).
//! - `"plan_toggle"` `{"id": <item>, "done": true}` — the preview lists
//!   only unchecked items, so a toggle is always a completion.
//! - `"toggle"`      `{}` — the `»`/`«` buttons flipping rail ⇄ panel
//!   (`native-ui.json` `sidebar_open`).
//!
//! - `"settings"`    `{}` — the account footer's settings affordance.
//!
//! What did not port: the folders flyout — the current workspace is a
//! read-only row standing in for the old `Folders · current:` tooltip,
//! and recents open through the Overview's workspace picker instead.

use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

/// How many unchecked planner rows the preview shows before collapsing
/// the rest into a "+N more" line — 200px of rail is not a planner.
const PREVIEW_MAX: usize = 6;

/// The spawn catalog the Add chips walk — the renderable subset of
/// `canvas_view::WIDGET_CATALOG` (the renderer's `ls` list), in catalog
/// order. `music-player` and `chat` stay out: `renderable()` drops them
/// the same way the canvas's own right-click menu does.
const SPAWN_KINDS: [(&str, &str); 12] = [
    ("terminal", "Terminal"),
    ("files", "Files"),
    ("sys-monitor", "System Monitor"),
    ("timer", "Timer"),
    ("planner", "Planner"),
    ("orchestration", "Orchestration"),
    ("browser", "Browser"),
    ("image", "Image"),
    ("links", "Links"),
    ("notes", "Notes"),
    ("calendar", "Calendar"),
    ("kanban", "Kanban"),
];

/// One unchecked planner row lifted out of the folded document.
struct PlanEntry {
    id: String,
    title: String,
    project: Option<String>,
    day: Option<String>,
    order: f64,
}

/// `doc["items"]` is the array `workspace-planner.json` writes; the folded
/// `PlannerState` (an id→item map) and a bare `[...]` are accepted too, so
/// whichever snapshot the caller folded lands here. Unchecked only, sorted
/// like the planner pane: `day` ascending with undated last, then `order`.
fn open_plan_items(doc: &Value) -> Vec<PlanEntry> {
    let items = doc.get("items").unwrap_or(doc);
    let raw: Vec<&Value> = if let Some(entries) = items.as_array() {
        entries.iter().collect()
    } else if let Some(map) = items.as_object() {
        map.values().collect()
    } else {
        Vec::new()
    };
    let mut rows: Vec<PlanEntry> = raw
        .into_iter()
        .filter(|item| !item.get("done").and_then(Value::as_bool).unwrap_or(false))
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?.to_owned();
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default()
                .to_owned();
            if title.is_empty() {
                return None;
            }
            Some(PlanEntry {
                id,
                title,
                project: item
                    .get("project")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                day: item.get("day").and_then(Value::as_str).map(str::to_owned),
                order: item.get("order").and_then(Value::as_f64).unwrap_or(0.0),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        match (a.day.as_deref(), b.day.as_deref()) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    rows
}

/// The section caption — the original's
/// `text-[11px] font-medium tracking-[0.08em] uppercase text-text-faint`.
/// gpui has no letter-spacing; the uppercase text carries it.
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
                .font_weight(FontWeight::MEDIUM)
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

/// The rail's `IconButton`: a 38px square button whose label lived in a
/// hover flyout — here the hover fill alone carries it. `badge` is the
/// accent chip (`bg-accent text-bg` → the sand accent on `BASE`) for
/// counts.
fn rail_button(
    id: impl Into<ElementId>,
    glyph: &'static str,
    badge: usize,
    action: &'static str,
    payload: Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Stateful<Div> {
    let mut button = div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .w(px(38.0))
        .h(px(38.0))
        .flex_none()
        .cursor_pointer()
        .text_sm()
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(glyph)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.sidebar_command(action, payload.clone(), cx);
            }),
        );
    if badge > 0 {
        button = button.child(
            div()
                .absolute()
                .top(px(1.0))
                .right(px(1.0))
                .min_w(px(15.0))
                .h(px(15.0))
                .px(px(3.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(theme::hex(theme::accent::SAND)))
                .text_size(px(9.0))
                .text_color(rgb(theme::hex(theme::monochrome::BASE)))
                .child(badge.to_string()),
        );
    }
    button
}

/// One open widget: title + kind hint, hover reveals `×` — the same
/// pattern the planner rows use (`opacity-0 group-hover:opacity-100`
/// there, `invisible`/`group_hover` here).
fn widget_row(
    widget: &slate_app::projection::Widget,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let group: SharedString = format!("sidebar-widget-{}", widget.id).into();
    let focus_id = widget.id.clone();
    let close_id = widget.id.clone();
    let kind = widget.kind.clone().unwrap_or_else(|| "widget".to_owned());
    div()
        .group(group.clone())
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .h(px(30.0))
        .px_2()
        .cursor_pointer()
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(widget.title.clone()),
        )
        .child(
            div()
                .flex_none()
                .font_family("Departure Mono")
                .text_size(px(9.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(kind),
        )
        .child(
            div()
                .id(format!("sidebar-close-{}", widget.id))
                .flex_none()
                .px_1()
                .text_xs()
                .invisible()
                .group_hover(group, |el| el.visible())
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                        .text_color(rgb(theme::hex(theme::status::DANGER)))
                })
                .child("×")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        // The row below is the focus target — keep the ×
                        // press from reaching it.
                        cx.stop_propagation();
                        this.sidebar_command("close", json!({"id": close_id}), cx);
                    }),
                ),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.sidebar_command("focus", json!({"id": focus_id}), cx);
            }),
        )
}

/// One unchecked planner item: the pane's check box plus the
/// title (and the project when it has one). Press → `plan_toggle`; every
/// row here is open, so `done` goes out `true`.
fn plan_row(
    entry: &PlanEntry,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let item = entry.id.clone();
    let mut row = div()
        .id(format!("sidebar-plan-{}", entry.id))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py(px(4.0))
        .cursor_pointer()
        .text_xs()
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(
            div()
                .flex_none()
                .w(px(12.0))
                .h(px(12.0))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT))),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(entry.title.clone()),
        );
    if let Some(project) = &entry.project {
        row = row.child(
            div()
                .flex_none()
                .font_family("Departure Mono")
                .text_size(px(9.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(project.clone()),
        );
    }
    row.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, _window, cx| {
            cx.stop_propagation();
            this.sidebar_command("plan_toggle", json!({"id": item, "done": true}), cx);
        }),
    )
}

/// An Add chip — the catalog labels the shell's context menu listed,
/// `+ Label` so it reads as a spawn, not a filter.
fn spawn_chip(
    kind: &'static str,
    label: &'static str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    div()
        .id(format!("sidebar-spawn-{kind}"))
        .px_2()
        .py(px(2.0))
        .flex_none()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .cursor_pointer()
        .font_family("Departure Mono")
        .text_size(px(10.0))
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(format!("+ {}", label.to_uppercase()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.sidebar_command("spawn", json!({"kind": kind}), cx);
            }),
        )
}

/// The collapsed rail — `w-rail items-center` with the icon cluster held
/// mid-panel by `flex-1` spacers, exactly the shell's collapsed layout.
/// The cluster's entries map onto `sidebar_command` verbs: `≡` and `✓`
/// expand (badged with the counts they summarize), `+` spawns a terminal.
fn collapsed_rail(
    canvas: &slate_app::projection::CanvasState,
    planner_items: &Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    let open_plans = open_plan_items(planner_items).len();
    let mut rail = div()
        .flex()
        .flex_col()
        .items_center()
        .w_full()
        .h_full()
        .gap(px(6.0))
        .pt_2()
        .pb_2()
        // `»` — the shell's title-bar expand tab lives on the rail itself
        // here; there is no title-bar slot for it in this layout.
        .child(rail_button(
            "sidebar-expand",
            "»",
            0,
            "toggle",
            json!({}),
            cx,
        ));
    rail = rail.child(div().flex_1());
    rail = rail.child(
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(6.0))
            .child(rail_button(
                "sidebar-widgets",
                "≡",
                canvas.widgets.len(),
                "toggle",
                json!({}),
                cx,
            ))
            .child(rail_button(
                "sidebar-planner",
                "✓",
                open_plans,
                "toggle",
                json!({}),
                cx,
            ))
            .child(rail_button(
                "sidebar-new-terminal",
                "+",
                0,
                "spawn",
                json!({"kind": "terminal"}),
                cx,
            )),
    );
    rail.child(div().flex_1())
}

/// The expanded panel — caption header, scrollable sections, the pinned
/// bottom spawn button where the shell pinned `+ New folder`.
fn expanded_panel(
    canvas: &slate_app::projection::CanvasState,
    planner_items: &Value,
    user_name: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    // Open widgets in stacking order — bottom to top, matching the list a
    // raise-walks-through read of `z` gives.
    let mut widgets: Vec<&slate_app::projection::Widget> = canvas.widgets.values().collect();
    widgets.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));

    let plans = open_plan_items(planner_items);
    let workspace = slate_app::workspace::current();

    let mut panel = div().flex().flex_col().w_full().h_full();

    // ── Header: h-11 caption + the collapse control ────────────────
    panel = panel.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.0))
            .px_3()
            .flex_none()
            .border_b_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .child(
                div()
                    .font_family("Departure Mono")
                    .text_size(px(10.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("PANES"),
            )
            .child(
                div()
                    .id("sidebar-collapse")
                    .px_1()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .child("«")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.sidebar_command("toggle", json!({}), cx);
                        }),
                    ),
            ),
    );

    // ── Scroll body — Workspace / Widgets / Add / Planner ──────────
    let mut body = div()
        .id("sidebar-scroll")
        .flex()
        .flex_col()
        .gap_3()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .px_2()
        // Extra bottom clearance — the last Add chip row sat flush
        // against the footer's hairline at the scroll end.
        .pt_2()
        .pb_4();

    // Workspace area — the rail tooltip's `Folders · current: {name}`
    // flattened to one read-only row. Pressing it spawns Files, the
    // pane that browses this directory (the folders flyout's recents
    // have no verb, so they are not listed).
    if let Some(dir) = workspace {
        let name = std::path::Path::new(&dir)
            .file_name()
            .map(|part| part.to_string_lossy().into_owned())
            .filter(|part| !part.is_empty())
            .unwrap_or_else(|| dir.clone());
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(caption("Workspace", None))
                .child(
                    div()
                        .id("sidebar-workspace")
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .px_2()
                        .py(px(6.0))
                        .cursor_pointer()
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
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
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.sidebar_command("spawn", json!({"kind": "files"}), cx);
                            }),
                        ),
                ),
        );
    }

    // Widgets — the open-widgets list.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(caption("Widgets", Some(widgets.len())));
        if widgets.is_empty() {
            section = section.child(
                div()
                    .px_2()
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("No panes open — right-click to add one"),
            );
        }
        for widget in widgets {
            section = section.child(widget_row(widget, cx));
        }
        body = body.child(section);
    }

    // Add — the spawn affordances, catalog order, renderable kinds only.
    {
        let mut chips = div().flex().flex_row().flex_wrap().gap_1().px_1();
        for (kind, label) in SPAWN_KINDS {
            chips = chips.child(spawn_chip(kind, label, cx));
        }
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(caption("Add", None))
                .child(chips),
        );
    }

    // Planner — the unchecked preview.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(caption("Planner", Some(plans.len())));
        if plans.is_empty() {
            section = section.child(
                div()
                    .px_2()
                    .py(px(6.0))
                    .text_size(px(11.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("All clear — no open tasks"),
            );
        } else {
            for entry in plans.iter().take(PREVIEW_MAX) {
                section = section.child(plan_row(entry, cx));
            }
            if plans.len() > PREVIEW_MAX {
                section = section.child(
                    div()
                        .px_2()
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!("+{} more open", plans.len() - PREVIEW_MAX)),
                );
            }
        }
        body = body.child(section);
    }

    panel = panel.child(body);

    // ── Pinned bottom spawn — the shell's `+ New folder` slot ──────
    panel = panel.child(
        div()
            .id("sidebar-new-terminal")
            .mx_2()
            .mb_2()
            .h(px(36.0))
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_2()
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::SOFT)))
            .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
            .font_family("Departure Mono")
            .text_size(px(11.0))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .child("+ NEW TERMINAL")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.sidebar_command("spawn", json!({"kind": "terminal"}), cx);
                }),
            ),
    );

    // ── Account footer — the shell's `border-t` footer row: avatar
    // initials + name opening account settings, gear for the settings
    // modal. The initials circle is squared off like every other tile.
    let initials: String = {
        let mut chars = user_name.trim().chars().filter(|c| c.is_alphanumeric());
        let first = chars.next().map(|c| c.to_ascii_uppercase());
        let second = chars.next().map(|c| c.to_ascii_uppercase());
        match (first, second) {
            (Some(a), Some(b)) => format!("{a}{b}"),
            (Some(a), None) => a.to_string(),
            _ => "YO".to_owned(),
        }
    };
    panel = panel.child(
        div()
            .flex_none()
            .h(px(40.0))
            .px_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .border_t_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .child(
                div()
                    .id("sidebar-account")
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.sidebar_command("settings", json!({}), cx);
                        }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .size(px(22.0))
                            .border_1()
                            .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                            .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .flex()
                            .items_center()
                            .justify_center()
                            .font_family("Departure Mono")
                            .text_size(px(9.0))
                            .text_color(rgb(theme::hex(theme::accent::SAND)))
                            .child(initials),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family("Departure Mono")
                            .text_size(px(11.0))
                            .text_color(rgb(theme::hex(theme::text::DIM)))
                            .child(user_name.to_owned()),
                    ),
            )
            .child(
                div()
                    .id("sidebar-account-settings")
                    .flex_none()
                    .size(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .child(crate::assets::icon(
                        "settings",
                        13.0,
                        theme::hex(theme::text::FAINT),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.sidebar_command("settings", json!({}), cx);
                        }),
                    ),
            ),
    );

    panel
}

/// `App.tsx`'s `<Sidebar>`: the left strip. `open` picks the 200px panel
/// or the 56px rail; the shell keeps it `w-rail` on the canvas and widens
/// it elsewhere — here `prefs.sidebar_open` decides, through `toggle`.
pub fn sidebar_pane(
    canvas: &slate_app::projection::CanvasState,
    planner_items: &Value,
    open: bool,
    user_name: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let width = if open {
        theme::geometry::SIDEBAR_EXPANDED
    } else {
        theme::geometry::RAIL_WIDTH
    };
    div()
        .id("slate-sidebar")
        .h_full()
        .w(px(width))
        .flex_none()
        .flex()
        .flex_col()
        .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
        .border_r_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .overflow_hidden()
        // In the shell the rail sat next to the canvas, not inside it —
        // presses here never began a pan or dropped the selection.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event, _window, cx| cx.stop_propagation()),
        )
        .child(if open {
            expanded_panel(canvas, planner_items, user_name, cx)
        } else {
            collapsed_rail(canvas, planner_items, cx)
        })
}
