//! Planner pane — the Electron `PlannerWidget` port.
//!
//! Items fold from the `workspace-planner.json` snapshot plus the
//! command-journal tail, same as `slate plan list` computes them. The
//! original's scopes were never a stored field: they derive from `day`
//! against the *local* calendar day (`ln()` in the renderer, `today_local`
//! here):
//!
//! * `inbox` — no `day` ("Inbox (No Date)")
//! * `today` — `day == today`
//! * `week`  — `today <= day <= today + 6`
//! * `all`   — every item
//!
//! So moving a row between scopes is a `day` patch, not a `status` patch —
//! that is how the original's reschedule (`planner.update(id, {day})`)
//! worked, and the typed `planner::PlanItem` has no `status` field at all
//! (the kanban pane overlays one separately; the planner widget never did).
//!
//! Pane actions go through `widget_command`:
//!
//! * `{"op": "plan_toggle", "id", "done"}` — existing arm, `plan.toggle`.
//! * `{"op": "plan_move", "id", "status", "day"}` — scope cycle. `status`
//!   names the target scope (`"inbox"|"today"|"week"`); `day` is the
//!   resolved `YYYY-MM-DD`, or JSON `null` for inbox. The dispatcher should
//!   journal `plan.update` on `plan:{id}` with `{"day": <day>}` — the `day`
//!   key must be present even when null, because the fold reads absent as
//!   "leave alone" and `null` as "clear the schedule".
//! * `{"op": "plan_create", "title", "day"?, "project"?}` — `plan.create`
//!   on `plan:new`; the payload is the action minus `op`, using exactly the
//!   keys `slate plan create` writes (`title`, `day`, `project`, ...).
//! * `{"op": "plan_update", "id", "patch": {...}}` — `plan.update` on
//!   `plan:{id}` with `patch` verbatim (reserved for per-field edits).
//! * `{"op": "plan_delete", "id"}` — `plan.delete` on `plan:{id}`, `{}`.
//! * `{"op": "set", "state": {"scope": ...}}` / `{"project": ...}` — the
//!   tab and the project filter live in journaled widget state.
//!
//! fix-planner: the create form (title / project / date + staged photos) and
//! the inline title / note / date edits are CanvasView state
//! (`planner_editing`, `planner_draft`), the same single-owner shape as the
//! browser omnibox. Field commits emit the existing `plan_update` op with a
//! `patch`; the create emits `plan_create` (via `planner_add`). Photos are
//! attached with the paperclip (file prompt) or by pasting a clipboard image.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

/// One planner row lifted out of the folded document.
pub struct PlannerRow {
    pub id: String,
    pub title: String,
    pub note: String,
    pub project: Option<String>,
    pub day: Option<String>,
    pub time: Option<String>,
    pub done: bool,
    pub order: f64,
    /// fix-planner: the item's photo paths, folded from `attachments`.
    pub attachments: Vec<String>,
}

/// fix-planner: the original's `pendingCreateAttachments` cap (`>= 12`).
pub(crate) const MAX_ATTACHMENTS: usize = 12;

/// fix-planner: the create form's draft — `title`, `project`, `day` as typed,
/// plus the staged photo paths. `day` empty = the scope default (`add()`).
#[derive(Default)]
pub(crate) struct PlannerDraft {
    pub title: String,
    pub project: String,
    pub day: String,
    pub attachments: Vec<String>,
}

/// The one text field that owns the keyboard — the create form's inputs or a
/// row's inline title / note / date.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum PlannerField {
    CreateTitle,
    CreateProject,
    CreateDay,
    Title(String),
    Note(String),
    Day(String),
}

/// Where a photo lands: the create draft, or an existing item's list.
#[derive(Clone)]
pub(crate) enum PlannerAttachTarget {
    Draft,
    Item(String),
}

/// An open field: `buffer` is what the user is typing, `cursor` a byte offset
/// on a char boundary, `seed` the value when focus arrived (an unchanged row
/// field commits nothing).
#[derive(Clone)]
pub(crate) struct PlannerEdit {
    pub wid: String,
    pub field: PlannerField,
    pub buffer: String,
    pub cursor: usize,
    pub seed: String,
}

