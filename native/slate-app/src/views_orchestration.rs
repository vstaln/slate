//! Orchestration pane — the Electron `OrchestrationWidget` on the store the
//! `slate` CLI writes.
//!
//! The store reloads from `orchestration.json` on every canvas render, so the
//! pane is pure: rows come straight from `OrchestrationStore` and every
//! action is a `widget_command` op that the dispatcher turns into the
//! matching `/orchestration/*` POST (`orc_ask_reply`, `orc_allow`,
//! `orc_deny`, `orc_gate`, `orc_ack`, `orc_release`). The original ordered its
//! sections by what needed the user first; this port groups by object —
//! runs, tasks, dispatches, then the inbox — and folds the widget's
//! keep-or-release row into dispatches.
//!
//! A click on a run journals `state.selectedRun` and scopes every section
//! below it; clicking the selected run again clears the scope back to
//! "all runs".

use std::collections::HashMap;

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::orchestration::{
    Dispatch, Gate, Message, OrchestrationStore, OrchestrationTask, Run,
};
use slate_app::theme;

// The store caps messages at 2_000 but a pane is not a pager — every section
// shows a bounded head with a "+N more" tail instead of a wall of rows.
const MAX_RUNS: usize = 12;
const MAX_TASKS: usize = 40;
const MAX_DISPATCHES: usize = 12;
const MAX_INBOX: usize = 12;
const MAX_MAIL: usize = 8;

// fix-orchestration: the pane's slice of CanvasView state. Passed in as data
// because `entity().read()` mid-render would re-borrow the view (see
// `widget_body`), the same reason the files and browser fields ride along.
pub struct OrcState<'a> {
    /// Message id whose text field owns the keyboard (`orc_editing`).
    pub editing: Option<&'a str>,
    /// Typed-but-unsent text per message id (`orc_drafts`) — the original's
    /// `draft` record, keyed the same way.
    pub drafts: &'a HashMap<String, String>,
    /// The last rejected control write (`orc_error`) — the red banner.
    pub error: Option<&'a str>,
}

/// Which text field holds the keyboard. Enter on a reply sends it; Enter on a
/// permission note only releases the field (the buttons send).
#[derive(Clone)]
pub enum OrcEdit {
    Reply(String),
    Note(String),
}

impl OrcEdit {
    pub fn message_id(&self) -> &str {
        match self {
            OrcEdit::Reply(id) | OrcEdit::Note(id) => id,
        }
    }
}

/// First line of `text`, shortened to `max` chars — what a run row and the
/// header show of an objective.
fn excerpt(text: &str, max: usize) -> String {
    let line = text.split('\n').next().unwrap_or("").trim();
    if line.chars().count() > max {
        let head: String = line.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    } else {
        line.to_owned()
    }
}

/// The `Nc` row's tag map — same labels, nearest palette hue per status
/// (the original's accent/amber/emerald/red/orange ladder).
fn task_status(status: &str) -> (&'static str, theme::Rgb) {
    match status {
        "ready" => ("ready", theme::accent::SAND),
        "dispatched" => ("running", theme::status::WARN),
        "completed" => ("done", theme::status::OK),
        "failed" => ("failed", theme::status::DANGER),
        "blocked" => ("blocked", theme::status::WARN),
        _ => ("waiting", theme::text::FAINT),
    }
}

/// A section's uppercase faint label — `Section`'s `h3` in the original.
fn section_header(title: impl Into<String>) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::hex(theme::text::FAINT)))
        .child(title.into())
}

/// One small bordered button — Allow/Deny/Release/ack/option replies share
/// it, styled like the timer pane's pills.
fn orc_button(
    label: impl Into<String>,
    wid: &str,
    action: Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = wid.to_owned();
    div()
        .px_2()
        .py(px(1.0))
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .cursor_pointer()
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(label.into()),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.widget_command(&wid, action.clone(), cx);
            }),
        )
}

/// The bordered card asks, permissions and gates render inside — the
/// original's `bg-bg-hover/25` panel becomes the next rung of greys.
fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        .p_2()
}

