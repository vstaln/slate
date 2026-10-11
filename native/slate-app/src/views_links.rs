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
//! paths the original listed but could not browse to. `+` and `×` are
//! journaled writes: `CanvasView::widget_command` posts to `/links/{widget}`
//! (`links_route` in http.rs), which journals `link.*` and then writes the
//! file. Clicking a row hands http/https URLs to `xdg-open`.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::json;
use slate_app::http::{is_safe_url, scheme_of};
use slate_app::theme;

pub struct LinkRow {
    pub id: String,
    pub title: String,
    pub url: String,
    /// http(s) — the original's `isSafeUrl` gate for the click-through path.
    pub web: bool,
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
                    cx.listener(move |this, _event, _window, cx| {
                        // Keep the row's open handler from also firing on
                        // the same press.
                        cx.stop_propagation();
                        // fix-panes2: journaled `DELETE /links/{widget}/{id}`.
                        this.widget_command(
                            &wid,
                            json!({ "op": "link_remove", "id": link_id }),
                            cx,
                        );
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
                            cx.listener(move |this, _event, _window, cx| {
                                // fix-panes2: journaled `POST /links/{widget}`.
                                this.widget_command(&wid, json!({ "op": "link_add" }), cx);
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
