//! Timer pane — the read-mostly half of the old Electron `TimerWidget`.
//!
//! The original kept per-widget countdown state in localStorage under
//! `orcspace-timer:<widgetId>` — `{totalMs, remaining, running, deadline,
//! rang, isCustom, ...}` — where `deadline` is a `Date.now()` epoch-ms
//! timestamp and `remaining` only means something while paused. The native
//! mirror of that store is `workspace-timers.json` in the user data dir:
//! `{ "<widgetId>": {totalMs, remaining, running, deadline, rang} }`.
//!
//! The pane's buttons write that file directly — Start/Pause flips
//! `running` and recomputes `deadline`/`remaining` against `Date.now()`,
//! Reset returns to `totalMs` — the same fields the Electron widget's
//! `timerPersist` patched. While a state is absent the face shows the idle
//! 25:00 the original booted into, and Start creates the entry. The canvas
//! re-renders on a 250ms tick, which is what makes the countdown move.

use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

/// The duration the Electron widget armed itself with on first paint.
const DEFAULT_TOTAL_MS: i64 = 25 * 60_000;

/// The persisted shape, trimmed to what a read-only face needs.
#[derive(Debug, Clone, Copy)]
pub struct TimerState {
    pub total_ms: f64,
    /// Paused countdown residue; meaningless while `running`.
    pub remaining_ms: f64,
    pub running: bool,
    /// Epoch ms (`Date.now()`) at which a running timer reaches zero.
    pub deadline_ms: f64,
    /// The original's "already notified" latch — once true, a finished timer
    /// stays announced rather than re-firing.
    pub rang: bool,
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

fn timers_path() -> std::path::PathBuf {
    slate_app::ipc::user_data_dir().join("workspace-timers.json")
}

/// Serializes the read-modify-write below: the file holds one map for every
/// timer widget, so an unlocked RMW can drop a sibling's concurrent update.
static TIMERS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read-modify-write this widget's timer entry, preserving every other
/// widget's state and any fields the pane does not model (the original's
/// `isCustom`, `label`, …). Temp-file rename keeps a concurrent reader from
/// meeting a truncated file.
fn mutate_entry(widget_id: &str, mutate: impl FnOnce(&mut serde_json::Map<String, Value>, f64)) {
    let _guard = TIMERS_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let path = timers_path();
    let mut document = slate_app::ipc::read_store_recovered(&path)
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let mut entry = document
        .get(widget_id)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    mutate(&mut entry, now_ms());
    document[widget_id] = Value::Object(entry);
    let bytes = slate_app::jsjson::to_js_json_pretty(&document, 2).into_bytes();
    let _ = slate_app::ipc::write_file_atomic(&path, &bytes);
}

/// The Electron widget's start/pause toggle: pausing banks `deadline − now`
/// into `remaining`; starting arms a fresh `deadline` — a finished or idle
/// timer restarts from `totalMs` rather than resuming a negative remainder.
fn toggle_running(widget_id: &str) {
    mutate_entry(widget_id, |entry, now| {
        let total = entry
            .get("totalMs")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TOTAL_MS as f64);
        let running = entry
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        entry.insert("totalMs".into(), json!(total));
        if running {
            let deadline = entry.get("deadline").and_then(Value::as_f64).unwrap_or(now);
            entry.insert("remaining".into(), json!(deadline - now));
            entry.insert("running".into(), json!(false));
        } else {
            let remaining = entry
                .get("remaining")
                .and_then(Value::as_f64)
                .unwrap_or(total);
            let remaining = if remaining <= 0.0 { total } else { remaining };
            entry.insert("remaining".into(), json!(remaining));
            entry.insert("deadline".into(), json!(now + remaining));
            entry.insert("running".into(), json!(true));
            entry.insert("rang".into(), json!(false));
        }
    });
}

/// The original's reset: stopped, full `remaining`, no deadline, `rang`
/// cleared so the next finish notifies again.
fn reset_timer(widget_id: &str) {
    mutate_entry(widget_id, |entry, _now| {
        let total = entry
            .get("totalMs")
            .and_then(Value::as_f64)
            .unwrap_or(DEFAULT_TOTAL_MS as f64);
        entry.insert("totalMs".into(), json!(total));
        entry.insert("remaining".into(), json!(total));
        entry.insert("deadline".into(), json!(0.0));
        entry.insert("running".into(), json!(false));
        entry.insert("rang".into(), json!(false));
    });
}

/// One bordered pill button — the Start/Pause/Reset controls share it.
fn timer_button(
    label: &'static str,
    listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .px_2()
        .py(px(2.0))
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .cursor_pointer()
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(label),
        )
        .on_mouse_down(MouseButton::Left, listener)
}

