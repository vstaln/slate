//! WebView element for rendering HTML content using platform-native webviews.
//!
//! Vendored from gpui PR #54433 ("webview element") — the copy that lived at
//! `native/orcspace-app/src/webview.rs` @2cf4566 — re-ported to gpui v1.22.0
//! (zed tag) and extended for the BrowserWidget parity port:
//!
//! * per-widget ids: `WebView::for_url(widget_id, url)` maps a canvas widget
//!   id onto a stable `usize`, so a widget keeps its webview across renders,
//!   moves and resizes — create-once, move-later.
//! * commands from the GPUI thread (`navigate`, `reload`, `stop`, `go_back`,
//!   `go_forward`) ride the same channel bounds do.
//! * wry's navigation / page-load / new-window hooks are wired: the
//!   will-navigate guard and popup suppression from the shell's
//!   `windowManager.ts` live in `create_child` below, and everything the
//!   chrome needs to show (`status_for`, `drain_events`) is mirrored into
//!   main-thread statics.
//!
//! On Linux/X11 webviews are embedded as child views inside the GPUI window
//! via wry's `build_as_child()`; the GTK loop they need runs on a dedicated
//! thread. On Linux/Wayland, where `build_as_child` is unsupported, each
//! webview opens in a separate GTK window (accepted per spec). Other
//! platforms render nothing — the pane falls back to a notice.
//!
//! Limitations versus the Electron `<webview>` this replaces:
//!
//! * The native child window is NOT clipped or z-ordered by GPUI: an
//!   overlapping widget still leaves the webview painted on top. What we
//!   can and do fix is clipping: the element intersects its layout bounds
//!   with the current `content_mask()` and a caller-supplied `clip` rect
//!   (the canvas area under the floating title bar / beside the rail), so
//!   a browser panned under the chrome no longer bleeds over it; fully
//!   covered webviews are hidden via `set_visible`.
//! * wry 0.57 exposes no load-failure or fullscreen hooks on Linux, so a
//!   dead page reports `loading → finished` with no error text (the chrome's
//!   error banner still covers URL validation and blocked navigations), and
//!   HTML fullscreen is confined to the widget's bounds by WebKitGTK rather
//!   than promoting the widget the way `onFullscreenChange` did.

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, StyleRefinement, Styled, Window, px,
};
use gpui::Refineable;
use std::collections::HashMap;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::sync::Mutex;

static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

/// Canvas widget id ↔ element id. A widget keeps one webview id for the life
/// of the app; `KEY_TO_ID` never shrinks so a reused widget id can't inherit
/// a foreign webview mid-session.
static KEY_TO_ID: std::sync::LazyLock<Mutex<HashMap<String, usize>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static ID_TO_KEY: std::sync::LazyLock<Mutex<HashMap<usize, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// The last URL the GTK side was told to load for an id. The element's paint
/// compares `content` against it so a journaled `widget.state.url` change
/// (address bar, `slate browser navigate`, a refold) re-navigates without
/// double-loading the value the pane already sent.
static SENT_URL: std::sync::LazyLock<Mutex<HashMap<usize, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Everything the chrome paints, mirrored off the GTK thread — the
/// webview's own `url()`/`can_go_back()` are the source after each load.
#[derive(Clone, Debug, Default)]
pub struct WebViewStatus {
    pub url: Option<String>,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    /// Chrome-side failures: invalid input, blocked navigation. WebKitGTK
    /// load errors are not surfaced by wry — see module docs.
    pub error: Option<String>,
}

static STATUS: std::sync::LazyLock<Mutex<HashMap<usize, WebViewStatus>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Things the GTK thread observed that the canvas needs to react to —
/// drained on the 250ms tick by `CanvasView`, not mid-render.
#[derive(Clone, Debug)]
pub enum WebViewEvent {
    /// did-navigate/did-navigate-in-page: a page-initiated navigation that
    /// should be journaled back into `widget.state.url`.
    Navigated { widget: String, url: String },
    /// `window.open` — the shell's `browser:onOpenTab`: the renderer spawned
    /// a centered browser widget for it (subject to its own 3/5s budget).
    OpenTab { widget: String, url: String },
}

static EVENTS: std::sync::LazyLock<Mutex<Vec<WebViewEvent>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

fn lock<'a, T>(
    mutex: &'a std::sync::LazyLock<Mutex<T>>,
) -> std::sync::MutexGuard<'a, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// The stable webview id for a canvas widget id.
pub fn key_for(widget: &str) -> usize {
    let mut map = lock(&KEY_TO_ID);
    if let Some(id) = map.get(widget) {
        return *id;
    }
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    map.insert(widget.to_owned(), id);
    lock(&ID_TO_KEY).insert(id, widget.to_owned());
    id
}

