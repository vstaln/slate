//! The Browser widget pane — the Electron `BrowserWidget.tsx` port.
//!
//! Chrome mirrors the original's row: Back / Forward / Reload-or-Stop, the
//! omnibox input (URL, search or local path), a host chip, then the loading
//! hairline and the error banner. The page itself is the wry `WebView`
//! element from `slate_app::webview`.
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
//! * `{"op": "browser_retry"}` — the banner's Retry, capped at 3 per the
//!   original's `retryCountRef` (journaled as `state.retry`, reset on a
//!   clean `Loaded`).
//! * `{"op": "browser_edit", "editing": bool}` — omnibox focus tracking;
//!   carried as a pane op so CanvasView owns the buffer.
//! * `{"op": "browser_media_close"}` — the MediaViewer's close; clears
//!   journaled `state.media` and the webview returns to the page URL.
//! * `{"op": "browser_media_open", "path": <abs>}` — the doc card's
//!   open-externally (`window.api.fs.openPath` → `xdg-open`).
//!
//! Dropped media (`state.media`) renders through the same webview — WebKit
//! plays the file: URL inline — under the original's MediaViewer chrome: a
//! name bar + Close for video/audio/pdf/text, a bare surface for image, and
//! the open-externally card for doc. Not ported: HTML-fullscreen promotion
//! — wry 0.57 doesn't expose fullscreen enter/leave, so WebKitGTK confines
//! it to the widget bounds. See `webview.rs` for the clipping/z-order
//! caveats.

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

/// `DroppedMedia` — the `{mediaUrl, path, name, kind}` record a file drop
/// journals into `state.media` (the shell's `orcspace-browser-media:<id>`).
struct PaneMedia {
    path: String,
    name: String,
    kind: String,
}

/// `readMedia` — absent/blank `path` means no media (a `media: null` close
/// reads as absent too).
fn widget_media(widget: &Widget) -> Option<PaneMedia> {
    let media = widget.state.as_ref()?.get("media")?;
    let path = media.get("path").and_then(Value::as_str)?.to_owned();
    if path.is_empty() {
        return None;
    }
    let name = media
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or("Dropped file")
        .to_owned();
    let kind = media
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("doc")
        .to_owned();
    Some(PaneMedia { path, name, kind })
}

/// Media kinds WebKit can render inline — the MediaViewer's element kinds.
/// `doc` and anything unknown get the open-externally card instead.
fn media_renders_inline(kind: &str) -> bool {
    matches!(kind, "image" | "video" | "audio" | "pdf" | "text")
}

/// Widget's journaled URL, `HOME_URL` when never navigated — the original's
/// `readUrl` default. A viewable media drop takes over the surface: its
/// `path` becomes a `file:` URL the webview loads so webkit renders
/// image/video/audio/pdf/text inline — the MediaViewer's native counterpart.
fn widget_url(widget: &Widget) -> String {
    let state = widget.state.as_ref();
    if let Some(media) = widget_media(widget) {
        if media_renders_inline(&media.kind) {
            return file_url(&media.path);
        }
    }
    if let Some(url) = state
        .and_then(|state| state.get("url"))
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
    {
        return url.to_owned();
    }
    HOME_URL.to_owned()
}