impl PlannerField {
    pub(crate) fn is_create(&self) -> bool {
        matches!(
            self,
            PlannerField::CreateTitle | PlannerField::CreateProject | PlannerField::CreateDay
        )
    }

    pub(crate) fn attach_target(&self) -> PlannerAttachTarget {
        match self {
            PlannerField::Title(id) | PlannerField::Note(id) | PlannerField::Day(id) => {
                PlannerAttachTarget::Item(id.clone())
            }
            _ => PlannerAttachTarget::Draft,
        }
    }
}

impl PlannerDraft {
    /// The draft slot a create field edits; `None` for row fields.
    pub(crate) fn slot(&mut self, field: &PlannerField) -> Option<&mut String> {
        match field {
            PlannerField::CreateTitle => Some(&mut self.title),
            PlannerField::CreateProject => Some(&mut self.project),
            PlannerField::CreateDay => Some(&mut self.day),
            _ => None,
        }
    }

    pub(crate) fn value(&self, field: &PlannerField) -> String {
        match field {
            PlannerField::CreateTitle => self.title.clone(),
            PlannerField::CreateProject => self.project.clone(),
            PlannerField::CreateDay => self.day.clone(),
            _ => String::new(),
        }
    }
}

/// `current` plus `added`, capped at `MAX_ATTACHMENTS` — the staged-photo rule.
pub(crate) fn merge_attachments(current: &[String], added: &[String]) -> Vec<String> {
    current
        .iter()
        .chain(added)
        .take(MAX_ATTACHMENTS)
        .cloned()
        .collect()
}

/// The original's `add()`: validates the draft and builds the `plan_create`
/// action. `Ok(None)` is a blank title (a no-op); `Err` is the notice text for
/// a bad date. Project follows `projectFilter || project.trim()`; `day` is the
/// scope default (today under Today/Week, else undated) unless typed.
pub(crate) fn create_action(
    state: Option<&Value>,
    draft: &PlannerDraft,
) -> Result<Option<Value>, String> {
    let title = draft.title.trim();
    if title.is_empty() {
        return Ok(None);
    }
    let today = slate_app::planner::today_local();
    let scope = Scope::parse(
        state
            .and_then(|s| s.get("scope"))
            .and_then(Value::as_str)
            .unwrap_or("today"),
    );
    let filter = state
        .and_then(|s| s.get("project"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let typed = draft.day.trim();
    let day = if typed.is_empty() {
        matches!(scope, Scope::Today | Scope::Week).then(|| today.0.clone())
    } else {
        slate_app::planner::normalize_day(Some(&json!(typed)), &today)
            .map_err(|e| e.0)?
    };
    let project = filter.or_else(|| {
        let p = draft.project.trim();
        (!p.is_empty()).then(|| p.to_owned())
    });
    let mut action = json!({ "op": "plan_create", "title": title });
    if let Some(day) = day {
        action["day"] = json!(day);
    }
    if let Some(project) = project {
        action["project"] = json!(project);
    }
    if !draft.attachments.is_empty() {
        action["attachments"] = json!(draft.attachments);
    }
    Ok(Some(action))
}

/// The original's `se` tab list, in order. `all` is a filter width, not a
/// bucket an item belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Today,
    Week,
    Inbox,
}

impl Scope {
    const TABS: [Scope; 4] = [Scope::All, Scope::Today, Scope::Week, Scope::Inbox];

    fn key(self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Today => "today",
            Scope::Week => "week",
            Scope::Inbox => "inbox",
        }
    }

    fn parse(key: &str) -> Scope {
        match key {
            "all" => Scope::All,
            "week" => Scope::Week,
            "inbox" => Scope::Inbox,
            _ => Scope::Today,
        }
    }

    /// The original's tab labels (`se`): All / Today / Week / Inbox.
    fn tab_label(self) -> &'static str {
        match self {
            Scope::All => "All",
            Scope::Today => "Today",
            Scope::Week => "Week",
            Scope::Inbox => "Inbox",
        }
    }

    /// The header label the original used for the scope.
    fn label(self) -> &'static str {
        match self {
            Scope::All => "All Tasks",
            Scope::Today => "Today",
            Scope::Week => "This Week",
            Scope::Inbox => "Inbox (No Date)",
        }
    }

    fn contains(self, day: Option<&str>, today: &str, week_end: &str) -> bool {
        match self {
            Scope::All => true,
            Scope::Inbox => day.is_none(),
            Scope::Today => day == Some(today),
            // `hc(day, q, z)`: `day >= today && day <= today + 6` — plain
            // string compare, `YYYY-MM-DD` sorts chronologically.
            Scope::Week => day.is_some_and(|d| d >= today && d <= week_end),
        }
    }
}