fn widget_for(id: usize) -> Option<String> {
    lock(&ID_TO_KEY).get(&id).cloned()
}

/// Snapshot of what the pane should show for a widget's webview.
pub fn status_for(widget: &str) -> Option<WebViewStatus> {
    let id = lock(&KEY_TO_ID).get(widget).copied()?;
    lock(&STATUS).get(&id).cloned()
}

/// Chrome-driven error state — the equivalent of the original's `setLoadError`
/// for failures the page never got a chance to produce.
pub fn set_error(widget: &str, error: Option<String>) {
    let id = key_for(widget);
    let mut status = lock(&STATUS);
    let entry = status.entry(id).or_default();
    entry.error = error;
    if entry.error.is_none() {
        entry.loading = false;
    }
}

/// WebView-side events since the last drain. CanvasView's tick folds them:
/// Navigated → journaled `set`, OpenTab → a new browser widget.
pub fn drain_events() -> Vec<WebViewEvent> {
    std::mem::take(&mut *lock(&EVENTS))
}

fn push_event(event: WebViewEvent) {
    lock(&EVENTS).push(event);
}

fn update_status(id: usize, f: impl FnOnce(&mut WebViewStatus)) {
    let mut status = lock(&STATUS);
    f(status.entry(id).or_default());
}

/// Record `url` as the URL the GTK side has (or will) load for `id`, then
/// hand the message off — no-op outside Linux.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn send_navigate(id: usize, url: &str) {
    lock(&SENT_URL).insert(id, url.to_owned());
    update_status(id, |status| {
        status.loading = true;
        status.error = None;
    });
    if is_wayland() {
        gtk_thread::send(gtk_thread::GtkMessage::Navigate {
            id,
            url: url.to_owned(),
        });
    } else {
        x11_child_thread::send(x11_child_thread::ChildMessage::Navigate {
            id,
            url: url.to_owned(),
        });
    }
}

/// `view.loadURL(url)` — user/CLI driven navigation to a webview. Normalizes
/// nothing: callers pass the already-validated target.
pub fn navigate(widget: &str, url: &str) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    send_navigate(key_for(widget), url);
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = (widget, url);
}

/// `view.reload()` / the reload half of the stop-or-reload button.
pub fn reload(widget: &str) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let id = key_for(widget);
        update_status(id, |status| {
            status.loading = true;
            status.error = None;
        });
        if is_wayland() {
            gtk_thread::send(gtk_thread::GtkMessage::Reload { id });
        } else {
            x11_child_thread::send(x11_child_thread::ChildMessage::Reload { id });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = widget;
}

/// `view.stop()` — wry exposes no stop; `window.stop()` in-page is the same
/// brake the DOM API applies.
pub fn stop(widget: &str) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let id = key_for(widget);
        update_status(id, |status| status.loading = false);
        if is_wayland() {
            gtk_thread::send(gtk_thread::GtkMessage::Stop { id });
        } else {
            x11_child_thread::send(x11_child_thread::ChildMessage::Stop { id });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = widget;
}

/// `view.goBack()` — a no-op when history is empty, same as Electron's.
pub fn go_back(widget: &str) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let id = key_for(widget);
        if is_wayland() {
            gtk_thread::send(gtk_thread::GtkMessage::Back { id });
        } else {
            x11_child_thread::send(x11_child_thread::ChildMessage::Back { id });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = widget;
}

/// `view.goForward()`.
pub fn go_forward(widget: &str) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        let id = key_for(widget);
        if is_wayland() {
            gtk_thread::send(gtk_thread::GtkMessage::Forward { id });
        } else {
            x11_child_thread::send(x11_child_thread::ChildMessage::Forward { id });
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = widget;
}

