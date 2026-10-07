//! Bottom toolbar — the native port of the Electron canvas `Toolbar`,
//! bundle function `Fu` (`renderer/assets/index-*.js`, grep
//! `"Canvas Tools"` / `command-suggestions` / `fixed bottom-6`).
//!
//! The original is a 48px `fixed bottom-6 left-1/2 -translate-x-1/2` glass
//! strip (`rgba(18,18,18,.80)` + `blur(20px)`, `w-[min(780px,100vw-24px)]`,
//! `rounded-panel border-line px-2 py-1 gap-1`) holding, left to right:
//! the `Kn` icon buttons (Draw inside the input `<form>`, then Select /
//! Eraser / Pan), the command `<input>` with a `↵` submit, the widget-alias
//! listbox (`#command-suggestions`), a floating 8-swatch color group that
//! pops above the bar while `tool === "draw"`, and a "⋯" overflow menu
//! carrying the workspace-dir button, Settings, the Mode + Terminal
//! `<select>`s, the zoom group, and "Clear all drawings".
//!
//! The port keeps the shell's exact bar order — Draw, the command input,
//! the `↵` submit, Select / Eraser / Pan, then the `⋯` button that pops
//! the overflow menu — and parks everything the menu carried inside it:
//! workspace pick + Settings, the Mode + Terminal selects, the zoom
//! group and "Clear all drawings". Undo/redo stayed on the TitleBar.
//!
//! Deviations and why, flagged for the coordinator:
//!
//! - **Selects are click-to-cycle chips.** gpui has no `<select>`, so
//!   Mode toggles `command`↔`message` on click and the terminal chip
//!   cycles the list (left = next, right = previous). Same verbs.
//! - **No backdrop blur / scale transitions.** gpui has no
//!   `backdrop-filter`; the strip keeps the 80% surface fill, and a
//!   selected swatch is a white border instead of `scale-110`.
//! - **Swatch palette only while `tool == "draw"`** — the original's
//!   hover-to-keep-open half has no counterpart state.
//!
//! Icons are the vendored Lucide SVGs (`crate::assets::icon`): Pencil
//! draw · MousePointer2 select · Eraser erase · Hand pan · MoreHorizontal
//! more · FolderOpen workspace · Settings · Expand fit · Trash2 clear.
//!
//! Emitted through `CanvasView::toolbar_command(action, json!({}), cx)`:
//! `tool:select` `tool:draw` `tool:erase` `tool:pan`, `stroke:<0-7>`,
//! `submit`, `overflow`, `clear_strokes`, `workspace:pick`,
//! `mode:command` `mode:message`, `target:<id>`, `focus_input`,
//! `suggest:<kind>`, `fit`, `zoom_out`, `zoom_reset`, `zoom_in`,
//! `settings`.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::json;
use slate_app::theme;

/// Everything the strip renders, filled by `CanvasView` each frame.
/// The original props were `tool`, `hasStrokes`, `strokeColor`,
/// `workspaceDir`, `terminals`, `targetTerminalId`, `commandPrefix`,
/// `zoom` plus a dozen callbacks; the callbacks became the
/// `toolbar_command` verbs above, and the props became this struct.
pub struct ToolbarState {
    /// The `<input>`'s `value` — text routed by the caller while `open`.
    pub buffer: String,
    /// The input owns the keyboard (the original's DOM focus on `A`).
    /// Draws the `▌` caret; an empty buffer still shows the placeholder.
    pub open: bool,
    /// `g === "message"` — text submits to the target terminal instead of
    /// being parsed as a `/ . @` widget invocation.
    pub message_mode: bool,
    /// The resolved target terminal's display name (`ie`'s title), or
    /// `None` when there is nothing to address — renders "No terminal".
    pub target_name: Option<String>,
    /// `(id, title)` per terminal — the target select's `<option>`s.
    pub terminals: Vec<(String, String)>,
    /// `"select"` | `"draw"` | `"erase"` — one active `Kn` at a time.
    pub tool: String,
    /// Index into [`STROKE_SWATCHES`] — the picked stroke color.
    pub stroke_color: usize,
    /// `canSubmit` — the `↵` button's disabled gate: non-empty buffer
    /// and either a live target terminal or (command mode) a parseable
    /// widget invocation.
    pub can_submit: bool,
    /// The `⋯` overflow menu's open flag.
    pub overflow_open: bool,
    /// `hasStrokes` — gates "Clear all drawings".
    pub has_strokes: bool,
    /// `Math.round(zoom * 100)` — the reset button's label.
    pub zoom_percent: u32,
    /// `workspaceDir` basename, already falling back to `"No folder"`.
    pub workspace_name: String,
    /// `(kind, label, hint)` rows — `ls` filtered by the buffer's first
    /// token (`W`). Empty vec collapses the popover entirely.
    pub suggestions: Vec<(String, String, String)>,
    /// `p` — the highlighted suggestion; clamped to `len - 1` on render,
    /// matching the original's `Math.min(p, W.length - 1)`.
    pub suggestion_sel: usize,
    /// Window width — the strip is `w-[min(780px,calc(100vw-24px))]`, a
    /// fixed width, not content-sized.
    pub window_width: f32,
    /// Master gate — `false` dims the strip and drops every handler.
    pub enabled: bool,
}

