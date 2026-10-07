//! Notes pane — the read half of the old Electron `NotesWidget`.
//!
//! Data source: `workspace-notes.json` in the user data dir, the file the
//! Electron `NotesStore` (src/main/notesStore.ts) debounce-persisted:
//! `{ "schemaVersion": 1, "items": [NoteItem], "categoryColors": {name: "#rrggbb"} }`
//! where `NoteItem` is `{id, title, body, tags, category?, color, createdBy,
//! order, createdAt, updatedAt, version}` and `color` is always resolved to
//! `#rrggbb` (own color, else the category's, else the palette default).
//!
//! The native command plane has no `note.*` handlers yet, so the pane's
//! add/delete affordances write `workspace-notes.json` directly — the same
//! file the Electron `NotesStore` debounce-persisted. Notes written by the
//! Electron build — or by anything else that speaks the file format — show
//! up here on the next render, and vice versa.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

/// The color a note with no category wears (`DEFAULT_NOTE_COLOR`,
/// src/shared/noteColors.ts — first palette entry).
const DEFAULT_COLOR: u32 = 0x7aa2f7;

/// One display row. The store carries more (order, timestamps, version);
/// the pane keeps the id so select/delete can address the note.
pub struct NoteRow {
    pub id: String,
    pub title: String,
    pub preview: String,
    pub body: String,
    pub color: u32,
    pub category: Option<String>,
    pub tags: Vec<String>,
}

/// Epoch milliseconds — `createdAt`/`updatedAt`/`id` parts in the store.
fn now_ms_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

fn notes_path() -> std::path::PathBuf {
    slate_app::ipc::user_data_dir().join("workspace-notes.json")
}

/// The whole document, or the schema the store would have booted into.
/// A corrupt file is quarantined and the `.bak` tried instead — falling
/// back to defaults AND THEN SAVING would silently destroy the user's
/// notes, which is exactly what the shell's readStoreJson was built
/// against.
fn read_document() -> Value {
    let document = slate_app::ipc::read_store_recovered(&notes_path())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match document {
        Some(document) if document.is_object() => document,
        _ => json!({ "schemaVersion": 1, "items": [], "categoryColors": {} }),
    }
}

/// `writeAtomic` — temp, fsync, .bak of the previous good copy, rename.
fn write_document(document: &Value) -> Result<(), String> {
    let path = notes_path();
    let bytes = slate_app::jsjson::to_js_json_pretty(document, 2).into_bytes();
    slate_app::ipc::write_file_atomic(&path, &bytes).map_err(|e| e.to_string())
}