/// Hide the widget's native child without unmounting — the canvas culls
/// off-viewport widgets before their element's paint() ever runs, so the
/// child would otherwise stay visible at its last bounds after a jump-cut
/// (focus jump, arrange, zoom-fit). No-op when no webview exists yet.
pub fn hide(widget: &str) {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if !is_wayland() {
        // Lookup only — `key_for` would mint an id for a widget that never
        // had a webview.
        if let Some(id) = lock(&KEY_TO_ID).get(widget).copied() {
            x11_child_thread::request_set_visible(id, false);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = widget;
}

/// The widget is gone — destroy the native webview and all bookkeeping.
/// Called from `CanvasView::close_widget` and the journal-refold GC.
pub fn remove_widget(widget: &str) {
    let id = match lock(&KEY_TO_ID).get(widget).copied() {
        Some(id) => id,
        None => return,
    };
    lock(&KEY_TO_ID).remove(widget);
    lock(&ID_TO_KEY).remove(&id);
    lock(&STATUS).remove(&id);
    lock(&SENT_URL).remove(&id);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        if is_wayland() {
            gtk_thread::remove_created(id);
        } else {
            x11_child_thread::remove_created(id);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    let _ = id;
}

/// Drop webviews whose widgets vanished — `slate canvas remove` and journal
/// refolds never run `close_widget`, so the tick reconciles the key set
/// against the canvas.
pub fn gc(live_widgets: &std::collections::HashSet<String>) {
    let stale: Vec<String> = lock(&KEY_TO_ID)
        .keys()
        .filter(|key| !live_widgets.contains(*key))
        .cloned()
        .collect();
    for key in stale {
        remove_widget(&key);
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").unwrap_or_default() == "Wayland"
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn to_wry_bounds(bounds: Bounds<Pixels>, scale_factor: f32) -> wry::Rect {
    // GPUI bounds are in logical pixels. Convert to physical pixels for
    // the native child window since wry positions relative to the parent
    // window's pixel coordinates.
    let x = (bounds.origin.x.as_f32() * scale_factor) as i32;
    let y = (bounds.origin.y.as_f32() * scale_factor) as i32;
    let width = (bounds.size.width.as_f32() * scale_factor) as u32;
    let height = (bounds.size.height.as_f32() * scale_factor) as u32;
    wry::Rect {
        position: wry::dpi::Position::Physical(wry::dpi::PhysicalPosition::new(x, y)),
        size: wry::dpi::Size::Physical(wry::dpi::PhysicalSize::new(width, height)),
    }
}

/// The will-navigate guard from `windowManager.ts`: about:blank always loads
/// (it is the widget's initial src); http/https anywhere; file: only from a
/// file: page; everything else — data:, javascript: — is refused.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn navigation_allowed(id: usize, url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_lowercase())
        .unwrap_or_default();
    match scheme.as_str() {
        "http" | "https" => true,
        // `SENT_URL` holds the last URL *we* asked the webview to load —
        // matching it means this is a programmatic loadURL, which the
        // original's will-navigate guard exempted outright (that's how the
        // omnibox's file:// worked). Otherwise file: is only reachable from
        // a file: page.
        "file" => {
            lock(&SENT_URL).get(&id).map(String::as_str) == Some(url)
                || lock(&STATUS)
                    .get(&id)
                    .and_then(|status| status.url.as_deref())
                    .map(|current| current.starts_with("file:"))
                    .unwrap_or(false)
        }
        _ => false,
    }
}

/// Wire the three wry hooks onto a builder: the navigation guard + event
/// mirror, the load-state mirror, and the popup policy. Shared by the X11
/// child path and the Wayland window path so both behave identically.
/// `nav_flags` reads (can_go_back, can_go_forward) from whichever
/// thread-local webview store the handler is running on.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn wired_builder(
    id: usize,
    content: &WebViewContent,
    nav_flags: fn(usize) -> (bool, bool),
) -> wry::WebViewBuilder<'static> {
    let builder = match content {
        WebViewContent::Html(html) => wry::WebViewBuilder::new().with_html(html),
        WebViewContent::Url(url) => wry::WebViewBuilder::new().with_url(url),
    };
    builder
        .with_navigation_handler(move |url: String| {
            if !navigation_allowed(id, &url) {
                return false;
            }
            // did-navigate / did-navigate-in-page: persist real pages, never
            // about:blank or data: (the original filtered both out of setUrl).
            if url != "about:blank"
                && !url.starts_with("data:")
                && (url.starts_with("http:")
                    || url.starts_with("https:")
                    || url.starts_with("file:"))
            {
                update_status(id, |status| {
                    status.loading = true;
                    status.url = Some(url.clone());
                });
                // The webview is already loading this URL — record it as
                // sent so `maybe_sync_url` doesn't re-navigate when the
                // journaled state.url lands a tick later (every in-page link
                // click used to load twice).
                lock(&SENT_URL).insert(id, url.clone());
                if let Some(widget) = widget_for(id) {
                    push_event(WebViewEvent::Navigated { widget, url });
                }
            }
            true
        })
        .with_on_page_load_handler(move |event, url| {
            match event {
                wry::PageLoadEvent::Started => update_status(id, |status| {
                    status.loading = true;
                    status.error = None;
                }),
                wry::PageLoadEvent::Finished => {
                    // The webview answers its own history state now.
                    let (can_back, can_forward) = nav_flags(id);
                    update_status(id, |status| {
                        status.loading = false;
                        status.can_back = can_back;
                        status.can_forward = can_forward;
                        if !url.is_empty() {
                            status.url = Some(url);
                        }
                    });
                }
            }
        })
        .with_new_window_req_handler(move |url, _features| {
            // setWindowOpenHandler: every window.open is denied a real
            // window — the shell never created one either. What survives
            // validation, the 350ms same-URL dedup and the 5-per-10s per
            // webview budget is handed to the canvas as an OpenTab event;
            // App.tsx then minted a centered browser widget under its own
            // 3-per-5s global cap.
            if popup_allowed(id, &url) {
                if let Some(widget) = widget_for(id) {
                    push_event(WebViewEvent::OpenTab { widget, url });
                }
            }
            // Deny — never Allow: wry's GTK Allow path unwraps a
            // gtk::ApplicationWindow toplevel we don't have.
            wry::NewWindowResponse::Deny
        })
}

/// windowManager.ts's popup gate, evaluated on the GTK thread: protocol
/// allowlist (file: only from a file: page), no credentials, hostname
/// present, ≤2048 chars, then the dedup + budget counters.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn popup_allowed(id: usize, url: &str) -> bool {
    thread_local! {
        static POPUPS: std::cell::RefCell<HashMap<usize, PopupState>> =
            std::cell::RefCell::new(HashMap::new());
    }
    #[derive(Default)]
    struct PopupState {
        last_at: Option<std::time::Instant>,
        last_url: String,
        budget: std::collections::VecDeque<std::time::Instant>,
    }

    let current_is_file = lock(&STATUS)
        .get(&id)
        .and_then(|status| status.url.as_deref())
        .map(|url| url.starts_with("file:"))
        .unwrap_or(false);
    let lower = url.to_lowercase();
    let scheme_ok = lower.starts_with("https:")
        || lower.starts_with("http:")
        || (lower.starts_with("file:") && current_is_file);
    if !scheme_ok || url.len() > 2048 {
        return false;
    }
    let authority = lower
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    // No credentials; a file: URL may legitimately have an empty host, a web
    // one may not.
    if authority.contains('@') || (authority.is_empty() && !lower.starts_with("file:")) {
        return false;
    }

    let now = std::time::Instant::now();
    POPUPS.with(|popups| {
        let mut popups = popups.borrow_mut();
        let state = popups.entry(id).or_default();
        // Same-URL dedup (350ms) runs before the budget so a repeated click
        // doesn't spend the budget — the original's exact ordering.
        let is_repeat = state
            .last_at
            .map(|at| now.duration_since(at) <= std::time::Duration::from_millis(350))
            .unwrap_or(false)
            && state.last_url == url;
        if is_repeat {
            return false;
        }
        while state
            .budget
            .front()
            .map(|at| now.duration_since(*at) > std::time::Duration::from_secs(10))
            .unwrap_or(false)
        {
            state.budget.pop_front();
        }
        if state.budget.len() >= 5 {
            return false;
        }
        state.budget.push_back(now);
        state.last_at = Some(now);
        state.last_url = url.to_owned();
        true
    })
}

// On X11, wry's build_as_child internally creates GTK widgets that need
// their own event loop. We run this on a dedicated thread to avoid blocking
// GPUI's render loop. The main thread extracts raw window IDs and sends
// them to this thread.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod x11_child_thread {
    use super::*;
    use gtk::glib;

    // wry only accepts Xlib handles but GPUI provides Xcb. The X11 window
    // ID is the same u32 in both, so we store it and present as Xlib.
    struct RawXlibHandle {
        window_id: std::ffi::c_ulong,
        screen: i32,
    }

    // Safety: the window ID and screen are plain integers copied from the
    // main thread. They reference an X11 window that outlives this struct.
    unsafe impl Send for RawXlibHandle {}
    unsafe impl Sync for RawXlibHandle {}

    impl raw_window_handle::HasWindowHandle for RawXlibHandle {
        fn window_handle(
            &self,
        ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError>
        {
            let handle = raw_window_handle::XlibWindowHandle::new(self.window_id);
            Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(handle.into()) })
        }
    }

    impl raw_window_handle::HasDisplayHandle for RawXlibHandle {
        fn display_handle(
            &self,
        ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError>
        {
            let handle = raw_window_handle::XlibDisplayHandle::new(None, self.screen);
            Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(handle.into()) })
        }
    }

    pub enum ChildMessage {
        Create {
            id: usize,
            content: WebViewContent,
            window_id: std::ffi::c_ulong,
            screen: i32,
            bounds: wry::Rect,
        },
        SetBounds {
            id: usize,
            bounds: wry::Rect,
        },
        SetVisible {
            id: usize,
            visible: bool,
        },
        SetZoom {
            id: usize,
            zoom: f64,
        },
        Navigate {
            id: usize,
            url: String,
        },
        Reload {
            id: usize,
        },
        Stop {
            id: usize,
        },
        Back {
            id: usize,
        },
        Forward {
            id: usize,
        },
        Close {
            id: usize,
        },
    }

    static CREATED: std::sync::LazyLock<Mutex<std::collections::HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

    // wry::WebView isn't Send, so we store them on the GTK thread.
    thread_local! {
        static WEBVIEWS: std::cell::RefCell<HashMap<usize, wry::WebView>> =
            std::cell::RefCell::new(HashMap::new());
    }

    /// (can_go_back, can_go_forward) for `wired_builder`'s load handler.
    pub(super) fn nav_flags(id: usize) -> (bool, bool) {
        WEBVIEWS.with(|webviews| {
            webviews
                .try_borrow()
                .ok()
                .and_then(|webviews| {
                    webviews.get(&id).map(|wv| {
                        (
                            wv.can_go_back().unwrap_or(false),
                            wv.can_go_forward().unwrap_or(false),
                        )
                    })
                })
                .unwrap_or_default()
        })
    }

    static CHILD_SENDER: std::sync::LazyLock<Mutex<std::sync::mpsc::Sender<ChildMessage>>> =
        std::sync::LazyLock::new(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<ChildMessage>();

            std::thread::spawn(move || {
                if let Err(error) = gtk::init() {
                    log::error!("X11 child thread: GTK init failed: {}", error);
                    return;
                }

                let receiver = std::cell::RefCell::new(receiver);
                glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
                    while let Ok(message) = receiver.borrow().try_recv() {
                        match message {
                            ChildMessage::Create {
                                id,
                                content,
                                window_id,
                                screen,
                                bounds,
                            } => {
                                create_child(id, content, window_id, screen, bounds);
                            }
                            ChildMessage::SetBounds { id, bounds } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.set_bounds(bounds) {
                                            log::error!(
                                                "WebView #{}: set_bounds failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::SetVisible { id, visible } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.set_visible(visible) {
                                            log::error!(
                                                "WebView #{}: set_visible failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::SetZoom { id, zoom } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.zoom(zoom) {
                                            log::error!(
                                                "WebView #{}: zoom failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::Navigate { id, url } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.load_url(&url) {
                                            log::error!(
                                                "WebView #{}: load_url failed: {}",
                                                id,
                                                error
                                            );
                                            update_status(id, |status| {
                                                status.loading = false;
                                                status.error = Some(error.to_string());
                                            });
                                        }
                                    }
                                });
                            }
                            ChildMessage::Reload { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.reload() {
                                            log::error!(
                                                "WebView #{}: reload failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::Stop { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) =
                                            webview.evaluate_script("window.stop()")
                                        {
                                            log::error!(
                                                "WebView #{}: stop failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::Back { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.go_back() {
                                            log::error!(
                                                "WebView #{}: go_back failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::Forward { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.go_forward() {
                                            log::error!(
                                                "WebView #{}: go_forward failed: {}",
                                                id,
                                                error
                                            );
                                        }
                                    }
                                });
                            }
                            ChildMessage::Close { id } => {
                                WEBVIEWS.with(|webviews| {
                                    webviews.borrow_mut().remove(&id);
                                });
                            }
                        }
                    }
                    glib::ControlFlow::Continue
                });

                gtk::main();
            });

            Mutex::new(sender)
        });

    fn create_child(
        id: usize,
        content: WebViewContent,
        window_id: std::ffi::c_ulong,
        screen: i32,
        bounds: wry::Rect,
    ) {
        let handle = RawXlibHandle { window_id, screen };

        match wired_builder(id, &content, nav_flags)
            .with_bounds(bounds)
            .with_transparent(true)
            .build_as_child(&handle)
        {
            Ok(webview) => {
                log::info!("WebView #{} created (X11 child)", id);
                WEBVIEWS.with(|webviews| {
                    webviews.borrow_mut().insert(id, webview);
                });
            }
            Err(error) => {
                log::error!("WebView #{}: build_as_child failed: {}", id, error);
                update_status(id, |status| {
                    status.loading = false;
                    status.error = Some(format!("webview failed to start: {error}"));
                });
            }
        }
    }

    pub fn send(message: ChildMessage) {
        let sender = CHILD_SENDER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = sender.send(message) {
            log::error!("webview: failed to send to X11 child thread: {}", error);
        }
    }

    pub fn request_create(
        id: usize,
        content: &WebViewContent,
        bounds: wry::Rect,
        window: &Window,
    ) {
        let mut created = CREATED.lock().unwrap_or_else(|poison| poison.into_inner());
        if created.contains(&id) {
            return;
        }
        created.insert(id);
        drop(created);

        let (window_id, screen) = match raw_window_handle::HasWindowHandle::window_handle(window) {
            Ok(handle) => match handle.as_raw() {
                raw_window_handle::RawWindowHandle::Xcb(xcb) => {
                    (xcb.window.get() as std::ffi::c_ulong, 0i32)
                }
                _ => {
                    log::error!("WebView #{}: expected Xcb window handle", id);
                    return;
                }
            },
            Err(error) => {
                log::error!("WebView #{}: failed to get window handle: {}", id, error);
                return;
            }
        };

        if let WebViewContent::Url(url) = content {
            lock(&SENT_URL).insert(id, url.clone());
        }
        send(ChildMessage::Create {
            id,
            content: content.clone(),
            window_id,
            screen,
            bounds,
        });
    }

    // Track last bounds/zoom per webview on the main thread to avoid sending
    // redundant messages every paint frame.
    static LAST_BOUNDS: std::sync::LazyLock<Mutex<HashMap<usize, Bounds<Pixels>>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    static LAST_ZOOM: std::sync::LazyLock<Mutex<HashMap<usize, f64>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    static LAST_VISIBLE: std::sync::LazyLock<Mutex<HashMap<usize, bool>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Show/hide the native child. Only meaningful once the webview exists —
    /// a hidden-before-create webview gets its initial visibility from the
    /// paint path that next creates it, so this is a no-op pre-create.
    pub fn request_set_visible(id: usize, visible: bool) {
        {
            let created = CREATED.lock().unwrap_or_else(|poison| poison.into_inner());
            if !created.contains(&id) {
                return;
            }
        }
        let mut last = LAST_VISIBLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.get(&id) == Some(&visible) {
            return;
        }
        last.insert(id, visible);
        drop(last);
        send(ChildMessage::SetVisible { id, visible });
    }

    pub fn request_set_bounds(id: usize, bounds: Bounds<Pixels>, scale_factor: f32) {
        let mut last = LAST_BOUNDS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.get(&id) == Some(&bounds) {
            return;
        }
        last.insert(id, bounds);
        drop(last);
        send(ChildMessage::SetBounds {
            id,
            bounds: super::to_wry_bounds(bounds, scale_factor),
        });
    }

    pub fn request_zoom(id: usize, zoom: f64) {
        let mut last = LAST_ZOOM
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if last.get(&id) == Some(&zoom) {
            return;
        }
        last.insert(id, zoom);
        drop(last);
        send(ChildMessage::SetZoom { id, zoom });
    }

    /// The element's content↔URL reconciliation: when the journaled
    /// `widget.state.url` drifts from what the GTK side was last told to
    /// load, send the new one. Explicit `navigate()` calls update SENT_URL
    /// too, so the element never double-loads.
    pub fn maybe_sync_url(id: usize, content: &WebViewContent) {
        let WebViewContent::Url(url) = content else {
            return;
        };
        let needs = lock(&SENT_URL).get(&id) != Some(url);
        if needs {
            lock(&SENT_URL).insert(id, url.clone());
            send(ChildMessage::Navigate {
                id,
                url: url.clone(),
            });
        }
    }

    pub fn remove_created(id: usize) {
        CREATED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        LAST_BOUNDS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        LAST_ZOOM
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        LAST_VISIBLE
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        send(ChildMessage::Close { id });
    }
}