/// The shell's `bo`/`pa` pair — the swatch row, in order, with the
/// tooltip names (`title={pa[H]}`): White Red Orange Yellow Green Blue
/// Violet Pink. `canvas_view::STROKE_COLORS` holds the same hexes for
/// the strokes themselves; this duplicate carries the labels.
const STROKE_SWATCHES: [(&str, &str); 8] = [
    ("#ffffff", "White"),
    ("#ff6b6b", "Red"),
    ("#ffa94d", "Orange"),
    ("#ffd43b", "Yellow"),
    ("#69db7c", "Green"),
    ("#4dabf7", "Blue"),
    ("#b197fc", "Violet"),
    ("#f783ac", "Pink"),
];

/// `h-12` — the strip's fixed height.
const STRIP_H: f32 = 48.0;
/// `Kn`: `h-8 w-8` icon buttons.
const BTN: f32 = 32.0;
/// Swatch `h-5 w-5` and the zoom group's `h-7 w-7` buttons.
const SWATCH: f32 = 20.0;
const ZOOM_BTN: f32 = 28.0;
/// Popovers ride `bottom-[calc(100%+8px)]` — 48 + 8.
const POPOVER_LIFT: f32 = STRIP_H + 8.0;

/// The strip's glass fill — the original's inline
/// `background: rgba(18, 18, 18, 0.80)` (token `surface` @ 80%).
const GLASS: u32 = 0x121212CC;
/// `bg-bg-panel/95` — the suggestion listbox fill (`elevated` @ 95%).
const PANEL_95: u32 = 0x181818F2;

/// `"#rrggbb"` → the `0xRRGGBBAA` `rgba()` wants (opaque).
fn swatch_rgba(hex: &str) -> u32 {
    u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0xffffff) << 8 | 0xff
}

/// `Kn` — the toolbar's icon button: `grid h-8 w-8 place-items-center
/// rounded-panel`, active or hovered rides `bg-bg-hover`, disabled drops
/// to `opacity-35` and stops taking presses.
fn tool_button(
    id: &'static str,
    icon_name: &'static str,
    icon_size: f32,
    action: &'static str,
    active: bool,
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut el = div()
        .id(id)
        .flex_none()
        .w(px(BTN))
        .h(px(BTN))
        .flex()
        .items_center()
        .justify_center()
        .text_color(rgb(theme::hex(theme::text::NORMAL)));
    if enabled {
        el = el
            .cursor_pointer()
            .when(active, |el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
            })
            .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.toolbar_command(action, json!({}), cx);
                }),
            );
    } else {
        el = el.opacity(0.35);
    }
    el.child(crate::assets::icon(
        icon_name,
        icon_size,
        theme::hex(theme::text::NORMAL),
    ))
}

/// A row in `#command-suggestions`: `prefix+kind` in mono on the left,
/// `label · hint` faint on the right — click emits `suggest:<kind>`,
/// the original's `ce(H)` (fill the input with `${prefix}${kind} `).
fn suggestion_row(
    index: usize,
    selected: bool,
    prefix: char,
    (kind, label, hint): (&str, &str, &str),
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let action = format!("suggest:{kind}");
    let mut row = div()
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
                // text-accent — the one hue, graysite's sand.
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
        );
    if enabled {
        row = row
            .cursor_pointer()
            .when(!selected, |el| {
                el.hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.toolbar_command(&action, json!({}), cx);
                }),
            );
    }
    row
}

