//! Settings modal — the native port of the Electron settings dialog's
//! canvas-facing half.
//!
//! The original was a tabbed dialog (`account`/`appearance`/`ai`/`code`/
//! `automation`); the account and AI tabs do not port, and neither does
//! `Theme` (a `localStorage["orcspace-theme"]` dark/photo switch — this
//! build is dark-only) or `Terminal shell` (`windowsShell`, Windows-only).
//! What remains lands here, same `Nt` section titles and hints:
//!
//! - **Canvas** — the `native-ui.json` prefs: `sidebar_open`, `auto_names`
//!   and the default `arrange_mode` pick. `UiPreferences` is loaded fresh
//!   here because the caller only hands over `&Settings` — the file is
//!   small and the modal renders only while open.
//! - **Agent permissions** — `settings.json` `autoApprovePermissions`.
//! - **Right-click menu** — the `favoriteWidgets` checklist that drives
//!   the canvas context menu ("Which widgets appear when you right-click
//!   the canvas.").
//! - **Image widget hotkey** — `imageInsertShortcut`, display + Reset.
//!   The original captured a keystroke on focus; that wiring does not
//!   exist here, so the row is read-only (STUB — coordinator follow-up).
//! - **Favorite terminal names** — `favoriteTerminalNames`, read-only
//!   display (the original was a textarea; same stub caveat).
//! - **Background** — the stored image plus `backgroundDim`/`backgroundBlur`
//!   steppers. `Choose…` emits `pick_background`, an extra verb outside
//!   the documented set, safe to leave unhandled until a file dialog is
//!   wired.
//!
//! Interaction: every control calls `CanvasView::settings_command` —
//! `toggle_pref`/`set_pref` carry `{"key": <snake_case field name>}`,
//! `toggle_fav` carries `{"kind": <widget kind>}`, and `close` takes `{}`.
//! Keys are the Rust field names, matching the match arms already in
//! `settings_command` (`sidebar_open`, `auto_approve_permissions`).

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

/// The 14 widget kinds — `canvas_view::WIDGET_CATALOG` order, labels
/// included. All stay checkable: the original's settings listed the whole
/// catalog, and kinds this build cannot spawn (`music-player`, `chat`) are
/// still valid favorites for the file's own round-trip.
const FAVORITE_KINDS: [(&str, &str); 14] = [
    ("terminal", "Terminal"),
    ("files", "Files"),
    ("sys-monitor", "System Monitor"),
    ("timer", "Timer"),
    ("planner", "Planner"),
    ("orchestration", "Orchestration"),
    ("browser", "Browser"),
    ("image", "Image"),
    ("links", "Links"),
    ("music-player", "Music Player"),
    ("chat", "AI Chat"),
    ("notes", "Notes"),
    ("calendar", "Calendar"),
    ("kanban", "Kanban"),
];

/// Kinds `widget_body` cannot render (`canvas_view::renderable`) — shown
/// with a marker instead of silently dropping a favorite the file holds.
fn renderable(kind: &str) -> bool {
    !matches!(kind, "music-player" | "chat")
}

/// The 13×13 checkbox: INFO fill + ✓ when on, ELEVATED box when off.
fn checkbox(checked: bool) -> Div {
    div()
        .flex_none()
        .w(px(13.0))
        .h(px(13.0))
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(9.0))
        .when(checked, |el| {
            el.bg(rgb(theme::hex(theme::accent::SAND)))
                .text_color(rgb(theme::hex(theme::monochrome::BASE)))
        })
        .when(!checked, |el| {
            el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        })
        .child(if checked { "✓" } else { "" })
}

/// One toggleable row — the original's `label`+checkbox rows. `action` and
/// `payload` go straight to `CanvasView::settings_command`.
fn toggle_row(
    id: impl Into<ElementId>,
    checked: bool,
    label: &'static str,
    detail: Option<String>,
    action: &'static str,
    payload: Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .px_2()
        .py(px(6.0))
        .flex()
        .flex_row()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .text_xs()
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(checkbox(checked))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .child(label)
                .when_some(detail, |el, detail| {
                    el.child(
                        div()
                            .text_size(px(10.0))
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .truncate()
                            .child(detail),
                    )
                }),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.settings_command(action, payload.clone(), cx);
            }),
        )
}

/// A section header — the original's `Nt` title + faint hint line.
fn section(title: &'static str, hint: &'static str) -> Div {
    let mut head = div().flex().flex_col().gap(px(2.0)).child(
        div()
            .text_xs()
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .child(title),
    );
    if !hint.is_empty() {
        head = head.child(
            div()
                .text_size(px(10.0))
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(hint),
        );
    }
    head
}