// On Linux/Wayland, build_as_child is unsupported — each webview is a
// separate GTK window on the same dedicated GTK thread. Behavior matches the
// vendored element; the chrome stays inside the canvas.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod gtk_thread {
    use super::*;
    use gtk::glib;
    use gtk::prelude::*;
    use wry::WebViewBuilderExtUnix;

    pub enum GtkMessage {
        Create { id: usize, content: WebViewContent },
        Navigate { id: usize, url: String },
        Reload { id: usize },
        Stop { id: usize },
        Back { id: usize },
        Forward { id: usize },
        Close { id: usize },
    }

    static CREATED: std::sync::LazyLock<Mutex<std::collections::HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

    // gtk::Window and wry::WebView aren't Send, so they live on the GTK
    // thread; the webview is kept alive as long as the window exists.
    thread_local! {
        static WINDOWS: std::cell::RefCell<HashMap<usize, gtk::Window>> =
            std::cell::RefCell::new(HashMap::new());
        static WEBVIEWS: std::cell::RefCell<HashMap<usize, wry::WebView>> =
            std::cell::RefCell::new(HashMap::new());
    }

    /// (can_go_back, can_go_forward) for `wired_builder`'s load handler.
    fn nav_flags(id: usize) -> (bool, bool) {
        WEBVIEWS.with(|webviews| {
            webviews
                .try_borrow()
                .ok()
                .and_then(|webviews| {
                    webviews.get(&id).map(|wv| {
                        (
                            wv.can_go_back().unwrap_or(false),
                            wv.can_go_forward().unwrap_or(false),
                        )
                    })
                })
                .unwrap_or_default()
        })
    }

    static GTK_SENDER: std::sync::LazyLock<Mutex<std::sync::mpsc::Sender<GtkMessage>>> =
        std::sync::LazyLock::new(|| {
            let (sender, receiver) = std::sync::mpsc::channel::<GtkMessage>();

            std::thread::spawn(move || {
                if let Err(error) = gtk::init() {
                    log::error!("Failed to initialize GTK: {}", error);
                    return;
                }

                let receiver = std::cell::RefCell::new(receiver);
                glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                    while let Ok(message) = receiver.borrow().try_recv() {
                        match message {
                            GtkMessage::Create { id, content } => create_window(id, content),
                            GtkMessage::Navigate { id, url } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        if let Err(error) = webview.load_url(&url) {
                                            update_status(id, |status| {
                                                status.loading = false;
                                                status.error = Some(error.to_string());
                                            });
                                        }
                                    }
                                });
                            }
                            GtkMessage::Reload { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        let _ = webview.reload();
                                    }
                                });
                            }
                            GtkMessage::Stop { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        let _ = webview.evaluate_script("window.stop()");
                                    }
                                });
                            }
                            GtkMessage::Back { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        let _ = webview.go_back();
                                    }
                                });
                            }
                            GtkMessage::Forward { id } => {
                                WEBVIEWS.with(|webviews| {
                                    if let Some(webview) = webviews.borrow().get(&id) {
                                        let _ = webview.go_forward();
                                    }
                                });
                            }
                            GtkMessage::Close { id } => close_window(id),
                        }
                    }
                    glib::ControlFlow::Continue
                });

                gtk::main();
            });

            Mutex::new(sender)
        });

    fn create_window(id: usize, content: WebViewContent) {
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title("Slate Browser");
        window.set_default_size(900, 700);

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(true);
        window.add(&container);
        window.show_all();

        match wired_builder(id, &content, nav_flags).build_gtk(&container) {
            Ok(webview) => {
                log::info!("WebView #{} created (Wayland/GTK)", id);
                WEBVIEWS.with(|webviews| {
                    webviews.borrow_mut().insert(id, webview);
                });
            }
            Err(error) => {
                log::error!("WebView #{}: failed: {}", id, error);
                update_status(id, |status| {
                    status.loading = false;
                    status.error = Some(format!("webview failed to start: {error}"));
                });
                window.close();
                return;
            }
        }

        let webview_id = id;
        window.connect_delete_event(move |_, _| {
            let was_tracked =
                WINDOWS.with(|windows| windows.borrow_mut().remove(&webview_id).is_some());
            if was_tracked {
                WEBVIEWS.with(|webviews| {
                    webviews.borrow_mut().remove(&webview_id);
                });
            }
            glib::Propagation::Proceed
        });

        WINDOWS.with(|windows| {
            windows.borrow_mut().insert(id, window);
        });
    }

    fn close_window(id: usize) {
        WINDOWS.with(|windows| {
            if let Some(window) = windows.borrow_mut().remove(&id) {
                WEBVIEWS.with(|webviews| {
                    webviews.borrow_mut().remove(&id);
                });
                window.close();
            }
        });
    }

    pub fn send(message: GtkMessage) {
        let sender = GTK_SENDER
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = sender.send(message) {
            log::error!("webview: failed to send to GTK thread: {}", error);
        }
    }

    pub fn remove_created(id: usize) {
        CREATED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        send(GtkMessage::Close { id });
    }

    pub fn ensure_created(id: usize, content: &WebViewContent) {
        let mut created = CREATED.lock().unwrap_or_else(|poison| poison.into_inner());
        if created.contains(&id) {
            return;
        }
        created.insert(id);
        drop(created);
        if let WebViewContent::Url(url) = content {
            lock(&SENT_URL).insert(id, url.clone());
        }
        send(GtkMessage::Create {
            id,
            content: content.clone(),
        });
    }

    pub fn maybe_sync_url(id: usize, content: &WebViewContent) {
        let WebViewContent::Url(url) = content else {
            return;
        };
        let needs = lock(&SENT_URL).get(&id) != Some(url);
        if needs {
            lock(&SENT_URL).insert(id, url.clone());
            send(GtkMessage::Navigate {
                id,
                url: url.clone(),
            });
        }
    }
}