/// `file://` + percent-encoded path — the omnibox's path-to-URL half.
fn file_url(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 8);
    out.push_str("file://");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'/'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'!'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'&'
            | b'+'
            | b','
            | b';'
            | b'='
            | b'@'
            | b':' => out.push(byte as char),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
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
    // The omnibox's caret and selection anchor — the DOM input's
    // `selectionStart`/`selectionEnd`.
    editing_cursor: usize,
    editing_sel: Option<usize>,
    // The chrome-free canvas region the native webview child is clipped to —
    // X11 children don't respect GPUI masks, so the element intersects its
    // bounds with this rect before moving the window.
    clip: Bounds<Pixels>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = widget.id.clone();
    let media = widget_media(widget);
    let url = widget_url(widget);
    let status = slate_app::webview::status_for(&wid).unwrap_or_default();
    let loading = status.loading;
    let load_error = status.error.clone();
    // `retryCountRef` journaled — Retry hides at 3 like the original's
    // `retryCountRef.current < 3` gate.
    let retries = widget
        .state
        .as_ref()
        .and_then(|state| state.get("retry"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // The live URL is the webview's once it has one — the journaled one is
    // the same modulo the tick's write-back ordering, and the webview wins
    // the instant it navigates. A media drop reports its file: URL here
    // until close returns the webview to the page.
    let shown_url = status.url.clone().unwrap_or_else(|| url.clone());
    // `isHome = !media && …` — the media viewer overlays the home check.
    let home = is_home(&shown_url) && media.is_none();

    let editing = editing_id == Some(wid.as_str());
    let address = if editing {
        editing_buffer.to_owned()
    } else if let Some(media) = &media {
        // The original's `setAddress(media.name)` — the file name rides the
        // omnibox while the viewer is up.
        media.name.clone()
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
    let wid_media = wid.clone();

    // What a click into the omnibox seeds the buffer with — the media name
    // while the viewer is up, else the shown URL (`media ? media.name : url`,
    // the same value Escape restores).
    let address_seed = media
        .as_ref()
        .map(|media| media.name.clone())
        .unwrap_or_else(|| shown_url.clone());

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
            // `disabled={!canGoBack || !!media}` — the viewer is not history.
            status.can_back && media.is_none(),
            zoom,
            move |this, cx| this.widget_command(&wid_back, nav_action("back"), cx),
            cx,
        ))
        .child(chrome_button(
            ("browser-fwd", widget.z as u64),
            "→",
            "Forward",
            status.can_forward && media.is_none(),
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
                        .flex()
                        .flex_row()
                        .text_size(px(11.0 * zoom))
                        .text_color(rgb(theme::hex(if editing {
                            theme::text::NORMAL
                        } else {
                            theme::text::DIM
                        })))
                        .when(!editing, |el| {
                            el.child(if address.is_empty() {
                                "Search or enter address".to_owned()
                            } else {
                                address.clone()
                            })
                        })
                        .when(editing, |el| {
                            // The DOM input's selection model: anchor
                            // (browser_sel)..cursor highlights, a collapsed
                            // pair is the caret. Byte offsets snap to char
                            // boundaries defensively.
                            let snap = |at: usize| {
                                let mut at = at.min(address.len());
                                while at > 0 && !address.is_char_boundary(at) {
                                    at -= 1;
                                }
                                at
                            };
                            let cursor = snap(editing_cursor);
                            let (lo, hi) = editing_sel
                                .map(|anchor| {
                                    (snap(anchor).min(cursor), snap(anchor).max(cursor))
                                })
                                .unwrap_or((cursor, cursor));
                            let mut row = el
                                .child(address[..lo].to_owned())
                                .when(hi > lo, |row| {
                                    row.child(
                                        div()
                                            .bg(rgb(theme::hex(theme::accent::SAND)))
                                            .text_color(rgb(theme::hex(theme::monochrome::BASE)))
                                            .child(address[lo..hi].to_owned()),
                                    )
                                })
                                .when(hi == lo, |row| {
                                    row.child(
                                        div()
                                            .text_color(rgb(theme::hex(theme::text::DIM)))
                                            .child("▌".to_owned()),
                                    )
                                });
                            if hi < address.len() {
                                row = row.child(address[hi..].to_owned());
                            } else if address.is_empty() {
                                row = row.child(
                                    div()
                                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                                        .child("Search or enter address".to_owned()),
                                );
                            }
                            row
                        }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        cx.stop_propagation();
                        window.focus(&this.focus, cx);
                        // Focus semantics from the original: entering edit
                        // selects the current value (`e.target.select()` —
                        // anchor 0, caret at the end); a stale edit on
                        // another browser reverts first — blur.
                        if this.browser_editing.as_deref() != Some(wid_addr.as_str()) {
                            this.browser_editing = None;
                            this.browser_address = address_seed.clone();
                            this.browser_editing = Some(wid_addr.clone());
                            this.browser_cursor = this.browser_address.len();
                            this.browser_sel = Some(0);
                        }
                        this.active = Some(wid_addr.clone());
                        cx.notify();
                    }),
                ),
        )
        .when(!editing && media.is_none() && !home && !host.is_empty(), |el| {
            el.child(
                div()
                    .flex_none()
                    .px_1()
                    .text_size(px(10.0 * zoom))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(host),
            )
        })
        // The MediaViewer's chrome-row close — `closeMedia` wore "✕ Browser".
        .when(media.is_some(), |el| {
            el.child(
                div()
                    .id(("browser-media-close", widget.z as u64))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .px(px(8.0 * zoom))
                    .py(px(2.0 * zoom))
                    .rounded(px(10.0 * zoom))
                    .border_1()
                    .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                    .text_size(px(10.0 * zoom))
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .cursor_pointer()
                    .hover(|b| {
                        b.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .child("✕ Browser")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.widget_command(
                                &wid_media,
                                json!({ "op": "browser_media_close" }),
                                cx,
                            );
                        }),
                    ),
            )
        });

    // h-px load-bar track with a 1/3 accent — static here; the original's
    // was CSS-animated, ours repaints on state change only. `loading &&
    // !media && !isHome` — the viewer never shows it.
    let load_bar = loading && !home && media.is_none();
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
                            .bg(rgb(theme::hex(theme::accent::SAND))),
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
                            // `retryCountRef.current < 3` — three failed
                            // retries and only Home remains.
                            .when(retries < 3, |el| {
                                el.child(
                                    div()
                                        .id(("browser-retry", widget.z as u64))
                                        .px_2()
                                        .py(px(2.0))
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
                                                    json!({ "op": "browser_retry" }),
                                                    cx,
                                                );
                                            }),
                                        ),
                                )
                            })
                            .child(
                                div()
                                    .id(("browser-home", widget.z as u64))
                                    .px_2()
                                    .py(px(2.0))
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
            // the first real navigation. Dropped media takes over the same
            // surface (the MediaViewer's overlay): viewable kinds ride the
            // webview at their file: URL under the original's name bar,
            // `doc` gets the open-externally card.
            div().flex_1().min_h_0().flex().flex_col().child(
                if let Some(media) = &media {
                    media_surface(&wid, media, &url, widget.z, zoom, clip, cx)
                        .into_any_element()
                } else if home {
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
                    slate_app::webview::WebView::for_url(&wid, url.clone())
                        .zoom(zoom)
                        .clip(clip)
                        .size_full()
                        .into_any_element()
                },
            ),
        );

    let _ = &mut pane;
    pane
}