/// A small bordered button (Reset / Remove / Choose… / steppers).
fn button(
    id: impl Into<ElementId>,
    label: &'static str,
    action: &'static str,
    payload: Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    div()
        .id(id)
        .px_2()
        .py(px(2.0))
        .flex_none()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
        .cursor_pointer()
        .text_xs()
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
        })
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.settings_command(action, payload.clone(), cx);
            }),
        )
}

/// A read-only value row — used where the original had text inputs this
/// build cannot wire (shortcut capture, names textarea). The value sits in
/// a kbd-style box; an optional note explains the stub.
fn value_row(label: &'static str, value: String, note: &'static str) -> impl IntoElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(64.0))
                        .flex_none()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .child(label),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .px_2()
                        .py(px(3.0))
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .truncate()
                        .child(value),
                ),
        )
        .when(!note.is_empty(), |el| {
            el.child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(note),
            )
        })
}

/// `backgroundDim`/`backgroundBlur` — the original's sliders become −/+
/// steppers (5 per press, clamped 0–100) since there is no drag input here.
fn stepper_row(
    id: &'static str,
    label: &'static str,
    key: &'static str,
    value: u64,
    max: u64,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let down = value.saturating_sub(5);
    let up = (value + 5).min(max);
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(90.0))
                .flex_none()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(label),
        )
        .child(button(
            format!("{id}-down"),
            "−",
            "set_pref",
            json!({"key": key, "value": down}),
            cx,
        ))
        .child(
            div()
                .w(px(40.0))
                .flex_none()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                .child(format!("{value}%")),
        )
        .child(button(
            format!("{id}-up"),
            "+",
            "set_pref",
            json!({"key": key, "value": up}),
            cx,
        ))
}