/// A WebView element for rendering HTML content.
///
/// Each webview is identified by a unique ID derived from the canvas widget
/// id. The native webview is created once and persists across GPUI render
/// cycles; its bounds follow whatever rect the layout hands it.
pub struct WebView {
    id: usize,
    content: WebViewContent,
    zoom: f32,
    /// Window-space clip rect — the canvas content region the caller knows
    /// floating chrome (title bar, rail, status strip) overlays. Intersected
    /// with the paint-time `content_mask()` before the native child's bounds
    /// are set, since X11 children are not clipped by GPUI compositing.
    clip: Option<Bounds<Pixels>>,
    style: StyleRefinement,
}

/// Content to display in a webview.
#[derive(Clone)]
pub enum WebViewContent {
    /// Raw HTML string.
    Html(String),
    /// URL to navigate to.
    Url(String),
}

impl WebView {
    /// A URL webview owned by the given canvas widget id.
    pub fn for_url(widget: &str, url: impl Into<String>) -> Self {
        Self {
            id: key_for(widget),
            content: WebViewContent::Url(url.into()),
            zoom: 1.0,
            clip: None,
            style: Default::default(),
        }
    }

    /// The canvas camera zoom — forwarded to WebKitGTK's page zoom so the
    /// web content scales with the widget the way Electron's CSS transform
    /// scaled it.
    pub fn zoom(mut self, zoom: f32) -> Self {
        self.zoom = zoom;
        self
    }