/// `day + delta` as `YYYY-MM-DD` — the same civil arithmetic `planner.rs`
/// keeps private (`shift_day`); only `+6` (week end) and `+1` (tomorrow)
/// are needed here.
fn shift_day(key: &str, delta: i64) -> String {
    let parts: Vec<i64> = key.split('-').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 {
        return key.to_owned();
    }
    let days = days_from_civil(parts[0], parts[1], parts[2]) + delta;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = (mp + 2) % 12 + 1;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `"2026-09-19"` → `"19 Sep"` — the chip label `xc` produced with
/// `toLocaleDateString("en", {day: "numeric", month: "short"})`.
fn fmt_day(day: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = day.split('-');
    let (Some(_y), Some(m), Some(d)) = (parts.next(), parts.next(), parts.next()) else {
        return day.to_owned();
    };
    match m.parse::<usize>() {
        Ok(m) if (1..=12).contains(&m) => {
            format!("{} {}", d.trim_start_matches('0'), MONTHS[m - 1])
        }
        _ => day.to_owned(),
    }
}

/// Where a row sits relative to the scopes — input to the move cycle. A
/// `day` outside the week window only shows under `all`.
fn bucket(day: Option<&str>, today: &str, week_end: &str) -> Scope {
    if day.is_none() {
        Scope::Inbox
    } else if day == Some(today) {
        Scope::Today
    } else if day.is_some_and(|d| d >= today && d <= week_end) {
        Scope::Week
    } else {
        Scope::All
    }
}

/// The move cycle, inbox → today → week → inbox. "Week" lands the item on
/// tomorrow: inside `[today, today+6]` but out of the today bucket, so a
/// click visibly walks the row down the tabs. A stale or far-future `day`
/// cycles back to today. Returns `(target scope, resolved day)`.
fn move_target(day: Option<&str>, today: &str, week_end: &str) -> (Scope, Option<String>) {
    match bucket(day, today, week_end) {
        Scope::Inbox => (Scope::Today, Some(today.to_owned())),
        Scope::Today => (Scope::Week, Some(shift_day(today, 1))),
        Scope::Week => (Scope::Inbox, None),
        Scope::All => (Scope::Today, Some(today.to_owned())),
    }
}

pub fn planner_items() -> (Vec<PlannerRow>, Option<String>) {
    let dir = slate_app::ipc::user_data_dir();
    let log = slate_app::journal_log::JournalLog::open(dir.join("command-journal.ndjson"));
    // `today_local` like `cli_run.rs`'s recover and the HTTP fold — the
    // Electron store resolved "today" / "tomorrow" against `localDayKey`,
    // the machine's local calendar day, and every surface must agree.
    let today = slate_app::planner::today_local();
    let (items, error) = match log {
        Ok(log) => match slate_app::planner_document::PlannerDocument::recover(
            dir.join("workspace-planner.json"),
            &log,
            &today,
        ) {
            Ok(doc) => (
                doc.items.into_iter().map(|(_, i)| i).collect::<Vec<_>>(),
                None,
            ),
            Err(e) => (Vec::new(), Some(e)),
        },
        Err(e) => (Vec::new(), Some(e)),
    };
    let rows: Vec<PlannerRow> = items
        .iter()
        .map(|item| PlannerRow {
            id: item.id.clone(),
            title: item.title.clone(),
            note: item.note.clone(),
            project: item.project.clone(),
            day: item.day.clone(),
            time: item.time.clone(),
            done: item.done,
            order: item.order,
            attachments: item.attachments.clone().unwrap_or_default(),
        })
        .collect();
    (rows, error)
}

/// fix-planner: one inline field. At rest it shows `rest` (or `placeholder`
/// when empty); click focuses it (`planner_begin`) with `seed` as the buffer.
/// While it owns the keyboard the buffer shows with a caret at `cursor`.
#[allow(clippy::too_many_arguments)]
fn planner_text(
    base: Div,
    field: PlannerField,
    rest: &str,
    placeholder: &str,
    seed: &str,
    wid: &str,
    editing: Option<&PlannerEdit>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    let faint = rgb(theme::hex(theme::text::FAINT));
    let caret = rgb(theme::hex(theme::text::NORMAL));
    let active = editing.filter(|e| e.field == field);
    let body = match active {
        Some(edit) => {
            let at = edit.cursor.min(edit.buffer.len());
            div()
                .flex()
                .flex_row()
                .items_center()
                .child(edit.buffer[..at].to_owned())
                .child(div().flex_none().w(px(1.)).h(px(12.)).bg(caret))
                .child(edit.buffer[at..].to_owned())
        }
        None if rest.is_empty() => div().text_color(faint).child(placeholder.to_owned()),
        None => div().child(rest.to_owned()),
    };
    let wid = wid.to_owned();
    let seed = seed.to_owned();
    base.min_w_0()
        .overflow_hidden()
        .cursor_text()
        .child(body)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, window, cx| {
                // Don't let the click fall through to the row's toggle.
                cx.stop_propagation();
                window.focus(&this.focus, cx);
                this.planner_begin(wid.clone(), field.clone(), seed.clone(), cx);
            }),
        )
}