/// The `MediaViewer` — viewable kinds (video/audio/pdf/text) ride the same
/// webview at their `file:` URL under the original's name bar + Close;
/// `image` stays bare like the original's `<img>`; `doc` and unknown kinds
/// get the open-externally card (`window.api.fs.openPath` → `browser_media_open`).
/// Closing clears `state.media` — `widget_url` falls back to the page URL
/// and `maybe_sync_url` drives the webview back to it.
fn media_surface<'a>(
    wid: &str,
    media: &PaneMedia,
    url: &str,
    // The widget's z — the element-id discriminator every other button in
    // this file already uses.
    z: f64,
    zoom: f32,
    clip: Bounds<Pixels>,
    cx: &'a mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    let wid_close = wid.to_owned();
    // The name bar from the original's video/pdf/text viewers — the file
    // name left, Close right. Audio takes the same bar: the original's
    // centered `<audio>` card has no native counterpart (the webview shows
    // the player), and the bar keeps name + close reachable.
    let name_bar = |wid: String| {
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
            .border_color(rgb(theme::hex(theme::hairline::SOFT)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_size(px(10.0 * zoom))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(media.name.clone()),
            )
            .child(
                div()
                    .id(("browser-media-close-body", z as u64))
                    .flex_none()
                    .px_1()
                    .py(px(2.0))
                    .text_size(px(10.0 * zoom))
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .cursor_pointer()
                    .hover(|b| {
                        b.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                            .text_color(rgb(theme::hex(theme::text::NORMAL)))
                    })
                    .child("Close")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _e, _w, cx| {
                            cx.stop_propagation();
                            this.widget_command(
                                &wid,
                                json!({ "op": "browser_media_close" }),
                                cx,
                            );
                        }),
                    ),
            )
    };

    match media.kind.as_str() {
        // `<img className="object-contain">` — bare surface, the chrome
        // row's "✕ Browser" closes it.
        "image" => slate_app::webview::WebView::for_url(wid, url.to_owned())
            .zoom(zoom)
            .clip(clip)
            .size_full()
            .into_any_element(),
        "video" | "audio" | "pdf" | "text" => div()
            .size_full()
            .flex()
            .flex_col()
            .child(name_bar(wid_close))
            .child(
                div().flex_1().min_h_0().child(
                    slate_app::webview::WebView::for_url(wid, url.to_owned())
                        .zoom(zoom)
                        .clip(clip)
                        .size_full(),
                ),
            )
            .into_any_element(),
        // `doc` — the open-externally card: name, path, Open + Back.
        _ => {
            let wid_open = wid.to_owned();
            let path = media.path.clone();
            let wid_back = wid.to_owned();
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(rgb(theme::hex(theme::monochrome::BASE)))
                .child(
                    div()
                        .w(px(28.0 * zoom))
                        .h(px(28.0 * zoom))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0 * zoom))
                        .border_1()
                        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                        .text_size(px(13.0 * zoom))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child("file"),
                )
                .child(
                    div()
                        .text_size(px(12.0 * zoom))
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(media.name.clone()),
                )
                .child(
                    div()
                        .text_size(px(10.0 * zoom))
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(media.path.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_1()
                        .child(
                            div()
                                .id(("browser-media-open", z as u64))
                                .px_2()
                                .py(px(4.0 * zoom))
                                .rounded(px(6.0 * zoom))
                                .text_size(px(11.0 * zoom))
                                .bg(rgb(theme::hex(theme::accent::SAND)))
                                .text_color(rgb(theme::hex(theme::monochrome::BASE)))
                                .cursor_pointer()
                                .child("Open")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, _w, cx| {
                                        cx.stop_propagation();
                                        this.widget_command(
                                            &wid_open,
                                            json!({
                                                "op": "browser_media_open",
                                                "path": path,
                                            }),
                                            cx,
                                        );
                                    }),
                                ),
                        )
                        .child(
                            div()
                                .id(("browser-media-back", z as u64))
                                .px_2()
                                .py(px(4.0 * zoom))
                                .rounded(px(6.0 * zoom))
                                .border_1()
                                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                                .text_size(px(11.0 * zoom))
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                                .cursor_pointer()
                                .hover(|b| {
                                    b.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                })
                                .child("Back")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _e, _w, cx| {
                                        cx.stop_propagation();
                                        this.widget_command(
                                            &wid_back,
                                            json!({ "op": "browser_media_close" }),
                                            cx,
                                        );
                                    }),
                                ),
                        ),
                )
                .into_any_element()
        }
    }
}
