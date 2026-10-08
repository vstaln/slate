//! Sys-monitor pane — the read half of the old Electron `SysMonitorWidget`.
//!
//! The original polled a `system:stats` IPC handler (cpuSampler + `os` calls)
//! on a 1–5s interval. There is no such service in the native build; the
//! canvas already re-renders every 250ms, so this pane just samples `/proc`
//! per render instead:
//!
//! * `/proc/stat`   — CPU busy %, as the delta between this render and the
//!   previous one (first render falls back to load1/cores);
//! * `/proc/meminfo`— RAM used/total (`MemTotal` − `MemAvailable`);
//! * `/proc/loadavg`, `/proc/uptime`, `/proc/cpuinfo`,
//!   `/proc/sys/kernel/hostname` — the Host & Runtime card.
//!
//! The agent-usage, command-bus and terminal lists the original also drew
//! were served by main-process subsystems that do not exist here; the pane
//! keeps the CPU/RAM/Host cards and drops the rest rather than faking it.
//! Off-Linux there is no `/proc` and the pane degrades to a notice line.

use gpui::*;
use slate_app::theme;

/// Meter accents the original hardcoded — data colors, not chrome tokens:
/// CPU blue, RAM green, amber past the warning band, theme danger at the top.
const CPU_BLUE: u32 = 0x7aa2f7;
const RAM_GREEN: u32 = 0x7fd99a;
const WARN_AMBER: u32 = 0xe6c07b;

/// (idle_ticks, total_ticks) per widget from the previous render — the only
/// way a sampling pane gets a percentage without sleeping: diff consecutive
/// views. Keyed by widget id so two sys-monitor cards don't pollute each
/// other's delta.
static LAST_CPU: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<std::string::String, (u64, u64)>>,
> = std::sync::LazyLock::new(|| {
    std::sync::Mutex::new(std::collections::HashMap::new())
});

#[derive(Default)]
pub struct Snapshot {
    /// Busy %, already 0..=100. `None` before the first successful delta and
    /// whenever neither `/proc/stat` nor `/proc/loadavg` can be read.
    pub cpu_percent: Option<f32>,
    pub load: Option<[f64; 3]>,
    pub mem_total_kb: Option<u64>,
    pub mem_used_kb: Option<u64>,
    pub uptime_s: Option<u64>,
    pub cpu_model: Option<String>,
    pub cpu_count: Option<usize>,
    pub hostname: Option<String>,
}

/// First line of `/proc/stat`: `cpu  user nice system idle iowait irq
/// softirq steal guest guest_nice` — all in USER_HZ ticks. `idle` folds in
/// `iowait`, and `total` is everything (guest is already inside user/nice on
/// Linux, but summing the columns is what every reader does — the kernel's
/// own tools included — so the error stays cosmetically irrelevant).
fn read_cpu_ticks() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let ticks: Vec<u64> = fields.filter_map(|f| f.parse().ok()).collect();
    if ticks.len() < 5 {
        return None;
    }
    let idle = ticks[3] + ticks[4];
    let total = ticks.iter().sum();
    Some((idle, total))
}

fn sample_cpu_percent(
    key: &str,
    load: Option<[f64; 3]>,
    cores: usize,
) -> Option<f32> {
    let now = read_cpu_ticks();
    let mut map = LAST_CPU.lock().ok()?;
    let last = map.get(key).copied();
    let percent = match (now, last) {
        (Some((idle, total)), Some((prev_idle, prev_total))) => {
            let busy =
                (total.saturating_sub(prev_total)).saturating_sub(idle.saturating_sub(prev_idle));
            let span = total.saturating_sub(prev_total);
            if span > 0 {
                Some(busy as f32 / span as f32 * 100.0)
            } else {
                None
            }
        }
        // No delta yet on the first paint: 1-minute load over core count is
        // a decent stand-in so the card is not blank for one refresh.
        (Some(_), None) => {
            load.map(|l| (l[0] as f32 / cores.max(1) as f32 * 100.0).clamp(0.0, 100.0))
        }
        (None, _) => None,
    };
    match now {
        Some(ticks) => map.insert(key.to_owned(), ticks),
        None => map.remove(key),
    };
    percent
}

fn read_meminfo() -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return (None, None);
    };
    let kb = |name: &str| -> Option<u64> {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
    };
    let total = kb("MemTotal:");
    let available = kb("MemAvailable:");
    (
        total,
        total.zip(available).map(|(t, a)| t.saturating_sub(a)),
    )
}

fn read_load() -> Option<[f64; 3]> {
    let text = std::fs::read_to_string("/proc/loadavg").ok()?;
    let mut fields = text.split_whitespace();
    Some([
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ])
}

fn read_uptime() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .map(|s| s as u64)
}

fn read_cpuinfo() -> (Option<String>, Option<usize>) {
    let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") else {
        return (None, None);
    };
    let model = text
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .map(str::trim)
        .map(str::to_owned);
    let count = text
        .lines()
        .filter(|line| line.starts_with("processor"))
        .count();
    (model, (count > 0).then_some(count))
}

fn read_hostname() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// One render's worth of `/proc` — every field optional so a non-Linux host
/// (or a locked-down proc) degrades to `—` rows rather than an empty pane.
/// `key` scopes the CPU delta to one widget (two sysmon widgets render per
/// tick and would otherwise halve each other's sample window).
pub fn snapshot(key: &str) -> Snapshot {
    let (cpu_model, cpu_count) = read_cpuinfo();
    let load = read_load();
    let cores = cpu_count.unwrap_or(1);
    let (mem_total_kb, mem_used_kb) = read_meminfo();
    Snapshot {
        cpu_percent: sample_cpu_percent(key, load, cores),
        load,
        mem_total_kb,
        mem_used_kb,
        uptime_s: read_uptime(),
        cpu_model,
        cpu_count,
        hostname: read_hostname(),
    }
}