/// fix-planner: the photo strip — `att_thumb` in the original. Each thumb
/// removes itself on its × (`planner_remove_attachment`).
fn planner_thumbs(
    paths: &[String],
    target: PlannerAttachTarget,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Option<Div> {
    if paths.is_empty() {
        return None;
    }
    let mut strip = div().flex().flex_row().flex_wrap().gap_1().pl(px(18.));
    for (index, path) in paths.iter().enumerate() {
        let target = target.clone();
        let group = format!("planner-thumb-{index}-{path}");
        strip = strip.child(
            div()
                .group(group.clone())
                .relative()
                .w(px(80.))
                .h(px(64.))
                .overflow_hidden()
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .child(img(std::path::PathBuf::from(path)).size_full())
                .child(
                    div()
                        .absolute()
                        .top_1()
                        .right_1()
                        .px_1()
                        .text_xs()
                        .cursor_pointer()
                        .invisible()
                        .group_hover(group, |el| el.visible())
                        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .text_color(rgb(theme::hex(theme::status::DANGER)))
                        .child("×")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                cx.stop_propagation();
                                this.planner_remove_attachment(target.clone(), index, cx);
                            }),
                        ),
                ),
        );
    }
    Some(strip)
}

/// One hover-affordance button on a row (`→scope` move, `×` delete).
fn row_button<'a>(
    label: &'a str,
    wid: String,
    action: Value,
    danger: bool,
    group: SharedString,
    cx: &'a mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    div()
        .text_xs()
        .cursor_pointer()
        .px_1()
        .invisible()
        .group_hover(group, |el| el.visible())
        .text_color(rgb(theme::hex(theme::text::FAINT)))
        .hover(move |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(if danger {
                    theme::status::DANGER
                } else {
                    theme::text::NORMAL
                })))
        })
        .child(label.to_owned())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                // Don't let the click fall through to the row's toggle.
                cx.stop_propagation();
                this.widget_command(&wid, action.clone(), cx);
            }),
        )
}

