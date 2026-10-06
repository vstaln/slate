//! The Browser widget pane — the Electron `BrowserWidget.tsx` port.
//!
//! Chrome mirrors the original's row: Back / Forward / Reload-or-Stop, the
//! omnibox input (URL, search or local path), a host chip, then the loading
//! hairline and the error banner. The page itself is the wry `WebView`
//! element from `crate::webview`.
//!
//! Persistence: the URL lives in journaled `widget.state.url` — the `set`
//! op — where the shell kept `localStorage["orcspace-browser-url:<id>"]`.
//! Page-initiated navigations come back through `webview::drain_events()` on
//! the canvas tick and are journaled the same way.
//!
//! Address editing is CanvasView state (`browser_editing`/`browser_address`,
//! same shape as the header rename input): the field click hands focus to
//! the canvas and keystrokes route through `on_key_down` — Enter navigates,
//! Escape reverts, Backspace edits.
//!
//! Pane actions go through `widget_command`:
//!
//! * `{"op": "browser_navigate", "url": <raw>}` — the omnibox submit. The
//!   dispatcher normalizes with `browser::to_navigation_url`, journals
//!   `set{url}`, and tells the webview.
//! * `{"op": "browser_nav", "dir": "back"|"forward"|"reload"|"stop"}` —
//!   the chrome buttons.
//! * `{"op": "browser_home"}` — the error banner's Home button.
//! * `{"op": "browser_edit", "editing": bool}` — omnibox focus tracking;
//!   carried as a pane op so CanvasView owns the buffer.
//!
//! Not ported: the dropped-media viewer (`orcspace:open-image/open-media`
//! had no canvas equivalent), the agent-action dispatch (no in-page
//! JavaScript bridge is wired), and HTML-fullscreen promotion — wry 0.57
//! doesn't expose fullscreen enter/leave, so WebKitGTK confines it to the
//! widget bounds. See `webview.rs` for the clipping/z-order caveats.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::browser::{self, HOME_URL};
use slate_app::projection::Widget;
use slate_app::theme;

/// `isHome` — the original compared `hostname === 'www.google.com' &&
/// pathname === '/'`, i.e. bare `HOME_URL` with an optional trailing slash.
fn is_home(url: &str) -> bool {
    url.trim_end_matches('/').eq_ignore_ascii_case(HOME_URL)
}

/// Widget's journaled URL, `HOME_URL` when never navigated — the original's
/// `readUrl` default.
fn widget_url(widget: &Widget) -> String {
    widget
        .state
        .as_ref()
        .and_then(|state| state.get("url"))
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| HOME_URL.to_owned())
}

/// One chrome icon button — the original's `browser-icon-btn`.
fn chrome_button<'a>(
    id: impl Into<ElementId>,
    label: &'static str,
    title: &'static str,
    enabled: bool,
    zoom: f32,
    on_click: impl Fn(&mut crate::canvas_view::CanvasView, &mut Context<crate::canvas_view::CanvasView>)
        + 'static,
    cx: &'a mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    let _ = title; // gpui has no tooltip plumbing; the label carries it.
    let mut button = div()
        .id(id)
        .flex_none()
        .w(px(24.0 * zoom))
        .h(px(24.0 * zoom))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(10.0 * zoom))
        .text_size(px(13.0 * zoom))
        .text_color(rgb(theme::hex(if enabled {
            theme::text::DIM
        } else {
            theme::text::FAINT
        })));
    if enabled {
        button = button
            .cursor_pointer()
            .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _event, _window, cx| {
                    cx.stop_propagation();
                    on_click(this, cx);
                }),
            );
    } else {
        // disabled:pointer-events-none — the 30%-opacity look minus a cursor.
        button = button.opacity(0.4);
    }
    button.child(label)
}

