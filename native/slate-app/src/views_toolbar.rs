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
//! This port follows the coordinator's flattened strip: the overflow
//! menu's contents are inlined (workspace chip, mode select, target
//! select, zoom group, clear-strokes, settings) and undo/redo ride the
//! strip too — in the shell they lived on the TitleBar
//! (`titlebar-undo`/`titlebar-redo`, `disabled` + `text-white/25`).
//! The shell's `pan` tool is not ported: the contract's tool actions are
//! `tool:select` / `tool:draw` / `tool:erase` only, and middle-drag pans on
//! every tool.
//!
//! Deviations and why, flagged for the coordinator:
//!
//! - **Selects are click-to-cycle chips.** gpui has no `<select>` and
//!   `ToolbarState` carries no popup-open flag, so Mode toggles
//!   `command`↔`message` on click and the terminal chip cycles the list
//!   (left = next, right = previous). Both emit the documented verbs.
//! - **No `↵` submit button / `⋯` menu.** Enter and Escape are keystrokes
//!   the caller already routes through `command_bar`; there is no
//!   `submit` verb, so the button is omitted rather than dead.
//! - **No backdrop blur / scale transitions.** gpui has no
//!   `backdrop-filter`; the strip keeps the 80% surface fill, and a
//!   selected swatch is a white border instead of `scale-110`.
//! - **Swatch row is inline**, shown only while `tool == "draw"` — the
//!   original's hover-to-keep-open (`B` state) has no counterpart field.
//! - Glyphs stand in for lucide icons (no SVG assets here):
//!   ↖ select · ✎ draw · ⌫ erase · ✕ clear · ⌂ workspace ·
//!   ⛶ fit · ↶/↷ undo/redo · ⚙ settings.
//!
//! Emitted through `CanvasView::toolbar_command(action, json!({}), cx)`:
//! `tool:select` `tool:draw` `tool:erase`, `stroke:<0-7>`,
//! `clear_strokes`, `workspace:pick`, `mode:command` `mode:message`,
//! `target:<id>`, `focus_input`, `suggest:<kind>`, `fit`, `zoom_out`,
//! `zoom_reset`, `zoom_in`, `undo`, `redo`, `settings`.

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
    /// `canUndo` / `canRedo` — the TitleBar buttons' disabled gates.
    pub can_undo: bool,
    pub can_redo: bool,
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
    glyph: &'static str,
    glyph_size: f32,
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
        .rounded_xl() // rounded-panel — radiusPanel: 12px
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(glyph_size))
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
    el.child(glyph)
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
        .rounded_xl()
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
                // text-accent — the dark theme's accent is plain white.
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(format!("{prefix}{kind}")),
        )
        .child(
            div()
                .ml_auto()
                .min_w_0()
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

/// A labeled chip (`h-8 px-2`) — the flattened port of the overflow
/// menu's buttons and `<select>`s. `label` is the leading glyph, `value`
/// the truncated text. Emits `action`; `None` renders it inert+dim.
fn chip(
    id: &'static str,
    glyph: &'static str,
    value: String,
    action: Option<String>,
    enabled: bool,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let mut el = div()
        .id(id)
        .flex_none()
        .h(px(BTN))
        .px_2()
        .rounded_xl()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .text_size(px(11.0))
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(glyph),
        )
        .child(div().min_w_0().max_w(px(140.0)).truncate().child(value));
    if let (true, Some(action)) = (enabled, action) {
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
                    this.toolbar_command(&action, json!({}), cx);
                }),
            );
    } else {
        el = el.opacity(0.5);
    }
    el
}