#[allow(clippy::too_many_arguments)]
fn planner_row<'a>(
    index: usize,
    row: &'a PlannerRow,
    show_day: bool,
    today: &'a str,
    week_end: &'a str,
    wid: &'a str,
    editing: Option<&'a PlannerEdit>,
    cx: &'a mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    let group: SharedString = format!("plan-row-{}", row.id).into();

    let toggle_action = json!({ "op": "plan_toggle", "id": row.id, "done": !row.done });
    let (target, target_day) = move_target(row.day.as_deref(), today, week_end);
    let move_action = json!({
        "op": "plan_move",
        "id": row.id,
        "status": target.key(),
        "day": target_day,
    });
    let delete_action = json!({ "op": "plan_delete", "id": row.id });

    let line = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            // The original's round check button — done shows a ✓ in `ok`.
            div()
                .flex_none()
                .w(px(14.))
                .h(px(14.))
                .border_1()
                .border_color(rgb(theme::hex(if row.done {
                    theme::status::OK
                } else {
                    theme::hairline::FAINT
                })))
                .text_xs()
                .cursor_pointer()
                .text_color(rgb(theme::hex(theme::status::OK)))
                .hover(|el| el.border_color(rgb(theme::hex(theme::status::OK))))
                .child(if row.done { "✓" } else { "" })
                .on_mouse_down(MouseButton::Left, {
                    let wid = wid.to_owned();
                    cx.listener(move |this, _event, _window, cx| {
                        this.widget_command(&wid, toggle_action.clone(), cx);
                    })
                }),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(index.to_string()),
        )
        .child(planner_text(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_color(rgb(theme::hex(if row.done {
                    theme::text::FAINT
                } else {
                    theme::text::NORMAL
                })))
                .when(row.done, |el| el.line_through()),
            PlannerField::Title(row.id.clone()),
            &row.title,
            "Untitled",
            &row.title,
            wid,
            editing,
            cx,
        ))
        .child(row_button(
            match target {
                Scope::Today => "→today",
                Scope::Week => "→week",
                _ => "→inbox",
            },
            wid.to_owned(),
            move_action,
            false,
            group.clone(),
            cx,
        ))
        // fix-planner: the row's paperclip — the original's `Attach photo`.
        .child(
            div()
                .text_xs()
                .cursor_pointer()
                .px_1()
                .invisible()
                .group_hover(group.clone(), |el| el.visible())
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                })
                .child("+ photo")
                .on_mouse_down(MouseButton::Left, {
                    let id = row.id.clone();
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.planner_pick_photos(PlannerAttachTarget::Item(id.clone()), cx);
                    })
                }),
        )
        .child(row_button(
            "×",
            wid.to_owned(),
            delete_action,
            true,
            group.clone(),
            cx,
        ));

    // The original's sub-line: project · date chip · time · note. The chip
    // is a date picker there; here it's the readout the move cycle edits.
    // The original's sub-line: project · date chip · time · note. fix-planner:
    // the date chip and the note are inline fields — the date takes any
    // `YYYY-MM-DD` (or today/tomorrow/yesterday), an empty one clears it.
    let mut m = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_2()
        .pl(px(18.))
        .text_xs()
        .text_color(rgb(theme::hex(theme::text::FAINT)));
    if let Some(project) = &row.project {
        m = m.child(
            div()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(project.clone()),
        );
    }
    if show_day {
        m = m.child(planner_text(
            div().text_xs(),
            PlannerField::Day(row.id.clone()),
            &row.day.as_deref().map(fmt_day).unwrap_or_default(),
            "no date",
            row.day.as_deref().unwrap_or(""),
            wid,
            editing,
            cx,
        ));
    }
    if let Some(time) = &row.time {
        m = m.child(time.clone());
    }
    // An empty note is a hover-only "+ note" affordance.
    m = m.child(planner_text(
        div().truncate().text_xs().when(row.note.is_empty(), |el| {
            el.invisible().group_hover(group.clone(), |el| el.visible())
        }),
        PlannerField::Note(row.id.clone()),
        &row.note,
        "+ note",
        &row.note,
        wid,
        editing,
        cx,
    ));

    let mut el = div()
        .group(format!("plan-row-{}", row.id))
        .flex()
        .flex_col()
        .px_1()
        .py_1()
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(line)
        .child(m);
    if let Some(strip) = planner_thumbs(
        &row.attachments,
        PlannerAttachTarget::Item(row.id.clone()),
        cx,
    ) {
        el = el.child(strip);
    }
    el
}

