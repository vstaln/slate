//! Calendar pane — the read half of the old Electron `CalendarWidget`.
//!
//! Same data path as the planner and kanban panes: `CalendarWidget` read
//! `planner.list()` and dropped each item onto its `day` (`YYYY-MM-DD`)
//! inside a Monday-first 6×7 month grid. The original kept a browsable
//! anchor month and a selected day in localStorage; here both live in the
//! widget's journaled `state` (`{year, month, selected_day}`), so the browsed
//! month and the highlighted day survive a restart exactly like the
//! localStorage pair did.
//!
//! Today is drawn as the accent disc (white on this palette) the original
//! used; the 42-cell grid with greyed spill-over days is the same shape.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::json;
use slate_app::theme;
use std::collections::HashMap;

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Howard Hinnant's civil-date math — the same pair `planner.rs` carries,
/// duplicated because those are private to the reducer.
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
    let yoe = (doe - doe / 1460 + doe / 36 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = (mp + 2) % 12 + 1;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` for one grid cell.
fn day_key(y: i64, m: i64, d: i64) -> String {
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's `(year, month, day)` — local, matching `planner::today_local`
/// which resolves "today" in the planner store against the machine's local
/// calendar day (`localDayKey` in the Electron shell). Near midnight UTC and
/// local disagree, and only the local reading matches what the user sees.
fn today_parts() -> (i64, i64, i64) {
    let key = slate_app::planner::today_local().0;
    let parts: Vec<i64> = key.split('-').filter_map(|p| p.parse().ok()).collect();
    if parts.len() == 3 {
        (parts[0], parts[1], parts[2])
    } else {
        // Unreachable — today_local always formats YYYY-MM-DD — but never
        // let a malformed key panic the grid; fall back to the epoch day.
        let days = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs() / 86_400) as i64;
        civil_from_days(days)
    }
}

/// Planner items keyed by their `day`, then trimmed to display order.
/// `PlannerDocument::open` + the pure `planner::reduce` tail fold — same
/// view `recover` computes, but without writing the consolidated snapshot
/// back to disk; the pane stays strictly read-only.
pub fn items_by_day() -> (HashMap<String, Vec<String>>, Option<String>) {
    let dir = slate_app::ipc::user_data_dir();
    let log = match slate_app::journal_log::JournalLog::open(dir.join("command-journal.ndjson")) {
        Ok(log) => log,
        Err(error) => return (HashMap::new(), Some(error)),
    };
    let planner_path = dir.join("workspace-planner.json");
    let document = match slate_app::planner_document::PlannerDocument::open(&planner_path) {
        Ok(document) => document,
        Err(error) => return (HashMap::new(), Some(error)),
    };
    let items = match document.folded_items(&log, &slate_app::planner::today_local()) {
        Ok(items) => items,
        Err(error) => return (HashMap::new(), Some(error)),
    };
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (_, item) in items.iter() {
        if let Some(day) = &item.day {
            map.entry(day.clone()).or_default().push(if item.done {
                format!("{} ✓", item.title)
            } else {
                item.title.clone()
            });
        }
    }
    (map, None)
}

/// The `(year, month)` the pane is showing — `widget.state` first, today as
/// the fallback, so a fresh widget opens on the current month like the
/// original did before localStorage had a cursor.
fn viewed_month(widget: &slate_app::projection::Widget, today_y: i64, today_m: i64) -> (i64, i64) {
    let num = |key: &str| widget.state.as_ref()?.get(key)?.as_i64();
    let year = num("year").unwrap_or(today_y);
    let month = num("month").unwrap_or(today_m);
    if (1..=12).contains(&month) {
        (year, month)
    } else {
        (today_y, today_m)
    }
}

/// A clickable header control (‹ › Today) — a journaled `op:set` away from
/// the Electron widget's prev/next buttons.
fn nav_button(
    widget_id: &str,
    label: &'static str,
    patch: serde_json::Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let widget_id = widget_id.to_owned();
    div()
        .px_1()
        .cursor_pointer()
        .text_xs()
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.widget_command(&widget_id, json!({ "op": "set", "state": patch }), cx);
            }),
        )
}

/// One grid cell: day number (accent disc when today, faint when the day
/// belongs to a neighbouring month), a task dot when the day holds planner
/// items, then up to two item titles and a `+N more` tail — the original
/// showed three; a compact pane keeps two. Clicking selects the day into
/// `widget.state.selected_day`.
fn day_cell(
    widget_id: &str,
    key: String,
    day_of_month: i64,
    (in_month, is_today, is_selected): (bool, bool, bool),
    titles: &[String],
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let disc = div()
        .size(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .when(is_today, |el| el.bg(rgb(theme::hex(theme::text::NORMAL))))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(if is_today {
                    theme::monochrome::BASE
                } else if in_month {
                    theme::text::DIM
                } else {
                    theme::text::FAINT
                })))
                .child(day_of_month.to_string()),
        );
    let mut cell = div()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .p(px(2.0))
        .cursor_pointer()
        .when(!in_month, |el| el.opacity(0.4))
        .when(is_selected, |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        });
    // Marker dot next to the number on days that hold planner items — the
    // Electron widget painted a faint bullet for the same thing.
    let mut number = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(2.0))
        .child(disc);
    if !titles.is_empty() {
        number = number.child(
            div()
                .size(px(3.0))
                .rounded_full()
                .bg(rgb(theme::hex(theme::text::FAINT))),
        );
    }
    cell = cell.child(number);
    {
        let widget_id = widget_id.to_owned();
        cell = cell.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.widget_command(
                    &widget_id,
                    json!({ "op": "set", "state": { "selected_day": key } }),
                    cx,
                );
            }),
        );
    }
    for title in titles.iter().take(2) {
        cell = cell.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(title.clone()),
        );
    }
    if titles.len() > 2 {
        cell = cell.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(format!("+{} more", titles.len() - 2)),
        );
    }
    cell
}

pub fn calendar_pane(
    widget: &slate_app::projection::Widget,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let (today_y, today_m, today_d) = today_parts();
    let (y, m) = viewed_month(widget, today_y, today_m);
    let selected_day = widget
        .state
        .as_ref()
        .and_then(|state| state.get("selected_day"))
        .and_then(serde_json::Value::as_str);
    let (by_day, error) = items_by_day();
    let today_key = day_key(today_y, today_m, today_d);
    let wid = widget.id.clone();

    // A corrupted journaled month must not panic the render — rem_euclid
    // folds any i64 into 1..=12 for both the label and the ‹ › targets so
    // one bad `state.month` can't crash the widget on every launch.
    let m1 = m.rem_euclid(12); // 0-based for indexing
    let (prev_y, prev_m) = if m1 == 0 { (y - 1, 12) } else { (y, m1) };
    let (next_y, next_m) = if m1 == 11 { (y + 1, 1) } else { (y, m1 + 2) };

    let mut root = div().flex().flex_col().size_full();
    // Header: ‹ › browse the journaled month cursor, "Today" jumps back to
    // the current month and selects today — the original's prev/next/today
    // controls, persisted instead of localStorage.
    root = root.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_1()
            .pb_1()
            .border_b_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .child(nav_button(
                        &wid,
                        "‹",
                        json!({ "year": prev_y, "month": prev_m }),
                        cx,
                    ))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                            .child(format!("{} {y}", MONTHS[m1 as usize])),
                    )
                    .child(nav_button(
                        &wid,
                        "›",
                        json!({ "year": next_y, "month": next_m }),
                        cx,
                    )),
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
                            .child(format!("{} task(s)", count_in_month(&by_day, y, m))),
                    )
                    .child(nav_button(
                        &wid,
                        "Today",
                        json!({
                            "year": today_y,
                            "month": today_m,
                            "selected_day": today_key,
                        }),
                        cx,
                    )),
            ),
    );
    if let Some(error) = &error {
        root = root.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::status::DANGER)))
                .child(format!("calendar: {error}")),
        );
    }

    // Weekday header.
    let mut header = div()
        .flex()
        .flex_row()
        .border_b_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)));
    for weekday in WEEKDAYS {
        header = header.child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .flex()
                .justify_center()
                .child(weekday),
        );
    }
    root = root.child(header);

    // 6 weeks × 7 days. The grid backs onto the first Monday on or before
    // the 1st — `(z + 3) % 7` because day 0 (1970-01-01) was a Thursday.
    let first_z = days_from_civil(y, m, 1);
    let start_z = first_z - (first_z + 3).rem_euclid(7);
    let mut grid = div()
        .id(format!("slate-calendar-grid-{}", wid))
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    for week in 0..6i64 {
        let mut row = div().flex().flex_row().flex_1().min_h_0();
        for weekday in 0..7i64 {
            let (cy, cm, cd) = civil_from_days(start_z + week * 7 + weekday);
            let key = day_key(cy, cm, cd);
            let titles: &[String] = by_day.get(&key).map(Vec::as_slice).unwrap_or(&[]);
            row = row.child(day_cell(
                &wid,
                key.clone(),
                cd,
                (
                    cm == m,
                    key == today_key,
                    selected_day == Some(key.as_str()),
                ),
                titles,
                cx,
            ));
        }
        grid = grid.child(row);
    }
    root.child(grid)
}

/// How many planner items land anywhere in the shown month — the count the
/// header displays instead of the original's nav controls.
fn count_in_month(by_day: &HashMap<String, Vec<String>>, y: i64, m: i64) -> usize {
    let prefix = format!("{y:04}-{m:02}-");
    by_day
        .iter()
        .filter(|(day, _)| day.starts_with(&prefix))
        .map(|(_, items)| items.len())
        .sum()
}