/// The settings surface — returns the full overlay (dim scrim + centered
/// panel) so the caller can mount it as a root-level child like the
/// close-confirm dialog does. Called as
/// `settings_modal(&self.settings, cx)` from `canvas_view.rs`; the
/// `native-ui.json` prefs are loaded here (small file, modal is open only
/// while shown) since the signature carries `&Settings`.
///
/// Commands emitted through `CanvasView::settings_command`:
/// - `"close"`      `{}` — backdrop, ×, Close button (Escape is handled in
///   `on_key` already).
/// - `"toggle_pref"`/`"set_pref"` `{"key": ...}` with snake_case keys:
///   `sidebar_open`, `auto_names`, `auto_approve_permissions`,
///   `image_insert_shortcut`
///   (Reset → `"Mod+Shift+I"`), `background_dim`/`background_blur`
///   (value 0–100), `background` (Remove → `null`).
/// - `"toggle_fav"` `{"kind": <widget kind>}` — the favorites checklist.
/// - `"pick_background"` `{}` — OPTIONAL: wire a file dialog, or ignore.
pub fn settings_modal(
    settings: &slate_app::settings::Settings,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    // `native-ui.json` — the sidebar/auto-name/arrange half. Loaded rather
    // than passed: the mount site hands over `&self.settings` only.
    let prefs = slate_app::ui_preferences::UiPreferences::load_or_default();

    let term_names = settings.favorite_terminal_names();
    // The background lives outside the typed slice — the merged value
    // still carries a stored `background`/`backgroundImage` string.
    let merged = settings.to_value();
    let background = ["background", "backgroundImage"]
        .iter()
        .find_map(|key| merged.get(*key).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let mut body = div()
        .id("slate-settings-body")
        .flex()
        .flex_col()
        .gap_4()
        .min_h_0()
        .flex_1()
        .overflow_y_scroll()
        .py_1();

    // — Panes (native-ui.json prefs) —
    body = body.child(section(
        "Panes",
        "Startup state for the workspace shell.",
    ));
    body = body.child(toggle_row(
        "slate-settings-sidebar",
        prefs.sidebar_open,
        "Sidebar open",
        Some("Open the widget rail expanded on startup.".into()),
        "toggle_pref",
        json!({"key": "sidebar_open"}),
        cx,
    ));
    body = body.child(toggle_row(
        "slate-settings-auto-names",
        prefs.auto_names,
        "Auto-name terminals",
        Some("New terminals take a favorite or generated name.".into()),
        "toggle_pref",
        json!({"key": "auto_names"}),
        cx,
    ));

    // — Agent permissions (settings.json autoApprovePermissions) —
    body = body.child(section(
        "Agent permissions",
        "When an agent hits something that needs your OK (orc ask --type permission), choose whether it waits for you or approves itself.",
    ));
    body = body.child(toggle_row(
        "slate-settings-auto-approve",
        settings.auto_approve_permissions(),
        "Approve requests automatically",
        None,
        "toggle_pref",
        json!({"key": "auto_approve_permissions"}),
        cx,
    ));

    // — Right-click menu (settings.json favoriteWidgets) —
    body = body.child(section(
        "Right-click menu",
        "Which widgets appear in the pane strip's + menu (and on right-click).",
    ));
    {
        let mut list = div().flex().flex_col();
        for (kind, label) in FAVORITE_KINDS {
            let checked = settings.is_favorite_widget(kind);
            let detail = (!renderable(kind)).then(|| "Not spawnable in this build".to_owned());
            list = list.child(toggle_row(
                format!("slate-settings-fav-{kind}"),
                checked,
                label,
                detail,
                "toggle_fav",
                json!({"kind": kind}),
                cx,
            ));
        }
        body = body.child(list);
    }

    // — Image widget hotkey (settings.json imageInsertShortcut) —
    body = body.child(section(
        "Image widget hotkey",
        "Copy an image, then press this shortcut anywhere in the workspace to pin it for your agent.",
    ));
    body = body.child(
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().min_w_0().flex_1().child(value_row(
                        "Shortcut",
                        settings.image_insert_shortcut().to_owned(),
                        "",
                    )))
                    .child(button(
                        "slate-settings-hotkey-reset",
                        "Reset",
                        "set_pref",
                        json!({"key": "image_insert_shortcut", "value": "Mod+Shift+I"}),
                        cx,
                    )),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("Rebinding is not wired in this build — edit settings.json."),
            ),
    );

    // — Favorite terminal names (settings.json favoriteTerminalNames) —
    body = body.child(section(
        "Favorite terminal names",
        "One name per line, or separated by commas. New terminals use the first available name from your list.",
    ));
    body = body.child(value_row(
        "Names",
        if term_names.is_empty() {
            "No favorites — generated names are used".to_owned()
        } else {
            term_names.join(", ")
        },
        "Editing is not wired in this build — edit settings.json.",
    ));

    // — Background (theme wallpaper + dim/blur) —
    body = body.child(section(
        "Background",
        "Wallpaper behind the work area, with dim and blur.",
    ));
    body = body.child(
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div().min_w_0().flex_1().child(
                            div()
                                .px_2()
                                .py(px(6.0))
                                .border_1()
                                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                                .text_xs()
                                .text_color(rgb(theme::hex(if background.is_some() {
                                    theme::text::DIM
                                } else {
                                    theme::text::FAINT
                                })))
                                .truncate()
                                .child(
                                    background
                                        .clone()
                                        .unwrap_or_else(|| "No background selected".to_owned()),
                                ),
                        ),
                    )
                    .child(button(
                        "slate-settings-bg-pick",
                        "Choose…",
                        "pick_background",
                        json!({}),
                        cx,
                    ))
                    .when_some(background, |el, _| {
                        el.child(button(
                            "slate-settings-bg-clear",
                            "Remove",
                            "set_pref",
                            json!({"key": "background", "value": null}),
                            cx,
                        ))
                    }),
            )
            .child(stepper_row(
                "slate-settings-dim",
                "Dim",
                "background_dim",
                settings.background_dim().min(100),
                100,
                cx,
            ))
            .child(stepper_row(
                "slate-settings-blur",
                "Blur",
                "background_blur",
                // `setBlur` clamps 0–90 in the shell's theme context.
                settings.background_blur().min(90),
                90,
                cx,
            )),
    );

    // — Overlay: the scrim eats the press (click-outside = close) and the
    // panel eats its own presses so only real controls act — same shape as
    // the close-confirm modal.
    div()
        .id("slate-settings-overlay")
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x00000055))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _event, _window, cx| {
                cx.stop_propagation();
                this.settings_command("close", json!({}), cx);
            }),
        )
        .child(
            div()
                .id("slate-settings-panel")
                .w(px(480.0))
                .max_h(px(560.0))
                .flex()
                .flex_col()
                .gap_2()
                .p_4()
                .bg(rgb(theme::hex(theme::monochrome::SURFACE)))
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|_this, _event, _window, cx| {
                        cx.stop_propagation();
                    }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                .child("Settings"),
                        )
                        .child(
                            div()
                                .id("slate-settings-x")
                                .px_1()
                                .cursor_pointer()
                                .text_xs()
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .hover(|el| {
                                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                })
                                .child("×")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.settings_command("close", json!({}), cx);
                                    }),
                                ),
                        ),
                )
                .child(body)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .pt_1()
                        .border_t_1()
                        .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                        .child(
                            div()
                                .id("slate-settings-close")
                                .px_3()
                                .py_1()
                                .cursor_pointer()
                                .border_1()
                                .border_color(rgb(theme::hex(theme::hairline::FAINT)))
                                .text_xs()
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                                .hover(|el| {
                                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                })
                                .child("Close")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        this.settings_command("close", json!({}), cx);
                                    }),
                                ),
                        ),
                ),
        )
}