pub fn browser_pane(
    widget: &Widget,
    zoom: f32,
    // The canvas's omnibox edit state, passed in — `cx.entity().read(cx)`
    // would re-borrow CanvasView mid-render and panic.
    editing_id: Option<&str>,
    editing_buffer: &str,
    // The chrome-free canvas region the native webview child is clipped to —
    // X11 children don't respect GPUI masks, so the element intersects its
    // bounds with this rect before moving the window.
    clip: Bounds<Pixels>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = widget.id.clone();
    let url = widget_url(widget);
    let status = crate::webview::status_for(&wid).unwrap_or_default();
    let loading = status.loading;
    let load_error = status.error.clone();
    // The live URL is the webview's once it has one — the journaled one is
    // the same modulo the tick's write-back ordering, and the webview wins
    // the instant it navigates.
    let shown_url = status.url.clone().unwrap_or_else(|| url.clone());
    let home = is_home(&shown_url);

    let editing = editing_id == Some(wid.as_str());
    let address = if editing {
        editing_buffer.to_owned()
    } else {
        // `value={isHome ? '' : address}` — home shows the placeholder.
        if home { String::new() } else { shown_url.clone() }
    };
    let host = browser::host_of(&shown_url);
    let is_https = shown_url.starts_with("https://");
    let is_file = shown_url.starts_with("file:");

    let wid_back = wid.clone();
    let wid_fwd = wid.clone();
    let wid_reload = wid.clone();
    let wid_addr = wid.clone();
    let wid_retry = wid.clone();
    let wid_home = wid.clone();

    let nav_action = |dir: &str| json!({ "op": "browser_nav", "dir": dir });

    let chrome = div()
        .flex_none()
        .h(px(30.0 * zoom))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0 * zoom))
        .px(px(8.0 * zoom))
        .border_b_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .child(chrome_button(
            ("browser-back", widget.z as u64),
            "←",
            "Back",
            status.can_back,
            zoom,
            move |this, cx| this.widget_command(&wid_back, nav_action("back"), cx),
            cx,
        ))
        .child(chrome_button(
            ("browser-fwd", widget.z as u64),
            "→",
            "Forward",
            status.can_forward,
            zoom,
            move |this, cx| this.widget_command(&wid_fwd, nav_action("forward"), cx),
            cx,
        ))
        .child(chrome_button(
            ("browser-reload", widget.z as u64),
            // Stop while loading, Reload otherwise — the X/RotateCw swap.
            if loading { "✕" } else { "⟳" },
            if loading { "Stop" } else { "Reload" },
            true,
            zoom,
            move |this, cx| {
                this.widget_command(
                    &wid_reload,
                    nav_action(if loading { "stop" } else { "reload" }),
                    cx,
                )
            },
            cx,
        ))
        .child(
            // The omnibox — scheme icon, buffer-or-url, "▌" caret while
            // editing. Click hands the keyboard to the canvas.
            div()
                .id(("browser-address", widget.z as u64))
                .flex_1()
                .min_w_0()
                .h(px(22.0 * zoom))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0 * zoom))
                .px(px(8.0 * zoom))
                .rounded_full()
                .bg(rgb(theme::hex(theme::monochrome::BASE)))
                .cursor_text()
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.0 * zoom))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(if is_file {
                            "file"
                        } else if is_https {
                            "https"
                        } else {
                            "⌕"
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_size(px(11.0 * zoom))
                        .text_color(rgb(theme::hex(if editing {
                            theme::text::NORMAL
                        } else {
                            theme::text::DIM
                        })))
                        .child(if address.is_empty() {
                            "Search or enter address".to_owned()
                        } else {
                            address.clone()
                        })
                        .when(editing, |el| {
                            el.child(
                                div()
                                    .text_color(rgb(theme::hex(theme::text::DIM)))
                                    .child("▌".to_owned()),
                            )
                        }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        cx.stop_propagation();
                        window.focus(&this.focus, cx);
                        // Focus semantics from the original: entering edit
                        // selects the current value (our whole buffer is the
                        // selection, same as rename); a stale edit on another
                        // browser reverts first — blur.
                        if this.browser_editing.as_deref() != Some(wid_addr.as_str()) {
                            this.browser_editing = None;
                            this.browser_address = shown_url.clone();
                            this.browser_editing = Some(wid_addr.clone());
                        }
                        this.active = Some(wid_addr.clone());
                        cx.notify();
                    }),
                ),
        )
        .when(!editing && !home && !host.is_empty(), |el| {
            el.child(
                div()
                    .flex_none()
                    .px_1()
                    .text_size(px(10.0 * zoom))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(host),
            )
        });

    // h-px load-bar track with a 1/3 accent — static here; the original's
    // was CSS-animated, ours repaints on state change only.
    let load_bar = loading && !home;
    let error_bar = !loading && load_error.is_some();

    let mut pane = div()
        .size_full()
        .flex()
        .flex_col()
        .child(chrome)
        .when(load_bar, |el| {
            el.child(
                div()
                    .flex_none()
                    .h(px(1.0))
                    .w_full()
                    .bg(rgb(theme::hex(theme::hairline::SOFT)))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(1.0 / 3.0))
                            .bg(rgb(theme::hex(theme::status::INFO))),
                    ),
            )
        })
        .when(error_bar, |el| {
            let error = load_error.clone().unwrap_or_default();
            el.child(
                div()
                    .flex_none()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .px(px(8.0 * zoom))
                    .py(px(4.0 * zoom))
                    .border_b_1()
                    .border_color(rgb(theme::hex(theme::status::DANGER)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_size(px(11.0 * zoom))
                            .text_color(rgb(theme::hex(theme::status::DANGER)))
                            .child(format!("Error — {error}")),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                div()
                                    .id(("browser-retry", widget.z as u64))
                                    .px_2()
                                    .py(px(2.0))
                                    .rounded_md()
                                    .text_size(px(10.0 * zoom))
                                    .text_color(rgb(theme::hex(theme::status::DANGER)))
                                    .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                    .cursor_pointer()
                                    .hover(|b| {
                                        b.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                                    })
                                    .child("Retry")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            this.widget_command(
                                                &wid_retry,
                                                nav_action("reload"),
                                                cx,
                                            );
                                        }),
                                    ),
                            )
                            .child(
                                div()
                                    .id(("browser-home", widget.z as u64))
                                    .px_2()
                                    .py(px(2.0))
                                    .rounded_md()
                                    .text_size(px(10.0 * zoom))
                                    .text_color(rgb(theme::hex(theme::text::DIM)))
                                    .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                    .cursor_pointer()
                                    .hover(|b| {
                                        b.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                                    })
                                    .child("Home")
                                    .on_mouse_down(
                                        MouseButton::Left,
                                        cx.listener(move |this, _e, _w, cx| {
                                            cx.stop_propagation();
                                            this.widget_command(
                                                &wid_home,
                                                json!({ "op": "browser_home" }),
                                                cx,
                                            );
                                        }),
                                    ),
                            ),
                    ),
            )
        })
        .child(
            // The page surface. At home the original hid a warm about:blank
            // webview behind a splash; a native child window can't be hidden
            // under GPUI content, so we simply don't mount the element until
            // the first real navigation.
            div().flex_1().min_h_0().child(if home {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(14.0 * zoom))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                            .child("Search the web"),
                    )
                    .child(
                        div()
                            .text_size(px(11.0 * zoom))
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .child("Enter a URL or search term in the address bar above."),
                    )
                    .into_any_element()
            } else {
                crate::webview::WebView::for_url(&wid, url.clone())
                    .zoom(zoom)
                    .clip(clip)
                    .size_full()
                    .into_any_element()
            }),
        );

    let _ = &mut pane;
    pane
}