/// Append a blank note (`revive()` in the Electron store required id +
/// non-empty title) and return its generated id, or the failure text.
fn add_note() -> Result<String, String> {
    let mut document = read_document();
    let now = now_ms_f();
    let id = format!(
        "note-{now}-{id}",
        id = &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let items = document
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // `NotesStore.nextOrder` — max+1 over surviving items, not `len`, which
    // collides after a delete.
    let order = items
        .iter()
        .filter_map(|item| item.get("order").and_then(Value::as_f64))
        .fold(-1.0, f64::max)
        + 1.0;
    let mut items = items;
    // `color` is always resolved in the store — no category here, so the
    // palette default applies (DEFAULT_NOTE_COLOR).
    items.push(json!({
        "id": id,
        "title": "New note",
        "body": "",
        "tags": [],
        "color": format!("#{DEFAULT_COLOR:06x}"),
        "createdBy": "slate",
        "order": order,
        "createdAt": now,
        "updatedAt": now,
        "version": 1,
    }));
    document["items"] = Value::Array(items);
    write_document(&document)?;
    Ok(id)
}

/// Remove one note by id; unknown ids are a no-op success.
fn remove_note(id: &str) -> Result<(), String> {
    let mut document = read_document();
    let mut items = document
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items.retain(|entry| entry.get("id").and_then(Value::as_str) != Some(id));
    document["items"] = Value::Array(items);
    write_document(&document)
}

fn note_color(value: Option<&serde_json::Value>) -> Option<u32> {
    let text = value?.as_str()?;
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Notes as `(rows, error)` — items sorted by `updatedAt` descending, the
/// order the Electron widget used once a category filter or none applied.
/// A missing file is empty rather than an error; an unreadable or malformed
/// one surfaces a one-line message the way the planner pane does.
pub fn note_rows() -> (Vec<NoteRow>, Option<String>) {
    let path = slate_app::ipc::user_data_dir().join("workspace-notes.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Vec::new(), None);
        }
        Err(error) => return (Vec::new(), Some(error.to_string())),
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (
            Vec::new(),
            Some("unreadable workspace-notes.json".to_owned()),
        );
    };
    let category_colors = document
        .get("categoryColors")
        .and_then(serde_json::Value::as_object);
    let color_for = |category: Option<&str>| -> Option<u32> {
        category
            .and_then(|name| category_colors.and_then(|map| map.get(name)))
            .and_then(|value| note_color(Some(value)))
    };

    let mut items: Vec<(f64, NoteRow)> = Vec::new();
    for entry in document
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
    {
        // revive() in NotesStore drops items with no id or an empty title.
        let title = entry
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if title.is_empty() {
            continue;
        }
        let body = entry
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // line-clamp-2 in the original: the preview is one squashed line here.
        let preview: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview: String = preview.chars().take(140).collect();
        let category = entry
            .get("category")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let color = note_color(entry.get("color"))
            .or_else(|| color_for(category.as_deref()))
            .unwrap_or(DEFAULT_COLOR);
        let tags = entry
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect();
        let updated_at = entry
            .get("updatedAt")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        items.push((
            updated_at,
            NoteRow {
                id: id.to_owned(),
                title: title.to_owned(),
                preview,
                body,
                color,
                category,
                tags,
            },
        ));
    }
    items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    (items.into_iter().map(|(_, row)| row).collect(), None)
}

/// A note card: 3px color rail on the left (the original's
/// `borderLeft: 3px solid <color>`), title, squashed body preview — or the
/// full body when `expanded` — then a category chip and `#tag` chips when
/// the note has any. Clicking toggles the card into `widget.state.expanded`;
/// an expanded card also shows a × that drops the note from the store file.
fn note_card<'a>(
    row: &'a NoteRow,
    expanded: bool,
    widget_id: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement + 'a {
    let mut card = div()
        .flex()
        .flex_row()
        .gap_2()
        .overflow_hidden()
        .py_1()
        .cursor_pointer()
        .when(expanded, |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED))).px_1()
        });
    // The color rail: a fixed-width strip, rounded on both ends.
    card = card.child(
        div()
            .w(px(3.0))
            .flex_none()
            .bg(rgb(row.color)),
    );

    // Click selects/deselects the note into the widget's journaled state.
    let expanded_value = if expanded { Value::Null } else { json!(row.id) };
    let toggle = json!({ "op": "set", "state": { "expanded": expanded_value } });
    let wid_toggle = widget_id.to_owned();
    let wid_delete = widget_id.to_owned();
    let toggle_handler = cx.listener(move |this, _event, _window, cx| {
        this.widget_command(&wid_toggle, toggle.clone(), cx);
    });

    let mut title_row = div().flex().flex_row().items_center().gap_1().child(
        div()
            .flex_1()
            .min_w_0()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .truncate()
            .child(row.title.clone()),
    );
    if expanded {
        let note_id = row.id.clone();
        title_row = title_row.child(
            div()
                .flex_none()
                .px_1()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
                        .text_color(rgb(theme::hex(theme::status::DANGER)))
                })
                .child("×")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        // The card below is clickable too — stop the bubble
                        // so delete doesn't also re-select the dead note.
                        cx.stop_propagation();
                        if remove_note(&note_id).is_ok() {
                            this.widget_command(
                                &wid_delete,
                                json!({ "op": "set", "state": { "expanded": Value::Null } }),
                                cx,
                            );
                        }
                        cx.notify();
                    }),
                ),
        );
    }
    let mut body = div().flex().flex_col().min_w_0().flex_1().child(title_row);
    if expanded && !row.body.trim().is_empty() {
        body = body.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(row.body.clone()),
        );
    } else if !row.preview.is_empty() {
        body = body.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .truncate()
                .child(row.preview.clone()),
        );
    }
    if row.category.is_some() || !row.tags.is_empty() {
        let mut meta = div().flex().flex_row().flex_wrap().gap_1().pt(px(2.0));
        if let Some(category) = &row.category {
            meta = meta.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                    .px_2()
                    .child(div().size(px(5.0)).rounded_full().bg(rgb(row.color)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::hex(theme::text::DIM)))
                            .child(category.clone()),
                    ),
            );
        }
        for tag in &row.tags {
            meta = meta.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(format!("#{tag}")),
            );
        }
        body = body.child(meta);
    }
    card.child(body)
        .on_mouse_down(MouseButton::Left, toggle_handler)
}

pub fn notes_pane(
    widget: &slate_app::projection::Widget,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let (rows, error) = note_rows();
    let expanded = widget
        .state
        .as_ref()
        .and_then(|state| state.get("expanded"))
        .and_then(Value::as_str);
    let wid = widget.id.clone();

    let mut col = div()
        .flex()
        .flex_col()
        .gap_1()
        .size_full()
        .overflow_hidden();
    // Header row: count on the left, + adds a blank note (writing the store
    // file directly) and expands it for the follow-up edit.
    col = col.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .px_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child(format!("{} note(s)", rows.len())),
            )
            .child({
                let wid = wid.clone();
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
                            if let Ok(id) = add_note() {
                                this.widget_command(
                                    &wid,
                                    json!({ "op": "set", "state": { "expanded": id } }),
                                    cx,
                                );
                            } else {
                                cx.notify();
                            }
                        }),
                    )
            }),
    );
    // The cards scroll under the pinned header; an expanded card's body
    // text rides along inside its card.
    let mut list = div()
        .id(format!("slate-notes-list-{}", wid))
        .flex()
        .flex_col()
        .gap_1()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    if let Some(error) = error {
        list = list.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::status::DANGER)))
                .child(format!("notes: {error}")),
        );
    }
    if rows.is_empty() {
        list = list.child(
            div()
                .text_sm()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child("No notes yet — + creates one"),
        );
    }
    for row in &rows {
        list = list.child(note_card(row, expanded == Some(row.id.as_str()), &wid, cx));
    }
    col.child(list)
}