/// `id · from · task` — the attribution line atop an inbox card, so the row
/// stays answerable by CLI (`slate reply <id>`) even with no buttons on it.
fn attribution(m: &Message) -> String {
    match &m.task_id {
        Some(task) => format!("{} · {} · {}", m.id, m.from, task),
        None => format!("{} · {}", m.id, m.from),
    }
}

/// The ask's displayed text — `body || subject`, the widget's own fallback.
fn question_text(m: &Message) -> String {
    if m.body.is_empty() {
        m.subject.clone()
    } else {
        m.body.clone()
    }
}

/// Unanswered and nobody has acked it yet — once a reply exists the widget
/// dropped the row even unacked, and a `slate check` ack settles it too.
pub(crate) fn pending(m: &Message, store: &OrchestrationStore) -> bool {
    m.acked_by.is_empty() && store.reply_to(&m.id).is_none()
}

/// A one-line text field — the original's `<input>`. Canvas has no DOM input,
/// so the buffer is `CanvasView::orc_drafts` and keystrokes route through
/// `orc_edit_key` while the field is armed (`active`). Clicking it arms it.
fn field(
    id: &str,
    kind: &str,
    draft: &str,
    placeholder: &str,
    active: bool,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = wid.to_owned();
    let action = json!({ "op": "orc_edit", "id": id, "kind": kind });
    let text = if draft.is_empty() && !active {
        div()
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .child(placeholder.to_owned())
    } else {
        div()
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .child(format!("{draft}{}", if active { "▏" } else { "" }))
    };
    div()
        .flex_1()
        .min_w_0()
        .px_2()
        .py(px(1.0))
        .border_1()
        .border_color(rgb(theme::hex(if active {
            theme::accent::SAND
        } else {
            theme::hairline::SOFT
        })))
        .cursor_text()
        .text_xs()
        .child(text)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.widget_command(&wid, action.clone(), cx);
            }),
        )
}

fn ask_card(
    m: &Message,
    draft: &str,
    active: bool,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut el = card()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!("ask · {}", attribution(m))),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(question_text(m)),
        );
    if !m.options.is_empty() {
        let mut row = div().flex().flex_row().flex_wrap().gap_1();
        for option in &m.options {
            row = row.child(orc_button(
                option.clone(),
                wid,
                json!({ "op": "orc_ask_reply", "id": m.id, "body": option }),
                cx,
            ));
        }
        el = el.child(row);
    }
    // The original's "Answer…" input and its send button, both here.
    el.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .child(field(&m.id, "reply", draft, "Answer…", active, wid, cx))
            .child(orc_button(
                "Send",
                wid,
                json!({ "op": "orc_ask_reply", "id": m.id, "body": draft }),
                cx,
            )),
    )
}

fn permission_card(
    m: &Message,
    draft: &str,
    active: bool,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    card()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!("permission · {}", attribution(m))),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(question_text(m)),
        )
        .child(field(
            &m.id,
            "note",
            draft,
            "Optional note…",
            active,
            wid,
            cx,
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_1()
                .child(orc_button(
                    "Allow",
                    wid,
                    json!({ "op": "orc_allow", "id": m.id, "note": draft }),
                    cx,
                ))
                .child(orc_button(
                    "Deny",
                    wid,
                    json!({ "op": "orc_deny", "id": m.id, "note": draft }),
                    cx,
                )),
        )
}

fn gate_card(
    gate: &Gate,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let el = card()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(match &gate.task_id {
                    Some(task) => format!("gate · {} · {}", gate.id, task),
                    None => format!("gate · {}", gate.id),
                }),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(gate.question.clone()),
        );
    let mut row = div().flex().flex_row().flex_wrap().gap_1();
    // `(options.length ? options : ["ok"])` — an optionless gate still got
    // one resolve button in the original.
    let options: &[String] = if gate.options.is_empty() {
        &["ok".to_owned()]
    } else {
        &gate.options
    };
    for option in options {
        row = row.child(orc_button(
            option.clone(),
            wid,
            json!({ "op": "orc_gate", "id": gate.id, "resolution": option }),
            cx,
        ));
    }
    el.child(row)
}