/// One swatch (`h-5 w-5 rounded-pill border` in the shell, square here) —
/// the picked color gets the sand accent border the original drew at
/// `scale-110`.
/// Click emits `stroke:<n>`; the original also forced `tool = "draw"`,
/// moot here because the row only exists while draw is active.
fn swatch(
    index: usize,
    hex: &str,
    selected: bool,
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let action = format!("stroke:{index}");
    let mut el = div()
        .id(format!("slate-swatch-{index}"))
        .flex_none()
        .w(px(SWATCH))
        .h(px(SWATCH))
        .border_1()
        .border_color(rgb(theme::hex(if selected {
            theme::accent::SAND // border-accent
        } else {
            theme::hairline::FAINT // border-line
        })))
        .bg(rgba(swatch_rgba(hex)));
    if enabled {
        el = el.cursor_pointer().on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.toolbar_command(&action, json!({}), cx);
            }),
        );
    }
    el
}

/// The zoom group's `h-7` buttons (`−`, `%`, `+`, fit) — smaller than
/// `Kn`, same hover fill. `icon` carries the Lucide glyph (fit); text
/// buttons leave it `None`.
fn zoom_button(
    id: &'static str,
    label: String,
    icon: Option<(&'static str, f32)>,
    min_w: f32,
    action: &'static str,
    mono: bool,
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut el = div()
        .id(id)
        .flex_none()
        .h(px(ZOOM_BTN))
        .min_w(px(min_w))
        .px_1()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(if mono { 10.0 } else { 12.0 }))
        .when(mono, |el| el.font_family("Departure Mono"))
        // `font-semibold` on −/+, `font-medium` on the % label.
        .font_weight(if mono {
            gpui::FontWeight::MEDIUM
        } else {
            gpui::FontWeight::SEMIBOLD
        })
        .text_color(rgb(theme::hex(theme::text::DIM)));
    el = if let Some((name, size)) = icon {
        el.child(crate::assets::icon(name, size, theme::hex(theme::text::DIM)))
    } else {
        el.child(label)
    };
    if enabled {
        el = el
            .cursor_pointer()
            .hover(|el| {
                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.toolbar_command(action, json!({}), cx);
                }),
            );
    } else {
        el = el.opacity(0.35);
    }
    el
}