/// fix-planner: the original's add row — title (paste / paperclip for photos),
/// project and date inputs, the Add button, then the staged photo strip.
fn planner_create_form(
    draft: &PlannerDraft,
    editing: Option<&PlannerEdit>,
    wid: &str,
    show_project: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let input = |base: Div, field: PlannerField, placeholder: &str, cx: &mut Context<_>| {
        let value = draft.value(&field);
        planner_text(base, field, &value, placeholder, &value, wid, editing, cx)
    };
    let title = input(
        div().flex_1().min_w_0().text_sm().text_color(rgb(theme::hex(theme::text::NORMAL))),
        PlannerField::CreateTitle,
        "Add a task…  (paste a photo)",
        cx,
    );
    let project = show_project.then(|| {
        input(
            div().flex_1().min_w_0().text_xs().text_color(rgb(theme::hex(theme::text::DIM))),
            PlannerField::CreateProject,
            "Project (optional)",
            cx,
        )
    });
    let date = input(
        div().flex_1().min_w_0().text_xs().text_color(rgb(theme::hex(theme::text::DIM))),
        PlannerField::CreateDay,
        "Date YYYY-MM-DD  (empty = scope)",
        cx,
    );
    let button = |label: &'static str| {
        div()
            .flex_none()
            .text_xs()
            .cursor_pointer()
            .px_2()
            .py_1()
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .child(label)
    };
    let add = button("Add").on_mouse_down(MouseButton::Left, {
        let wid = wid.to_owned();
        cx.listener(move |this, _event, _window, cx| {
            cx.stop_propagation();
            this.planner_add(&wid, cx);
        })
    });
    let clip = button("📎").on_mouse_down(
        MouseButton::Left,
        cx.listener(|this, _event, _window, cx| {
            cx.stop_propagation();
            this.planner_pick_photos(PlannerAttachTarget::Draft, cx);
        }),
    );
    let mut form = div()
        .flex()
        .flex_col()
        .gap_1()
        .mx_1()
        .px_2()
        .py_1()
        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .child(title)
                .child(clip)
                .child(add),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .children(project)
                .child(date),
        );
    if let Some(strip) = planner_thumbs(&draft.attachments, PlannerAttachTarget::Draft, cx) {
        form = form.child(strip);
    }
    form
}