    /// The window-space rect the native child is clipped to — the canvas
    /// region not covered by floating chrome. Pass a degenerate rect while a
    /// full-screen overlay (splash, settings) owns the window and the child
    /// is hidden instead of bleeding over it.
    pub fn clip(mut self, clip: Bounds<Pixels>) -> Self {
        self.clip = Some(clip);
        self
    }
}

impl Element for WebView {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        if matches!(style.size.width, gpui::Length::Auto) {
            style.size.width = gpui::Length::Definite(px(400.0).into());
        }
        if matches!(style.size.height, gpui::Length::Auto) {
            style.size.height = gpui::Length::Definite(px(300.0).into());
        }
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            // A degenerate rect is mid-layout or a zero-size widget — no
            // native window should be created or moved for it.
            if bounds.size.width <= px(1.0) || bounds.size.height <= px(1.0) {
                return;
            }
            if is_wayland() {
                gtk_thread::ensure_created(self.id, &self.content);
                gtk_thread::maybe_sync_url(self.id, &self.content);
            } else {
                // X11 children aren't composited: clip the layout bounds to
                // the paint-time content mask (viewport, widget overflow)
                // and the caller's chrome-free canvas region. A webview
                // clipped to nothing hides rather than painting over the
                // title bar, rail, splash or settings modal.
                let mut clipped = bounds.intersect(&window.content_mask().bounds);
                if let Some(clip) = self.clip {
                    clipped = clipped.intersect(&clip);
                }
                if clipped.size.width <= px(1.0) || clipped.size.height <= px(1.0) {
                    x11_child_thread::request_set_visible(self.id, false);
                    return;
                }
                let scale_factor = window.scale_factor();
                x11_child_thread::request_create(
                    self.id,
                    &self.content,
                    to_wry_bounds(clipped, scale_factor),
                    window,
                );
                x11_child_thread::request_set_bounds(self.id, clipped, scale_factor);
                x11_child_thread::request_set_visible(self.id, true);
                x11_child_thread::request_zoom(self.id, f64::from(self.zoom));
                x11_child_thread::maybe_sync_url(self.id, &self.content);
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        {
            let _ = (bounds, window);
        }
    }
}

impl IntoElement for WebView {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Styled for WebView {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}
