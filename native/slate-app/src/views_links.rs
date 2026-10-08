//! Links pane — the read half of the old Electron `LinksWidget`.
//!
//! The original kept each links widget's list in localStorage under
//! `orcspace-links:<widgetId>` (`[{id, title, url}]`, capped at 200, unsafe
//! URL schemes dropped on read). The native mirror of that store is
//! `workspace-links.json` in the user data dir:
//! `{ "<widgetId>": [ {"id": "...", "title": "...", "url": "..."} ] }`.
//! A top-level bare array is also accepted — same shape, unscoped.
//!
//! Rows are title + display host/path, marked `↗` for the absolute schemes
//! `safeHref` could open (http/https/mailto/tel) and `·` for the relative
//! paths the original listed but could not browse to. The pane writes the
//! file directly: `+` appends a new entry, `×` removes one, and clicking a
//! row hands http/https URLs to `xdg-open`.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

pub struct LinkRow {
    pub id: String,
    pub title: String,
    pub url: String,
    /// http(s) — the original's `isSafeUrl` gate for the click-through path.
    pub web: bool,
}

fn links_path() -> std::path::PathBuf {
    slate_app::ipc::user_data_dir().join("workspace-links.json")
}

fn now_ms_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// The document on disk — either the scoped `{widgetId: [...]}` object or
/// the bare `[...]` the reader also accepts.
fn read_document() -> Option<Value> {
    let document = slate_app::ipc::read_store_recovered(&links_path())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())?;
    if document.is_object() || document.is_array() {
        Some(document)
    } else {
        None
    }
}

/// `writeAtomic` — temp, fsync, .bak of the previous good copy, rename.
fn write_document(document: &Value) -> Result<(), String> {
    let path = links_path();
    let bytes = slate_app::jsjson::to_js_json_pretty(document, 2).into_bytes();
    slate_app::ipc::write_file_atomic(&path, &bytes).map_err(|e| e.to_string())
}

/// The list this widget edits: `document[widget_id]` in the scoped form,
/// the root array itself for the bare form.
fn widget_entries<'a>(document: &'a mut Value, widget_id: &str) -> &'a mut Vec<Value> {
    if document.is_array() {
        return document.as_array_mut().expect("checked above");
    }
    if !document.is_object() {
        *document = json!({});
    }
    let entry = &mut document[widget_id];
    if !entry.is_array() {
        *entry = json!([]);
    }
    entry.as_array_mut().expect("normalized above")
}

/// Append a placeholder link — the Electron widget's add form needed a
/// title and URL; the canvas has no text input, so the new row is a stub
/// that is one file edit (or a future `link.update`) away from real.
fn add_link(widget_id: &str) -> Result<(), String> {
    let mut document = read_document().unwrap_or_else(|| json!({}));
    let id = format!("link-{}", now_ms_f() as u64);
    widget_entries(&mut document, widget_id).push(json!({
        "id": id,
        "title": "New link",
        "url": "https://example.com",
    }));
    write_document(&document)
}

fn remove_link(widget_id: &str, id: &str) -> Result<(), String> {
    let Some(mut document) = read_document() else {
        return Ok(());
    };
    widget_entries(&mut document, widget_id)
        .retain(|entry| entry.get("id").and_then(Value::as_str) != Some(id));
    write_document(&document)
}

/// `looksDangerous`: control/space-stripped, lowercased prefix sniff —
/// catches `java\tscript:` tricks and plain `javascript:`/`data:`/`vbscript:`.
fn looks_dangerous(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|c| !c.is_ascii_control() && !c.is_whitespace() && *c != '\u{7f}')
        .collect::<String>()
        .to_lowercase();
    compact.starts_with("javascript:")
        || compact.starts_with("data:")
        || compact.starts_with("vbscript:")
}

/// Percent-decode for the second sniff pass (the original tried
/// `decodeURIComponent` then `decodeURI` before comparing). Non-UTF8 and
/// truncated escapes fall back to the raw text.
fn decoded_for_sniff(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_owned())
}

/// The scheme run the original's `normalizeProtocol` extracted:
/// `^[a-zA-Z][a-zA-Z0-9+.-]*\s*:` → lowercased `name:`.
fn scheme_of(value: &str) -> Option<String> {
    let mut chars = value.trim_start().chars().peekable();
    if !chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut name = String::new();
    for c in chars.by_ref() {
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') || c.is_whitespace() {
            name.push(c);
            continue;
        }
        if c == ':' {
            let name: String = name.chars().filter(|c| !c.is_whitespace()).collect();
            if name.is_empty() {
                return None;
            }
            return Some(format!("{}:", name.to_lowercase()));
        }
        return None;
    }
    None
}

