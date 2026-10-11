//! Command bar — the summon-only overlay that replaced the Electron
//! Toolbar's always-on strip. It exists only while `CanvasView`'s
//! `command_bar` is open (`/`, `.`, `@` summon it; Escape drops it), and
//! carries just the pieces that need pixels: the input echo, the `↵`
//! submit, the mode/target readout, and the suggestion listbox.
//!
//! From the shell's `Fu` strip this keeps the glass fill, the input's
//! placeholder pair, and `#command-suggestions` — the tool buttons,
//! swatches, overflow menu and zoom group are gone with the strip.

use gpui::prelude::FluentBuilder;
use gpui::*;
use slate_app::theme;

/// What the overlay needs per frame, filled by `CanvasView`.
pub struct CommandBarState {
    /// The `/` input's `value`.
    pub buffer: String,
    /// `g === "message"` — placeholder and target readout swap.
    pub message_mode: bool,
    /// The message target's display name while `message_mode`.
    pub target_name: Option<String>,
    /// `↵`'s disabled gate — non-empty buffer with somewhere to go.
    pub can_submit: bool,
    /// `(kind, label, hint)` rows — `ls` filtered by the buffer's first
    /// token; empty collapses the popover.
    pub suggestions: Vec<(String, String, String)>,
    /// `p` — the highlighted row, clamped on render.
    pub suggestion_sel: usize,
    /// The pill is `min(560px, 100vw - 24px)`.
    pub window_width: f32,
}

/// The shell's glass fill — `rgba(18, 18, 18, 0.80)`.
const GLASS: u32 = 0x121212CC;
/// `bg-bg-panel/95` — the listbox fill (`elevated` @ 95%).
const PANEL_95: u32 = 0x181818F2;
/// Pill height — the old strip's `h-12` trimmed to the input row.
const BAR_H: f32 = 40.0;
/// Listbox rides `bottom-[calc(100%+8px)]`.
const POPOVER_LIFT: f32 = BAR_H + 8.0;

/// A `#command-suggestions` row — `prefix+kind` left, `label · hint`
/// faint right; a click adopts the kind into the input, the shell's
/// `ce(H)`.
fn suggestion_row(
    index: usize,
    selected: bool,
    prefix: char,
    (kind, label, hint): (&str, &str, &str),
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let action = format!("suggest:{kind}");
    div()
        .id(format!("slate-suggest-{index}"))
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .w_full()
        .px(px(10.0))
        .py_2()
        .text_size(px(11.0))
        .text_color(rgb(theme::hex(if selected {
            theme::text::NORMAL
        } else {
            theme::text::DIM
        })))
        .when(selected, |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
        })
        .child(
            div()
                .font_family("Departure Mono")
                .flex_none()
                .text_color(rgb(theme::hex(theme::accent::SAND)))
                .child(format!("{prefix}{kind}")),
        )
        .child(
            div()
                .ml_auto()
                .min_w_0()
                .font_family("Departure Mono")
                .text_size(px(10.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .truncate()
                .child(format!("{label} · {hint}")),
        )
        .cursor_pointer()
        .when(!selected, |el| {
            el.hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.commandbar_command(&action, cx);
            }),
        )
}

/// The overlay itself — centered by the caller (`left: rail_w, right: 0`
/// wrapper), only ever mounted while the bar owns the keyboard.
pub fn command_bar_pane(
    state: &CommandBarState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut bar = div()
        .id("slate-commandbar")
        .w(px((state.window_width - 24.0).clamp(0.0, 560.0)))
        .h(px(BAR_H))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .bg(rgba(GLASS))
        .px_2()
        .py_1()
        .shadow_2xl()
        // A press anywhere on the pill must not reach the canvas's
        // outside-click handler, which would close the bar.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event, _window, cx| {
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|_this, _event, _window, cx| {
                cx.stop_propagation();
            }),
        );

    // #command-suggestions — `absolute bottom-[calc(100%+8px)] left-10`.
    if !state.suggestions.is_empty() {
        let sel = state.suggestion_sel.min(state.suggestions.len() - 1);
        let prefix = state
            .buffer
            .trim_start()
            .chars()
            .next()
            .filter(|c| matches!(c, '/' | '.' | '@'))
            .unwrap_or('/');
        let mut pop = div()
            .id("slate-suggestions")
            .absolute()
            .bottom(px(POPOVER_LIFT))
            .left(px(8.0))
            .min_w(px(250.0))
            .flex()
            .flex_col()
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgba(PANEL_95))
            .p_1()
            .overflow_hidden()
            .shadow_2xl();
        for (index, (kind, label, hint)) in state.suggestions.iter().enumerate() {
            pop = pop.child(suggestion_row(
                index,
                index == sel,
                prefix,
                (kind, label, hint),
                cx,
            ));
        }
        bar = bar.child(pop);
    }

    // Mode readout — the strip's `<select>` pair reduced to a label:
    // `MSG→target` while message mode is engaged, nothing in command mode.
    if state.message_mode {
        bar = bar.child(
            div()
                .flex_none()
                .px_2()
                .font_family("Departure Mono")
                .text_size(px(10.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(match &state.target_name {
                    Some(name) => format!("MSG→{name}"),
                    None => "MSG".to_owned(),
                }),
        );
    }

    // The `<input>` — renders `buffer`, dims to the placeholder when
    // empty, `▌` caret always (the bar only exists while focused).
    let placeholder = if state.message_mode {
        "Write a message…"
    } else {
        "Run a command or /terminal"
    };
    bar = bar.child(
        div()
            .id("slate-command-input")
            .flex_1()
            .min_w(px(120.0))
            .h_full()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .px_1()
            .text_size(px(11.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(theme::hex(if state.buffer.is_empty() {
                        theme::text::FAINT
                    } else {
                        theme::text::NORMAL
                    })))
                    .child(if state.buffer.is_empty() {
                        placeholder.to_owned()
                    } else {
                        state.buffer.clone()
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .child("▌"),
            ),
    );

    // `↵` submit — the shell's `h-7 w-7` button.
    {
        let mut el = div()
            .id("slate-command-submit")
            .flex_none()
            .w(px(28.0))
            .h(px(28.0))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(14.0))
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .child("↵");
        if state.can_submit {
            el = el
                .cursor_pointer()
                .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.commandbar_command("submit", cx);
                    }),
                );
        } else {
            el = el.opacity(0.3);
        }
        bar = bar.child(el);
    }

    bar
}