/// `formatBytes` from the widget: GB once past the 1GB mark, MB below.
fn format_bytes(kb: u64) -> String {
    let bytes = kb as f64 * 1024.0;
    let gb = bytes / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{gb:.2} GB")
    } else {
        format!("{:.0} MB", bytes / (1024.0 * 1024.0))
    }
}

/// `formatUptime` from the widget: biggest non-zero unit down to minutes,
/// seconds only under a minute.
fn format_uptime(seconds: u64) -> String {
    let (d, h, m, s) = (
        seconds / 86_400,
        (seconds % 86_400) / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
    );
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// One labeled row of the Host card (`label: value`, value right-aligned).
fn stat_row(label: &str, value: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .gap_2()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(label.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .truncate()
                .min_w_0()
                .child(value),
        )
}

/// A section card: hairline ring, elevated fill, label row, content.
fn card(children: Vec<AnyElement>) -> Div {
    let mut card = div()
        .flex()
        .flex_col()
        .gap_1()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        .p_2();
    for child in children {
        card = card.child(child);
    }
    card
}

fn card_header(label: &str, value: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(label.to_owned()),
        )
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(value),
        )
}

/// The pill bar both meter cards shared (`bg-line-soft` track, accent fill).
fn meter(fraction: f32, fill: u32) -> impl IntoElement {
    div()
        .h(px(6.0))
        .w_full()
        .overflow_hidden()
        .bg(rgb(theme::hex(theme::hairline::FAINT)))
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0.02, 1.0)))
                .bg(rgb(fill)),
        )
}

/// Band colors: the widget's `>80`/`>50` CPU and `>85`/`>65` RAM thresholds.
fn meter_color(percent: f32, base: u32, warn_at: f32, danger_at: f32) -> u32 {
    if percent > danger_at {
        theme::hex(theme::status::DANGER)
    } else if percent > warn_at {
        WARN_AMBER
    } else {
        base
    }
}

pub fn sysmon_pane(
    widget_id: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    // Read-only pane: /proc sampling happens per render tick; the context is
    // accepted for signature uniformity but unused.
    let _ = cx;
    let snap = snapshot(widget_id);
    let mut col = div().flex().flex_col().gap_2().p_2();

    if snap.cpu_percent.is_none()
        && snap.mem_total_kb.is_none()
        && snap.load.is_none()
        && snap.uptime_s.is_none()
    {
        col = col.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child("No system stats — /proc is not readable here"),
        );
        return div()
            .id("slate-sysmon-body")
            .size_full()
            .overflow_y_scroll()
            .child(col);
    }

    // CPU Load card.
    {
        let percent = snap.cpu_percent.unwrap_or(0.0);
        let mut children: Vec<AnyElement> = vec![
            card_header("CPU Load", format!("{percent:.0}%")).into_any_element(),
            meter(percent / 100.0, meter_color(percent, CPU_BLUE, 50.0, 80.0)).into_any_element(),
        ];
        let mut sub = match (snap.cpu_count, &snap.cpu_model) {
            (Some(count), Some(model)) => format!("{count} cores · {model}"),
            (Some(count), None) => format!("{count} cores"),
            _ => String::new(),
        };
        if let Some(load) = snap.load {
            if !sub.is_empty() {
                sub.push_str(" · ");
            }
            sub.push_str(&format!(
                "load {:.2} {:.2} {:.2}",
                load[0], load[1], load[2]
            ));
        }
        if sub.is_empty() {
            sub.push_str("sampling…");
        }
        children.push(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(sub)
                .into_any_element(),
        );
        col = col.child(card(children));
    }

    // RAM Usage card.
    if let (Some(total), Some(used)) = (snap.mem_total_kb, snap.mem_used_kb) {
        let percent = if total > 0 {
            used as f32 / total as f32 * 100.0
        } else {
            0.0
        };
        col = col.child(card(vec![
            card_header("RAM Usage", format!("{percent:.0}%")).into_any_element(),
            meter(percent / 100.0, meter_color(percent, RAM_GREEN, 65.0, 85.0)).into_any_element(),
            stat_row(
                "Used / Total",
                format!("{} / {}", format_bytes(used), format_bytes(total)),
            )
            .into_any_element(),
        ]));
    }

    // Host & Runtime card.
    {
        let mut children: Vec<AnyElement> = Vec::new();
        children.push(
            stat_row(
                "Platform",
                format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH),
            )
            .into_any_element(),
        );
        children.push(
            stat_row(
                "Hostname",
                snap.hostname.clone().unwrap_or_else(|| "—".to_owned()),
            )
            .into_any_element(),
        );
        children.push(
            stat_row(
                "Uptime",
                snap.uptime_s
                    .map(format_uptime)
                    .unwrap_or_else(|| "—".to_owned()),
            )
            .into_any_element(),
        );
        if let Some(load) = snap.load {
            children.push(
                stat_row(
                    "Load avg",
                    format!("{:.2} {:.2} {:.2}", load[0], load[1], load[2]),
                )
                .into_any_element(),
            );
        }
        if !children.is_empty() {
            let mut section = vec![div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child("Host & Runtime")
                .into_any_element()];
            section.extend(children);
            col = col.child(card(section));
        }
    }

    // The whole stack of cards is the scroll region — the pane keeps no
    // pinned toolbar of its own.
    div()
        .id("slate-sysmon-body")
        .size_full()
        .overflow_y_scroll()
        .child(col)
}