/// `<id>` from `workspace-timers.json`. Every field is optional and lenient —
/// the original's `timerPersist.get` patched partial saves the same way.
pub fn timer_state(widget_id: &str) -> Option<TimerState> {
    let path = timers_path();
    let bytes = std::fs::read(&path).ok()?;
    let document: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let entry = document.get(widget_id)?;
    let number = |key: &str| entry.get(key).and_then(serde_json::Value::as_f64);
    Some(TimerState {
        total_ms: number("totalMs").unwrap_or(DEFAULT_TOTAL_MS as f64),
        remaining_ms: number("remaining").unwrap_or(DEFAULT_TOTAL_MS as f64),
        running: entry
            .get("running")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        deadline_ms: number("deadline").unwrap_or(0.0),
        rang: entry
            .get("rang")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

/// The original's `format(ms, totalMs)`: `MM:SS`, or `H:MM:SS` once an hour
/// is on the clock — including when a >1h total was configured but has since
/// counted below it.
fn format_ms(ms: i64, total_ms: i64) -> String {
    let total = (ms / 1000).max(0);
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 || total_ms >= 3_600_000 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// A thin pill bar — the same shape the sysmon and timer widgets shared.
fn progress_bar(fraction: f32, color: u32) -> impl IntoElement {
    div()
        .h(px(6.0))
        .w_full()
        .overflow_hidden()
        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0.0, 1.0)))
                .bg(rgb(color)),
        )
}

pub fn timer_pane(
    widget_id: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let now = now_ms();
    let state = timer_state(widget_id);

    // `remaining` is derived from the deadline while running — exactly what
    // timerPersist.get recomputed on load — so a relaunched timer never
    // resumes stale.
    let (shown_ms, total_ms, running, over, rang) = match state {
        Some(state) => {
            let remaining = if state.running {
                state.deadline_ms - now
            } else {
                state.remaining_ms
            };
            (
                remaining,
                state.total_ms,
                state.running,
                remaining <= 0.0,
                state.rang,
            )
        }
        None => (
            DEFAULT_TOTAL_MS as f64,
            DEFAULT_TOTAL_MS as f64,
            false,
            false,
            false,
        ),
    };
    // `over` is just `remaining <= 0` in the original — a real state at zero
    // counts, the idle fallback never does.
    let over = state.is_some() && over;

    let progress = if total_ms > 0.0 {
        (shown_ms / total_ms).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    // Overtime counts upward past zero, like the original's `+MM:SS`.
    let face = if over {
        format!("+{}", format_ms(-shown_ms as i64, total_ms as i64))
    } else {
        format_ms(shown_ms.max(0.0) as i64, total_ms as i64)
    };

    let mut col = div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .w_full()
        .min_h_full()
        .p_2();
    col = col.child(
        div()
            .font_family("Departure Mono")
            .text_size(px(28.0))
            .text_color(rgb(theme::hex(if over {
                theme::status::DANGER
            } else {
                theme::text::NORMAL
            })))
            .child(face),
    );
    col = col.child(progress_bar(
        progress,
        if over {
            theme::hex(theme::status::DANGER)
        } else {
            // colorAccent in this palette is white — the fill the original
            // painted as bg-accent/70.
            theme::hex(theme::text::NORMAL)
        },
    ));
    let status = match state {
        None => "Idle — no timer state yet".to_owned(),
        Some(_) if running && over => "Finished".to_owned(),
        Some(_) if running => format!("Running · {} total", format_ms(total_ms as i64, 0)),
        Some(_) if rang || over => "Finished".to_owned(),
        Some(_) => format!("Paused · {} total", format_ms(total_ms as i64, 0)),
    };
    col = col.child(
        div()
            .text_xs()
            .text_color(rgb(theme::hex(theme::text::FAINT)))
            .child(status),
    );

    // Start/Pause + Reset — the Electron widget's footer controls, writing
    // `workspace-timers.json` so the state survives a restart.
    let wid = widget_id.to_owned();
    let wid_reset = widget_id.to_owned();
    col = col.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(timer_button(
                if running { "Pause" } else { "Start" },
                cx.listener(move |_this, _event, _window, cx| {
                    toggle_running(&wid);
                    cx.notify();
                }),
            ))
            .child(timer_button(
                "Reset",
                cx.listener(move |_this, _event, _window, cx| {
                    reset_timer(&wid_reset);
                    cx.notify();
                }),
            )),
    );
    // The wrapper owns the scroll; `min_h_full` on the column keeps the
    // face centered when it fits and lets an overflowing pane reach the
    // top instead of clipping it dead behind `justify_center`.
    div()
        .id(format!("slate-timer-body-{}", widget_id))
        .size_full()
        .overflow_y_scroll()
        .child(col)
}