/// `isSafeUrl` (sanitizeUrl.ts): `javascript:`/`data:`/`vbscript:`/`file:`/
/// `blob:` and any other scheme outside `http:`/`https:`/`mailto:`/`tel:` is
/// rejected; relative addresses (`/`, `./`, `../`, `#`, or no scheme) pass —
/// `allowRelative` held true in the widget's renderer. Rows that fail are
/// dropped, not shown-disabled, exactly like `readLinks`.
fn is_safe_url(url: &str) -> bool {
    let trimmed: String = url
        .trim()
        .chars()
        .filter(|c| !c.is_ascii_control() && *c != '\u{7f}')
        .collect();
    if trimmed.is_empty() {
        return false;
    }
    if looks_dangerous(&trimmed) || looks_dangerous(&decoded_for_sniff(&trimmed)) {
        return false;
    }
    if trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return true;
    }
    // `file%3a/etc/passwd` has no raw scheme and would pass as relative — the
    // original re-extracted the scheme from the *decoded* string and checked
    // it against BLOCKED_PROTOCOLS, which is what catches it.
    if let Some(decoded) = scheme_of(&decoded_for_sniff(&trimmed)) {
        if matches!(
            decoded.as_str(),
            "javascript:" | "data:" | "vbscript:" | "file:" | "blob:"
        ) || looks_dangerous(&decoded)
        {
            return false;
        }
    }
    match scheme_of(&trimmed).as_deref() {
        // No scheme at all → a plain relative path/name.
        None => true,
        Some(proto) => matches!(proto, "http:" | "https:" | "mailto:" | "tel:"),
    }
}

/// `displayUrl` from the widget: `hostname + pathname` with a bare `/`
/// dropped, so `https://example.com/` reads as `example.com`.
fn display_url(url: &str) -> String {
    let rest = url
        .trim()
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| url.trim());
    // host up to the first `/`, `?` or `#`; path stops at query/fragment.
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(host_end);
    let path = tail.split(['?', '#']).next().unwrap_or_default();
    if path.is_empty() || path == "/" {
        host.to_owned()
    } else {
        format!("{host}{path}")
    }
}

/// This widget's slice of `workspace-links.json`.
pub fn link_rows(widget_id: &str) -> Vec<LinkRow> {
    let path = slate_app::ipc::user_data_dir().join("workspace-links.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    // Scoped `{id: [...]}` form, with a bare `[...]` accepted for hand-written
    // files — the original key namespaced one list per widget, this file
    // groups them under one root instead.
    let entries: &[serde_json::Value] = match document.get(widget_id).and_then(|v| v.as_array()) {
        Some(entries) => entries,
        None => document.as_array().map(Vec::as_slice).unwrap_or(&[]),
    };
    entries
        .iter()
        .take(200)
        .filter_map(|entry| {
            // The original's reader required id+title+url and dropped rows it
            // could not shape.
            let id = entry.get("id").and_then(|v| v.as_str())?;
            let title = entry.get("title").and_then(|v| v.as_str())?;
            let url = entry.get("url").and_then(|v| v.as_str())?;
            // The original only type-checked the three fields and dropped
            // on `isSafeUrl` — an empty *title* rendered as a blank line.
            if !is_safe_url(url) {
                return None;
            }
            Some(LinkRow {
                id: id.to_owned(),
                title: title.to_owned(),
                url: url.to_owned(),
                // `web` flags the rows `safeHref` could open — any allowed
                // absolute scheme. Relative/path rows render `·`, the
                // original's "Cannot open this path in browser" state.
                web: matches!(
                    scheme_of(url).as_deref(),
                    Some("http:") | Some("https:") | Some("mailto:") | Some("tel:")
                ),
            })
        })
        .collect()
}

fn link_row<'a>(
    row: &'a LinkRow,
    widget_id: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    // Only http/https open on click — mailto/tel/relative rows keep the
    // original's listed-but-inert treatment.
    let openable = matches!(
        scheme_of(&row.url).as_deref(),
        Some("http:") | Some("https:")
    );
    let url = row.url.clone();
    let wid = widget_id.to_owned();
    let link_id = row.id.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .border_1()
        .border_color(rgb(theme::hex(theme::hairline::SOFT)))
        .px_2()
        .py_1()
        .when(openable, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, _event, _window, cx| {
                        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                        cx.notify();
                    }),
                )
        })
        .child(
            div()
                .text_xs()
                .flex_none()
                .text_color(rgb(theme::hex(if row.web {
                    theme::text::NORMAL
                } else {
                    theme::text::FAINT
                })))
                .child(if row.web { "↗" } else { "·" }),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .truncate()
                        .child(row.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .truncate()
                        .child(display_url(&row.url)),
                ),
        )
        .child(
            div()
                .flex_none()
                .px_1()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .cursor_pointer()
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                        .text_color(rgb(theme::hex(theme::status::DANGER)))
                })
                .child("×")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, _event, _window, cx| {
                        // Keep the row's open handler from also firing on
                        // the same press.
                        cx.stop_propagation();
                        let _ = remove_link(&wid, &link_id);
                        cx.notify();
                    }),
                ),
        )
}

pub fn links_pane(
    widget_id: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let rows = link_rows(widget_id);
    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .p_1()
        .size_full()
        .overflow_hidden();
    {
        let wid = widget_id.to_owned();
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!("{} link(s)", rows.len())),
                )
                .child(
                    div()
                        .px_1()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                        .child("+")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_this, _event, _window, cx| {
                                let _ = add_link(&wid);
                                cx.notify();
                            }),
                        ),
                ),
        );
    }
    // Rows scroll under the pinned count/+ header.
    let mut list = div()
        .id(format!("slate-links-list-{}", widget_id))
        .flex()
        .flex_col()
        .gap_1()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    if rows.is_empty() {
        list = list.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child("No links saved yet — + adds one"),
        );
    }
    for row in &rows {
        list = list.child(link_row(row, widget_id, cx));
    }
    col.child(list)
}