/// The strip itself — `Canvas Tools`. The caller owns the `fixed
/// bottom-6 left-1/2 -translate-x-1/2` part; this returns the bar at the
/// original's fixed `w-[min(780px,calc(100vw-24px))]`.
pub fn toolbar_pane(
    state: &crate::views_toolbar::ToolbarState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let enabled = state.enabled;

    let mut strip = div()
        .id("slate-toolbar")
        .w(px((state.window_width - 24.0).clamp(0.0, 780.0)))
        .h(px(STRIP_H))
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
        .when(!enabled, |el| el.opacity(0.5))
        // `data-canvas-interactive`: presses on the strip never reach the
        // canvas pan/select handlers beneath it.
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event, _window, cx| {
                cx.stop_propagation();
            }),
        );

    // ── #command-suggestions — `role="listbox"` above the bar ────────────
    // The original anchors `absolute bottom-[calc(100%+8px)] left-10`.
    if !state.suggestions.is_empty() {
        let sel = state.suggestion_sel.min(state.suggestions.len() - 1);
        // The prefix glyph the user typed (`/`, `.`, `@`) — `b[0]` —
        // else the commandPrefix default, which the contract leaves as
        // "/".
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
            .left(px(40.0))
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
                enabled,
                cx,
            ));
        }
        strip = strip.child(pop);
    }

    // ── Stroke palette — the floating `Stroke Color` group ────────────────
    // Original: `absolute bottom-[calc(100%+8px)] left-1/2 -translate-x-1/2`
    // with swatches, visible while draw is active or the pencil/palette is
    // hovered. gpui can't translate, so a full-width row centers the panel.
    if state.tool == "draw" {
        let mut swatches = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgba(0x121212E5)) // bg-bg-raise/90
            .px(px(10.0))
            .py_2();
        for (index, (hex, _name)) in STROKE_SWATCHES.iter().enumerate() {
            swatches = swatches.child(swatch(index, hex, index == state.stroke_color, enabled, cx));
        }
        strip = strip.child(
            div()
                .absolute()
                .bottom(px(POPOVER_LIFT))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(swatches),
        );
    }

    // ── Overflow menu — `toolbar-overflow-menu`, right-anchored ──────────
    // Original: `absolute right-0 bottom-[calc(100%+8px)]` on the ⋯
    // wrapper, `w-[min(280px)] flex-col gap-2 p-2.5`. The wrapper sits at
    // the strip's content-box right edge (px-2), so `right: 8px` here.
    if state.overflow_open {
        let menu_label = |text: &'static str| {
            div()
                .px_1()
                .font_family("Departure Mono")
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(text)
        };
        let menu_row = |id: &'static str,
                        icon_name: &'static str,
                        value: String,
                        action: &'static str,
                        enabled: bool,
                        cx: &mut Context<crate::canvas_view::CanvasView>| {
            let mut el = div()
                .id(id)
                .flex_1()
                .h(px(BTN))
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .font_family("Departure Mono")
                .text_size(px(11.0))
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(
                    div()
                        .flex_none()
                        .child(crate::assets::icon(
                            icon_name,
                            14.0,
                            theme::hex(theme::text::FAINT),
                        )),
                )
                .child(div().min_w_0().truncate().child(value));
            if enabled {
                el = el
                    .cursor_pointer()
                    .hover(|el| {
                        el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.toolbar_command(action, json!({}), cx);
                        }),
                    );
            } else {
                el = el.opacity(0.35);
            }
            el
        };

        let mut menu = div()
            .id("slate-overflow-menu")
            .absolute()
            .bottom(px(POPOVER_LIFT))
            .right(px(8.0))
            .w(px(280.0))
            .flex()
            .flex_col()
            .gap_2()
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgba(PANEL_95))
            .p(px(10.0))
            .shadow_2xl()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event, _window, cx| {
                    cx.stop_propagation();
                }),
            )
            .child(menu_label("WORKSPACE"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(6.0))
                    .child(menu_row(
                        "slate-menu-workspace",
                        "folder-open",
                        if state.workspace_name.is_empty() {
                            "No folder".to_owned()
                        } else {
                            state.workspace_name.clone()
                        },
                        "workspace:pick",
                        enabled,
                        cx,
                    ))
                    .child(menu_row(
                        "slate-menu-settings",
                        "settings",
                        "SETTINGS".to_owned(),
                        "settings",
                        enabled,
                        cx,
                    )),
            )
            .child(
                div()
                    .h(px(1.0))
                    .bg(rgb(theme::hex(theme::hairline::FAINT))),
            )
            .child(menu_label("COMMAND TARGET"));

        // ── Mode + Terminal blocks — `label + <select>` pairs ────────────
        // gpui has no <select>: Mode toggles on click, Terminal cycles
        // (left = next, right = previous). Same verbs as the original.
        let mode_action = if state.message_mode {
            "mode:command"
        } else {
            "mode:message"
        };
        let select_block = |id: &'static str, label: &'static str, value: String| {
            div()
                .id(id)
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .bg(rgb(theme::hex(theme::monochrome::SURFACE))) // bg-bg-raise
                .px_2()
                .py(px(6.0))
                .child(
                    div()
                        .font_family("Departure Mono")
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(label),
                )
                .child(
                    div()
                        .h(px(24.0))
                        .min_w_0()
                        .truncate()
                        .font_family("Departure Mono")
                        .text_size(px(11.0))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(value),
                )
        };
        let mut mode_block = select_block(
            "slate-menu-mode",
            "MODE",
            if state.message_mode {
                "MESSAGE".to_owned()
            } else {
                "COMMAND".to_owned()
            },
        );
        if enabled {
            mode_block = mode_block
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command(mode_action, json!({}), cx);
                    }),
                );
        }

        // The current target's slot — `target_name` is a title, an id
        // match is accepted too. No match lands on index 0, the
        // original's `d[0]?.id` fallback.
        let at = state
            .terminals
            .iter()
            .position(|(id, t)| {
                state.target_name.as_deref() == Some(t.as_str())
                    || state.target_name.as_deref() == Some(id.as_str())
            })
            .unwrap_or(0);
        let len = state.terminals.len();
        let next = (len > 0).then(|| format!("target:{}", state.terminals[(at + 1) % len].0));
        let prev = (len > 1).then(|| format!("target:{}", state.terminals[(at + len - 1) % len].0));
        let title = if len == 0 {
            "No terminal".to_owned()
        } else {
            state
                .target_name
                .clone()
                .unwrap_or_else(|| state.terminals[at].1.clone())
        };
        let mut terminal_block = select_block("slate-menu-terminal", "TERMINAL", title);
        if let (true, Some(next)) = (enabled, next) {
            terminal_block = terminal_block
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command(&next, json!({}), cx);
                    }),
                );
            if let Some(prev) = prev {
                terminal_block = terminal_block.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command(&prev, json!({}), cx);
                    }),
                );
            }
        } else {
            terminal_block = terminal_block.opacity(0.5);
        }

        menu = menu.child(
            div()
                .flex()
                .flex_row()
                .gap(px(6.0))
                .child(mode_block)
                .child(terminal_block),
        );

        // ── Zoom group — `Canvas zoom` + fit/−/%/+ ───────────────────────
        menu = menu.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                .px_2()
                .py_1()
                .child(
                    div()
                        .font_family("Departure Mono")
                        .text_size(px(10.0))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child("CANVAS ZOOM"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .child(zoom_button(
                            "slate-zoom-fit",
                            String::new(),
                            Some(("expand", 14.0)),
                            ZOOM_BTN,
                            "fit",
                            false,
                            enabled,
                            cx,
                        ))
                        .child(zoom_button(
                            "slate-zoom-out",
                            "−".to_owned(),
                            None,
                            ZOOM_BTN,
                            "zoom_out",
                            false,
                            enabled,
                            cx,
                        ))
                        .child(zoom_button(
                            "slate-zoom-reset",
                            format!("{}%", state.zoom_percent),
                            None,
                            42.0,
                            "zoom_reset",
                            true,
                            enabled,
                            cx,
                        ))
                        .child(zoom_button(
                            "slate-zoom-in",
                            "+".to_owned(),
                            None,
                            ZOOM_BTN,
                            "zoom_in",
                            false,
                            enabled,
                            cx,
                        )),
                ),
        );

        // ── "Clear all drawings" — `tool-clear-strokes` ──────────────────
        let mut clear_row = div()
            .id("slate-clear-strokes")
            .h(px(BTN))
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .font_family("Departure Mono")
            .text_size(px(11.0))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .child(
                div()
                    .flex_none()
                    .child(crate::assets::icon(
                        "trash-2",
                        14.0,
                        theme::hex(theme::text::FAINT),
                    )),
            )
            .child("CLEAR ALL DRAWINGS");
        if enabled && state.has_strokes {
            clear_row = clear_row
                .cursor_pointer()
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command("clear_strokes", json!({}), cx);
                    }),
                );
        } else {
            clear_row = clear_row.opacity(0.35);
        }
        menu = menu.child(clear_row);
        strip = strip.child(menu);
    }

    // ── The form: pencil → input → ↵ ──────────────────────────────────────
    strip = strip.child(tool_button(
        "slate-tool-draw",
        "pencil",
        16.0,
        "tool:draw",
        state.tool == "draw",
        enabled,
        cx,
    ));

    // ── The command `<input>` — renders `buffer`; clicks emit focus ──────
    {
        let placeholder = if state.message_mode {
            "Write a message…"
        } else {
            // `commandPrefix` is not part of the contract; the original
            // rendered `Run a command or /terminal` for the default "/".
            "Run a command or /terminal"
        };
        let mut input = div()
            .id("slate-command-input")
            .flex_1()
            .min_w(px(160.0))
            .h(px(BTN))
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
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
            .when(state.open, |el| {
                el.child(
                    div()
                        .flex_none()
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .child("▌"),
                )
            });
        if enabled {
            input = input.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| {
                    cx.stop_propagation();
                    this.toolbar_command("focus_input", json!({}), cx);
                }),
            );
        }
        strip = strip.child(input);
    }

    // ── `↵` submit — `h-7 w-7 rounded-pill` in the shell, square here ─────
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
        if enabled && state.can_submit {
            el = el
                .cursor_pointer()
                .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command("submit", json!({}), cx);
                    }),
                );
        } else {
            // `disabled:opacity-30`
            el = el.opacity(0.3);
        }
        strip = strip.child(el);
    }

    // ── Select / Eraser / Pan — the trailing `Kn` trio ────────────────────
    strip = strip
        .child(tool_button(
            "slate-tool-select",
            "mouse-pointer-2",
            15.0,
            "tool:select",
            state.tool == "select",
            enabled,
            cx,
        ))
        .child(tool_button(
            "slate-tool-erase",
            "eraser",
            16.0,
            "tool:erase",
            state.tool == "erase",
            enabled,
            cx,
        ))
        .child(tool_button(
            "slate-tool-pan",
            "hand",
            16.0,
            "tool:pan",
            state.tool == "pan",
            enabled,
            cx,
        ));

    // ── `⋯` — `toolbar-more`, `h-8 w-8 rounded-pill`, square, menu toggle ──
    {
        let mut el = div()
            .id("slate-toolbar-more")
            .flex_none()
            .w(px(BTN))
            .h(px(BTN))
            .flex()
            .items_center()
            .justify_center()
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .child(crate::assets::icon(
                "ellipsis",
                16.0,
                theme::hex(theme::text::NORMAL),
            ));
        if enabled {
            el = el
                .cursor_pointer()
                .when(state.overflow_open, |el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                })
                .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command("overflow", json!({}), cx);
                    }),
                );
        } else {
            el = el.opacity(0.35);
        }
        strip = strip.child(el);
    }

    strip
}
