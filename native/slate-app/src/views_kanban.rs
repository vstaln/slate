//! Kanban pane — the read half of the old Electron `KanbanWidget`.
//!
//! The original did not have its own store: it read `planner.list()` and
//! grouped `PlanItem`s into Todo / Doing / Done columns by `status` (falling
//! back to `done ? done : todo`). Same data path here: the planner snapshot
//! plus journal tail folded through `planner::reduce`, which carries the
//! `resolveStatus` semantics — an explicit `status` wins a same-call `done`,
//! and `done`/`toggle` patches translate to a column. A card click journals
//! a real `plan.update` status patch, so every surface — planner pane, CLI,
//! kanban — reads the same folded state.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Todo,
    Doing,
    Done,
}

impl Column {
    /// The `status` string the Electron store persisted for the column.
    fn key(self) -> &'static str {
        match self {
            Column::Todo => "todo",
            Column::Doing => "doing",
            Column::Done => "done",
        }
    }

    /// Click cycles Todo → Doing → Done → Todo, the widget's advance-card
    /// affordance.
    fn next(self) -> Column {
        match self {
            Column::Todo => Column::Doing,
            Column::Doing => Column::Done,
            Column::Done => Column::Todo,
        }
    }
}

pub struct Card {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    pub column: Column,
    pub order: f64,
}

/// Cards grouped by column, each column ordered by `order` like the widget.
/// `(Todo, Doing, Done)` — the column order the original's COLUMNS fixed.
pub fn kanban_columns() -> ([Vec<Card>; 3], Option<String>) {
    let dir = slate_app::ipc::user_data_dir();
    let log = match slate_app::journal_log::JournalLog::open(dir.join("command-journal.ndjson")) {
        Ok(log) => log,
        Err(error) => return ([Vec::new(), Vec::new(), Vec::new()], Some(error)),
    };

    // Snapshot items (status normalized on open, like the Electron `revive`)
    // plus the pure `planner::reduce` fold over the journal tail — the typed
    // fold carries `status`, so no separate status overlay is needed.
    let planner_path = dir.join("workspace-planner.json");
    let document = match slate_app::planner_document::PlannerDocument::open(&planner_path) {
        Ok(document) => document,
        Err(error) => return ([Vec::new(), Vec::new(), Vec::new()], Some(error)),
    };
    let items = match document.folded_items(&log, &slate_app::planner::today_local()) {
        Ok(items) => items,
        Err(error) => return ([Vec::new(), Vec::new(), Vec::new()], Some(error)),
    };

    let mut columns: [Vec<Card>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (_, item) in items.iter() {
        let column = match item.status.unwrap_or(if item.done {
            slate_app::planner::PlanStatus::Done
        } else {
            slate_app::planner::PlanStatus::Todo
        }) {
            slate_app::planner::PlanStatus::Todo => Column::Todo,
            slate_app::planner::PlanStatus::Doing => Column::Doing,
            slate_app::planner::PlanStatus::Done => Column::Done,
        };
        columns[column as usize].push(Card {
            id: item.id.clone(),
            title: item.title.clone(),
            project: item.project.clone(),
            column,
            order: item.order,
        });
    }
    for cards in &mut columns {
        cards.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    (columns, None)
}

fn card_view<'a>(
    card: &'a Card,
    widget_id: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    let mut el = div()
        .flex()
        .flex_col()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        .px_2()
        .py_1()
        .cursor_pointer()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(if card.column == Column::Done {
                    theme::text::FAINT
                } else {
                    theme::text::NORMAL
                })))
                .when(card.column == Column::Done, |el| el.line_through())
                .child(card.title.clone()),
        );
    if let Some(project) = &card.project {
        el = el.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(project.clone()),
        );
    }
    // Click advances the card Todo → Doing → Done → Todo — a journaled
    // `plan.update` status patch (`PATCH /planner/{id}`), the same entry
    // `slate plan update` writes, so the planner pane and any other reader
    // fold to the same column. fix-panes2: via `kanban_move`, the REST route.
    let widget_id = widget_id.to_owned();
    let card_id = card.id.clone();
    let move_action =
        json!({ "op": "kanban_move", "id": card_id, "status": card.column.next().key() });
    el.on_mouse_down(
        MouseButton::Left,
        cx.listener(move |this, _event, _window, cx| {
            this.widget_command(&widget_id, move_action.clone(), cx);
        }),
    )
}

fn column_view<'a>(
    label: &'a str,
    key: &'a str,
    cards: &'a [Card],
    collapsed: bool,
    collapsed_map: &'a serde_json::Map<String, Value>,
    widget_id: &'a str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    let wid = widget_id.to_owned();
    let mut next_collapsed = collapsed_map.clone();
    next_collapsed.insert(key.to_owned(), json!(!collapsed));
    let collapse_action = json!({ "op": "set", "state": { "collapsed": next_collapsed } });
    let col = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .px_2()
                .py_1()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.widget_command(&wid, collapse_action.clone(), cx);
                    }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .child(if collapsed { "▸" } else { "▾" }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                                .child(label.to_owned()),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(cards.len().to_string()),
                ),
        );
    if collapsed {
        return col;
    }
    let mut body = div()
        .id(format!("slate-kanban-col-{}-{}", widget_id, key))
        .flex()
        .flex_col()
        .gap_1()
        .px_1()
        .pb_1()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    for card in cards {
        body = body.child(card_view(card, widget_id, cx));
    }
    col.child(body)
}

pub fn kanban_pane(
    widget: &slate_app::projection::Widget,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let (grouped, error) = kanban_columns();
    let state = widget.state.as_ref();
    let collapsed_map = state
        .and_then(|s| s.get("collapsed"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let mut root = div().flex().flex_col().gap_1().size_full();
    if let Some(error) = &error {
        root = root.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::status::DANGER)))
                .child(format!("kanban: {error}")),
        );
    }
    if grouped.iter().all(Vec::is_empty) && error.is_none() {
        root = root.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child("No cards yet — add them to the planner"),
        );
    }
    let is_collapsed = |key: &str| {
        collapsed_map
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    root.child(
        div()
            .flex()
            .flex_row()
            .gap_2()
            .flex_1()
            .min_h_0()
            .child(column_view(
                "Todo",
                "todo",
                &grouped[0],
                is_collapsed("todo"),
                &collapsed_map,
                &widget.id,
                cx,
            ))
            .child(column_view(
                "Doing",
                "doing",
                &grouped[1],
                is_collapsed("doing"),
                &collapsed_map,
                &widget.id,
                cx,
            ))
            .child(column_view(
                "Done",
                "done",
                &grouped[2],
                is_collapsed("done"),
                &collapsed_map,
                &widget.id,
                cx,
            )),
    )
}