/// One swatch (`h-5 w-5 rounded-pill border`) — the picked color gets the
/// accent border (white in this theme) the original drew at `scale-110`.
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
        .rounded_full()
        .border_1()
        .border_color(rgb(theme::hex(if selected {
            theme::text::NORMAL // border-accent
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
/// `Kn`, same hover fill.
fn zoom_button(
    id: &'static str,
    label: String,
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
        .rounded_xl()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(if mono { 10.0 } else { 12.0 }))
        .when(mono, |el| el.font_family("Departure Mono"))
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .child(label);
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
/// bottom-6 left-1/2 -translate-x-1/2` part; this returns the bar sized
/// to its contents (`w_auto`, capped at the original's 780px).
pub fn toolbar_pane(
    state: &crate::views_toolbar::ToolbarState,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let enabled = state.enabled;

    let mut strip = div()
        .id("slate-toolbar")
        .w_auto()
        .max_w(px(780.0))
        .h(px(STRIP_H))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .rounded_xl()
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
            .rounded_xl()
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

    // ── Tool buttons — `Kn`s for select/draw/erase ────────────────────────
    strip = strip
        .child(tool_button(
            "slate-tool-select",
            "↖",
            15.0,
            "tool:select",
            state.tool == "select",
            enabled,
            cx,
        ))
        .child(tool_button(
            "slate-tool-draw",
            "✎",
            16.0,
            "tool:draw",
            state.tool == "draw",
            enabled,
            cx,
        ))
        .child(tool_button(
            "slate-tool-erase",
            "⌫",
            16.0,
            "tool:erase",
            state.tool == "erase",
            enabled,
            cx,
        ));

    // ── "Clear all drawings" (`tool-clear-strokes`) ───────────────────────
    strip = strip.child(tool_button(
        "slate-clear-strokes",
        "✕",
        14.0,
        "clear_strokes",
        false,
        enabled,
        cx,
    ));

    // ── The 8-swatch row — the floating `Stroke Color` group, inlined ─────
    // Original gate: `me = t !== "draw" && !hovered`; the hover half has
    // no state-field counterpart, so this renders only in draw mode.
    if state.tool == "draw" {
        let mut row = div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded_xl()
            .border_1()
            .border_color(rgb(theme::hex(theme::hairline::FAINT)))
            .bg(rgba(0x121212E5)) // bg-bg-raise/90
            .px(px(10.0))
            .h(px(BTN));
        for (index, (hex, _name)) in STROKE_SWATCHES.iter().enumerate() {
            row = row.child(swatch(index, hex, index == state.stroke_color, enabled, cx));
        }
        strip = strip.child(row);
    }

    // ── Workspace chip — the menu's "Change directory" (folder + basename)
    strip = strip.child(chip(
        "slate-workspace",
        "⌂",
        if state.workspace_name.is_empty() {
            "No folder".to_owned()
        } else {
            state.workspace_name.clone()
        },
        Some("workspace:pick".to_owned()),
        enabled,
        cx,
    ));

    // ── Mode select — `command`/`message`, a click-to-toggle chip ─────────
    strip = strip.child(chip(
        "slate-mode",
        "›",
        if state.message_mode {
            "msg".to_owned()
        } else {
            "cmd".to_owned()
        },
        Some(
            if state.message_mode {
                "mode:command"
            } else {
                "mode:message"
            }
            .to_owned(),
        ),
        enabled,
        cx,
    ));

    // ── Terminal select — the target `<select>`, a click-to-cycle chip ────
    // Left click takes the next terminal, right click the previous one;
    // with one or zero terminals it is just the current target's name.
    {
        // The current target's slot: `target_name` is a title, but an id
        // match is accepted too (coordinator's choice of payload). No
        // match lands on index 0 — the original's `d[0]?.id` fallback.
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
        let mut el = div()
            .id("slate-target")
            .flex_none()
            .h(px(BTN))
            .px_2()
            .rounded_xl()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(px(11.0))
            .text_color(rgb(theme::hex(theme::text::DIM)))
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("❯"),
            )
            .child(div().min_w_0().max_w(px(140.0)).truncate().child(title));
        if let (true, Some(next)) = (enabled, next) {
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
                        this.toolbar_command(&next, json!({}), cx);
                    }),
                );
            if let Some(prev) = prev {
                el = el.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        this.toolbar_command(&prev, json!({}), cx);
                    }),
                );
            }
        } else {
            el = el.opacity(0.5);
        }
        strip = strip.child(el);
    }

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

    // ── Zoom group — the menu's `bg-bg-raise` Canvas-zoom block ──────────
    strip = strip.child(
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            .rounded_xl()
            .bg(rgb(theme::hex(theme::monochrome::SURFACE))) // bg-bg-raise
            .px(px(6.0))
            .py(px(2.0))
            .child(zoom_button(
                "slate-zoom-fit",
                "⛶".to_owned(),
                ZOOM_BTN,
                "fit",
                false,
                enabled,
                cx,
            ))
            .child(zoom_button(
                "slate-zoom-out",
                "−".to_owned(),
                ZOOM_BTN,
                "zoom_out",
                false,
                enabled,
                cx,
            ))
            .child(zoom_button(
                "slate-zoom-reset",
                format!("{}%", state.zoom_percent),
                42.0,
                "zoom_reset",
                true,
                enabled,
                cx,
            ))
            .child(zoom_button(
                "slate-zoom-in",
                "+".to_owned(),
                ZOOM_BTN,
                "zoom_in",
                false,
                enabled,
                cx,
            )),
    );

    // ── Undo/redo — the TitleBar pair, `disabled`+dimmed when empty ──────
    strip = strip
        .child(tool_button(
            "slate-undo",
            "↶",
            15.0,
            "undo",
            false,
            enabled && state.can_undo,
            cx,
        ))
        .child(tool_button(
            "slate-redo",
            "↷",
            15.0,
            "redo",
            false,
            enabled && state.can_redo,
            cx,
        ));

    // ── Settings — `pe()`'s `orcspace:open-settings` dispatch ────────────
    strip = strip.child(tool_button(
        "slate-settings",
        "⚙",
        15.0,
        "settings",
        false,
        enabled,
        cx,
    ));

    strip
}