pub(crate) fn planner_pane(
    widget: &slate_app::projection::Widget,
    editing: Option<&PlannerEdit>,
    draft: &PlannerDraft,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let (rows, error) = planner_items();
    let state = widget.state.as_ref();
    let scope = Scope::parse(
        state
            .and_then(|s| s.get("scope"))
            .and_then(Value::as_str)
            .unwrap_or("today"),
    );
    // `set` merges, so a cleared filter stays in state as Null — `as_str`
    // reads that as absent, which is the no-filter case.
    let project_filter = state
        .and_then(|s| s.get("project"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // The original's `q = ln()` — the *local* day, matching what the user
    // sees and what the fold path resolves.
    let today = slate_app::planner::today_local().0;
    let week_end = shift_day(&today, 6);

    let in_project = |row: &&PlannerRow| match &project_filter {
        Some(p) => row.project.as_deref() == Some(p.as_str()),
        None => true,
    };
    let in_scope = |scope: Scope| {
        let today = today.as_str();
        let week_end = week_end.as_str();
        move |row: &&PlannerRow| scope.contains(row.day.as_deref(), today, week_end)
    };

    // Unique projects sorted like the original (`de`), over every item —
    // the sidebar there counted across the whole list too.
    let mut projects: Vec<String> = rows.iter().filter_map(|r| r.project.clone()).collect();
    projects.sort();
    projects.dedup();

    // Filter scope → project, then the original's sort: open first, then
    // `day` ascending with undated last, then `order`.
    let mut items: Vec<&PlannerRow> = rows
        .iter()
        .filter(in_scope(scope))
        .filter(in_project)
        .collect();
    items.sort_by(|a, b| {
        a.done
            .cmp(&b.done)
            .then_with(|| match (a.day.as_deref(), b.day.as_deref()) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            })
            .then_with(|| {
                a.order
                    .partial_cmp(&b.order)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });
    let done_count = items.iter().filter(|r| r.done).count();
    let total_count = items.len();
    let wid = widget.id.clone();

    let mut root = div().flex().flex_col().gap_1().size_full();

    // ── Header: filter/scope label + the "G of Z" pill ───────────────
    let mut head_left = div()
        .flex()
        .flex_row()
        .items_baseline()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .truncate()
                .child(
                    project_filter
                        .clone()
                        .unwrap_or_else(|| scope.label().to_owned()),
                ),
        );
    if project_filter.is_some() {
        let wid = wid.clone();
        head_left = head_left.child(
            div()
                .text_xs()
                .cursor_pointer()
                .px_1()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                })
                .child("← all")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.widget_command(
                            &wid,
                            json!({ "op": "set", "state": { "project": null } }),
                            cx,
                        );
                    }),
                ),
        );
    }
    root = root.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_2()
            .px_1()
            .child(head_left)
            .child(
                div()
                    .flex_none()
                    .border_1()
                    .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                    .px_2()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .child(format!("{done_count} of {total_count}")),
            ),
    );

    // ── Scope tabs — open-item counts under the project filter ───────
    let mut tabs = div().flex().flex_row().flex_wrap().gap_1().px_1();
    for tab in Scope::TABS {
        let open = rows
            .iter()
            .filter(in_scope(tab))
            .filter(in_project)
            .filter(|r| !r.done)
            .count();
        let selected = tab == scope;
        let wid = wid.clone();
        tabs = tabs.child(
            div()
                .text_xs()
                .cursor_pointer()
                .px_2()
                .py_1()
                .text_color(rgb(theme::hex(if selected {
                    theme::text::NORMAL
                } else {
                    theme::text::FAINT
                })))
                .when(selected, |el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                })
                .when(!selected, |el| {
                    el.hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                            .text_color(rgb(theme::hex(theme::text::DIM)))
                    })
                })
                .child(format!("{} {open}", tab.tab_label()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.widget_command(
                            &wid,
                            json!({ "op": "set", "state": { "scope": tab.key() } }),
                            cx,
                        );
                    }),
                ),
        );
    }
    root = root.child(tabs);

    // ── Project chips — the original's sidebar, flattened to a row ───
    if !projects.is_empty() {
        let mut chips = div().flex().flex_row().flex_wrap().gap_1().px_1();
        let any_selected = project_filter.is_none();
        let wid_all = wid.clone();
        chips = chips.child(
            div()
                .text_xs()
                .cursor_pointer()
                .px_1()
                .text_color(rgb(theme::hex(if any_selected {
                    theme::text::NORMAL
                } else {
                    theme::text::FAINT
                })))
                .when(any_selected, |el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                })
                .when(!any_selected, |el| {
                    el.hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                            .text_color(rgb(theme::hex(theme::text::DIM)))
                    })
                })
                .child("all projects")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.widget_command(
                            &wid_all,
                            json!({ "op": "set", "state": { "project": null } }),
                            cx,
                        );
                    }),
                ),
        );
        for project in &projects {
            let open = rows
                .iter()
                .filter(|r| r.project.as_deref() == Some(project.as_str()) && !r.done)
                .count();
            let selected = project_filter.as_deref() == Some(project.as_str());
            let wid = wid.clone();
            let name = project.clone();
            chips = chips.child(
                div()
                    .text_xs()
                    .cursor_pointer()
                    .px_1()
                    .truncate()
                    .text_color(rgb(theme::hex(if selected {
                        theme::text::NORMAL
                    } else {
                        theme::text::FAINT
                    })))
                    .when(selected, |el| {
                        el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    })
                    .when(!selected, |el| {
                        el.hover(|el| {
                            el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                        })
                    })
                    .child(format!("{project} {open}"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            this.widget_command(
                                &wid,
                                json!({ "op": "set", "state": { "project": name } }),
                                cx,
                            );
                        }),
                    ),
            );
        }
        root = root.child(chips);
    }

    // ── Create form — the original's add row (see planner_create_form) ──
    root = root.child(planner_create_form(
        draft,
        editing,
        &wid,
        project_filter.is_none(),
        cx,
    ));

    if let Some(e) = &error {
        root = root.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::status::DANGER)))
                .child(format!("planner: {e}")),
        );
    }

    // ── The list ──────────────────────────────────────────────────────
    let show_day = scope != Scope::Today;
    if items.is_empty() && error.is_none() {
        root = root.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(match scope {
                    Scope::Today => {
                        "Nothing planned for today. Add a task above — items persist and sync with CLI & Planner."
                    }
                    Scope::Inbox => {
                        "No unscheduled tasks. Use inbox to drop ideas and assign dates later."
                    }
                    _ => "Plan list is empty. Add tasks above to track your day.",
                }),
        );
    }
    let mut list = div()
        .id(format!("slate-planner-list-{}", wid))
        .flex()
        .flex_col()
        .gap_1()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    for (index, row) in items.iter().enumerate() {
        list = list.child(planner_row(
            index + 1,
            row,
            show_day,
            &today,
            &week_end,
            &wid,
            editing,
            cx,
        ));
    }
    root.child(list)
}