/// A `worker_done`/`escalation` line with its ack — the pane-level version
/// of working through `slate check`.
fn ack_row(
    m: &Message,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let label = match &m.outcome {
        Some(outcome) => format!("{} · {} · {}", m.message_type, m.from, outcome),
        None => format!("{} · {}", m.message_type, m.from),
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .flex_none()
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .child(label),
                )
                .child(
                    div()
                        .text_xs()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(if m.body.is_empty() {
                            m.subject.clone()
                        } else {
                            m.body.clone()
                        }),
                ),
        )
        .child(orc_button(
            "ack",
            wid,
            json!({ "op": "orc_ack", "id": m.id }),
            cx,
        ))
}

/// `… +N more` — the tail a capped section shows instead of hiding the
/// remainder silently.
fn capped_tail(total: usize, shown: usize) -> Option<impl IntoElement> {
    (total > shown).then(|| {
        div()
            .text_xs()
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .child(format!("… +{} more", total - shown))
    })
}

fn run_row(
    run: &Run,
    selected: bool,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let open = run.closed_at.is_none();
    let action = if selected {
        json!({ "op": "set", "state": { "selectedRun": Value::Null } })
    } else {
        json!({ "op": "set", "state": { "selectedRun": run.id.clone() } })
    };
    let wid = wid.to_owned();
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_2()
        .px_1()
        .py(px(1.0))
        .cursor_pointer()
        .when(selected, |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
        })
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(
            div()
                .text_xs()
                .w(px(40.0))
                .flex_none()
                .text_color(rgb(theme::hex(if open {
                    theme::accent::SAND
                } else {
                    theme::text::FAINT
                })))
                .child(if open { "open" } else { "closed" }),
        )
        .child(
            div()
                .text_sm()
                .flex_none()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(run.id.clone()),
        )
        .child(
            div()
                .text_xs()
                .min_w_0()
                .truncate()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(excerpt(&run.objective, 64)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.widget_command(&wid, action.clone(), cx);
            }),
        )
}

fn task_row(task: &OrchestrationTask, dispatches: &[Dispatch]) -> impl IntoElement {
    let (label, color) = task_status(&task.status);
    let running_agent = dispatches
        .iter()
        .find(|d| d.task_id == task.id && d.state == "running")
        .map(|d| d.agent.clone());
    let mut row = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_2()
        .px_1()
        .child(
            div()
                .text_xs()
                .w(px(48.0))
                .flex_none()
                .text_color(rgb(theme::hex(color)))
                .child(label),
        )
        .child(
            div()
                .text_sm()
                .min_w_0()
                .truncate()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(task.title.clone()),
        );
    if !task.deps.is_empty() {
        row = row.child(
            div()
                .text_xs()
                .flex_none()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!("↳{}", task.deps.len())),
        );
    }
    // The original's trailing span: which agent is on this task right now.
    if let Some(agent) = running_agent {
        row = row.child(
            div()
                .text_xs()
                .flex_none()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(agent),
        );
    }
    row
}

fn dispatch_row(
    d: &Dispatch,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    if d.state == "running" {
        return div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap_2()
            .px_1()
            .child(
                div()
                    .text_xs()
                    .w(px(48.0))
                    .flex_none()
                    .text_color(rgb(theme::hex(theme::status::WARN)))
                    .child("running"),
            )
            .child(
                div()
                    .text_sm()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .child(format!("{} → {}", d.agent, d.task_id)),
            )
            .child(
                div()
                    .text_xs()
                    .flex_none()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(format!("{} · {}", d.id, d.terminal_id)),
            );
    }
    // Settled but unaccounted — the original's "Finished — keep or release"
    // row: outcome mark, task, agent, and the release that accounts for it.
    let failed = d.outcome.as_deref() == Some("failed");
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_2()
        .px_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .text_xs()
                        .flex_none()
                        .text_color(rgb(theme::hex(if failed {
                            theme::status::DANGER
                        } else {
                            theme::status::OK
                        })))
                        .child(if failed { "✗" } else { "✓" }),
                )
                .child(
                    div()
                        .text_sm()
                        .flex_none()
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .child(d.task_id.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!("{} · {}", d.agent, d.id)),
                ),
        )
        // The original's Keep (`retained`) and Release (`released`) pair.
        .child(
            div()
                .flex()
                .flex_row()
                .flex_none()
                .gap_1()
                .child(orc_button(
                    "Keep",
                    wid,
                    json!({ "op": "orc_retain", "id": d.id }),
                    cx,
                ))
                .child(orc_button(
                    "Release",
                    wid,
                    json!({ "op": "orc_release", "id": d.id }),
                    cx,
                )),
        )
}

fn mail_row(m: &Message) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_1()
        .px_1()
        .child(
            div()
                .text_xs()
                .flex_none()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(m.message_type.clone()),
        )
        .child(
            div()
                .text_xs()
                .min_w_0()
                .truncate()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!("· {} → {} · {}", m.from, m.to, m.subject)),
        )
}

pub fn orchestration_pane(
    store: &OrchestrationStore,
    widget: &slate_app::projection::Widget,
    orc: OrcState<'_>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = widget.id.clone();
    let runs = store.list_runs();

    // `state.selectedRun` picks the pane's scope. A stale id (run removed,
    // file edited by hand) falls back to unscoped rather than showing an
    // empty pane forever.
    let selected: Option<&Run> = widget
        .state
        .as_ref()
        .and_then(|state| state.get("selectedRun"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .and_then(|id| runs.iter().find(|run| run.id == id));
    let scope = selected.map(|run| run.id.as_str());

    let tasks = store.list_tasks(scope, None, false);
    let dispatches = store.list_dispatches(scope, None, None);
    let gates = store.list_gates(scope, true);
    let messages = store.list_messages(scope);

    let running: Vec<&Dispatch> = dispatches.iter().filter(|d| d.state == "running").collect();
    // A settled dispatch the coordinator has not accounted for yet is the
    // only finished kind worth a row — retained and released are done.
    let settled: Vec<&Dispatch> = dispatches.iter().filter(|d| d.state == "settled").collect();
    let ready_n = tasks.iter().filter(|t| t.status == "ready").count();

    let permissions: Vec<&Message> = messages
        .iter()
        .filter(|m| m.message_type == "permission" && pending(m, store))
        .collect();
    let asks: Vec<&Message> = messages
        .iter()
        .filter(|m| m.message_type == "ask" && pending(m, store))
        .collect();
    let notices: Vec<&Message> = messages
        .iter()
        .filter(|m| {
            matches!(m.message_type.as_str(), "worker_done" | "escalation") && m.acked_by.is_empty()
        })
        .collect();

    let mut col = div().flex().flex_col().gap_2().size_full();

    if runs.is_empty() {
        // The original's empty state, with the native CLI's verb.
        return col
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .child("No run yet."),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("slate run-create --objective \"…\""),
            );
    }

    // Header — the original showed the run's objective, coordinator and the
    // running/ready counts. With no run selected the counts span all runs.
    {
        let (title, sub) = match selected {
            Some(run) => (
                excerpt(&run.objective, 72),
                format!("{} · coordinator {}", run.id, run.coordinator),
            ),
            None => (
                "Orchestration".to_owned(),
                "all runs — click a run to focus".to_owned(),
            ),
        };
        col = col.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .pb_1()
                .border_b_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                                .child(title),
                        )
                        .child(
                            div()
                                .text_xs()
                                .flex_none()
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .child(format!("{} running · {} ready", running.len(), ready_n)),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(sub),
                ),
        );
    }

    // The control-write banner — the original's `actionError` strip. A
    // rejected POST lands here until dismissed or the next write succeeds.
    if let Some(error) = orc.error {
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .px_1()
                .py(px(2.0))
                .border_1()
                .border_color(rgb(theme::hex(theme::status::DANGER)))
                .child(
                    div()
                        .text_xs()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(theme::hex(theme::status::DANGER)))
                        .child(error.to_owned()),
                )
                .child(orc_button(
                    "Dismiss",
                    &wid,
                    json!({ "op": "orc_dismiss_error" }),
                    cx,
                )),
        );
    }

    // Everything below the header scrolls as one region; the header stays
    // pinned at the top of the pane.
    let mut list = div()
        .id(format!("slate-orc-body-{}", wid))
        .flex()
        .flex_col()
        .gap_2()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();

    // Actionable first, like the original: permission → waiting on you →
    // decision gates → ackable reports. Each card's text field reads its
    // draft and says whether it holds the keyboard.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(section_header("INBOX"));
        if permissions.is_empty() && asks.is_empty() && gates.is_empty() && notices.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("Nothing waiting on you"),
            );
        }
        for m in permissions.iter().take(MAX_INBOX) {
            let draft = orc.drafts.get(&m.id).map(String::as_str).unwrap_or("");
            let active = orc.editing == Some(m.id.as_str());
            section = section.child(permission_card(m, draft, active, &wid, cx));
        }
        for m in asks.iter().take(MAX_INBOX) {
            let draft = orc.drafts.get(&m.id).map(String::as_str).unwrap_or("");
            let active = orc.editing == Some(m.id.as_str());
            section = section.child(ask_card(m, draft, active, &wid, cx));
        }
        for gate in &gates {
            section = section.child(gate_card(gate, &wid, cx));
        }
        for m in notices.iter().take(MAX_INBOX) {
            section = section.child(ack_row(m, &wid, cx));
        }
        list = list.child(section);
    }

    // Finished — the original's "keep or release" rows. Only settled and
    // unaccounted dispatches; hidden entirely when there are none.
    if !settled.is_empty() {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(section_header(format!(
                "FINISHED — KEEP OR RELEASE ({})",
                settled.len()
            )));
        for d in settled.iter().take(MAX_DISPATCHES) {
            section = section.child(dispatch_row(d, &wid, cx));
        }
        if let Some(tail) = capped_tail(settled.len(), MAX_DISPATCHES) {
            section = section.child(tail);
        }
        list = list.child(section);
    }

    // Runs — click scopes the whole pane (journaled `selectedRun`), click the
    // selected one again to go back to all runs.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(section_header(format!("RUNS ({})", runs.len())));
        for run in runs.iter().take(MAX_RUNS) {
            section = section.child(run_row(run, scope == Some(run.id.as_str()), &wid, cx));
        }
        if let Some(tail) = capped_tail(runs.len(), MAX_RUNS) {
            section = section.child(tail);
        }
        list = list.child(section);
    }

    // Tasks — the original's `Nc` rows: status tag, title, dep count, and
    // the agent of the dispatch currently running it.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(section_header(format!("TASKS ({})", tasks.len())));
        if tasks.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("No tasks yet — slate task-create --spec \"…\""),
            );
        }
        for task in tasks.iter().take(MAX_TASKS) {
            section = section.child(task_row(task, &dispatches));
        }
        if let Some(tail) = capped_tail(tasks.len(), MAX_TASKS) {
            section = section.child(tail);
        }
        list = list.child(section);
    }

    // Running dispatches — the live workers.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(section_header(format!("RUNNING ({})", running.len())));
        if running.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("No dispatches running"),
            );
        }
        for d in running.iter().take(MAX_DISPATCHES) {
            section = section.child(dispatch_row(d, &wid, cx));
        }
        list = list.child(section);
    }

    // Recent mail — the original's last-12 tail, newest first.
    {
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(section_header("RECENT MAIL"));
        if messages.is_empty() {
            section = section.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("No messages yet"),
            );
        }
        for m in messages.iter().rev().take(MAX_MAIL) {
            section = section.child(mail_row(m));
        }
        if let Some(tail) = capped_tail(messages.len(), MAX_MAIL) {
            section = section.child(tail);
        }
        list = list.child(section);
    }

    col.child(list)
}
