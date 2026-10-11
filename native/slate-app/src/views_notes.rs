//! Notes pane — the Rust port of the Electron notes widget.
//!
//! Data source: `workspace-notes.json` in the user data dir, the file the
//! Electron `NotesStore` (src/main/notesStore.ts) debounce-persisted:
//! `{ "schemaVersion": 1, "items": [NoteItem], "categoryColors": {name: "#rrggbb"} }`
//! where `NoteItem` is `{id, title, body, tags, category?, color, createdBy,
//! order, createdAt, updatedAt, version}` and `color` is always resolved to
//! `#rrggbb` (own color, else the category's, else the palette default).
//!
//! Every pane write goes through the same `note.*` journal ops the REST
//! routes commit (`notes_route` in http.rs): `CanvasView::widget_command`
//! posts to `/notes` over the control socket, so the journal entry and the
//! file write happen in one place and the pane never touches the file.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;

/// The category palette (`NOTE_CATEGORY_PALETTE`, src/shared/noteColors.ts).
/// The first entry is `DEFAULT_NOTE_COLOR`, the color a note with no category
/// and no color of its own wears.
const PALETTE: [u32; 8] = [
    0x7aa2f7, 0xe6c07b, 0x7fd99a, 0xc792ea, 0xf87171, 0x38bdf8, 0xf783ac, 0xffa94d,
];
const DEFAULT_COLOR: u32 = PALETTE[0];

/// One display row. The store carries more (timestamps, version); the pane
/// keeps what it renders, edits, and sorts by (`order`).
pub struct NoteRow {
    pub id: String,
    pub title: String,
    pub preview: String,
    pub body: String,
    pub color: u32,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub order: f64,
}

/// Epoch milliseconds — the `id` part of a freshly created note.
fn now_ms_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// A note id in the shape `POST /notes` mints when none is supplied. The pane
/// mints it itself so it can expand the new card without a round-trip.
pub fn new_note_id() -> String {
    format!(
        "note-{}-{}",
        now_ms_f() as i64,
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    )
}

fn note_color(value: Option<&serde_json::Value>) -> Option<u32> {
    let text = value?.as_str()?;
    let hex = text.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// Notes sorted by `order` ascending — `NotesStore.list()`'s order, which the
/// reorder controls rewrite. A missing file is empty rather than an error; an
/// unreadable or malformed one surfaces a one-line message the way the planner
/// pane does.
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

    let mut items: Vec<NoteRow> = Vec::new();
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
        items.push(NoteRow {
            id: id.to_owned(),
            title: title.to_owned(),
            preview,
            body,
            color,
            category,
            tags,
            order: entry
                .get("order")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
        });
    }
    items.sort_by(|a, b| {
        a.order
            .partial_cmp(&b.order)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (items, None)
}

/// A clickable field in an expanded card. The `on_mouse_down` stops the bubble
/// so the card's own select-toggle doesn't also fire.
fn field_chip(
    label: impl IntoElement,
    widget_id: &str,
    note_id: &str,
    field: &str,
    value: String,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> Div {
    let action = json!({
        "op": "note_edit",
        "id": note_id,
        "field": field,
        "value": value,
    });
    let wid = widget_id.to_owned();
    div()
        .px_1()
        .cursor_pointer()
        .text_xs()
        .text_color(rgb(theme::hex(theme::text::DIM)))
        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                this.widget_command(&wid, action.clone(), cx);
            }),
        )
}

/// A note card: 3px color rail on the left (the original's
/// `borderLeft: 3px solid <color>`), title, squashed body preview — or the
/// full body when `expanded` — then a category chip and `#tag` chips when
/// the note has any. Clicking toggles the card into `widget.state.expanded`.
///
/// An expanded card is the editor: each field is click-to-edit (the buffer
/// lives on `CanvasView::notes_editing`), ▲/▼ swap `order` with the neighbour,
/// the swatch row recolors the note's category (or the note alone when it has
/// none), and × drops the note through `note_delete`.
#[allow(clippy::type_complexity)]
fn note_card<'a>(
    row: &'a NoteRow,
    expanded: bool,
    editing: Option<(&str, &str)>,
    buffer: &str,
    neighbours: (Option<(String, f64)>, Option<(String, f64)>),
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
    card = card.child(div().w(px(3.0)).flex_none().bg(rgb(row.color)));

    // Click selects/deselects the note into the widget's journaled state.
    let expanded_value = if expanded { Value::Null } else { json!(row.id) };
    let toggle = json!({ "op": "set", "state": { "expanded": expanded_value } });
    let wid_toggle = widget_id.to_owned();
    let toggle_handler = cx.listener(move |this, _event, _window, cx| {
        this.widget_command(&wid_toggle, toggle.clone(), cx);
    });
    let editing_here = |field: &str| editing == Some((row.id.as_str(), field));

    // Title: the editor's buffer when this field is open, else the title.
    let title_text: AnyElement = if editing_here("title") {
        field_buffer(buffer).into_any_element()
    } else if expanded {
        field_chip(
            row.title.clone(),
            widget_id,
            &row.id,
            "title",
            row.title.clone(),
            cx,
        )
        .text_sm()
        .text_color(rgb(theme::hex(theme::text::NORMAL)))
        .into_any_element()
    } else {
        row.title.clone().into_any_element()
    };
    let mut title_row = div().flex().flex_row().items_center().gap_1().child(
        div()
            .flex_1()
            .min_w_0()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(theme::hex(theme::text::NORMAL)))
            .truncate()
            .child(title_text),
    );
    if expanded {
        let (prev, next) = neighbours;
        let swap = |other: Option<(String, f64)>,
                    glyph: &'static str,
                    cx: &mut Context<crate::canvas_view::CanvasView>| {
            let wid = widget_id.to_owned();
            let note_id = row.id.clone();
            let order = row.order;
            let mut chip = div()
                .flex_none()
                .px_1()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(glyph);
            if let Some((other_id, other_order)) = other {
                chip = chip
                    .cursor_pointer()
                    .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::ELEVATED))))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            cx.stop_propagation();
                            this.widget_command(
                                &wid,
                                json!({
                                    "op": "note_move",
                                    "id": note_id,
                                    "order": other_order,
                                    "otherId": other_id,
                                    "otherOrder": order,
                                }),
                                cx,
                            );
                        }),
                    );
            }
            chip
        };
        title_row = title_row
            .child(swap(prev, "▲", cx))
            .child(swap(next, "▼", cx));
        let note_id = row.id.clone();
        let wid_delete = widget_id.to_owned();
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
                        this.widget_command(
                            &wid_delete,
                            json!({ "op": "note_delete", "id": note_id }),
                            cx,
                        );
                    }),
                ),
        );
    }
    let mut body = div().flex().flex_col().min_w_0().flex_1().child(title_row);
    if editing_here("body") {
        body = body.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .child(field_buffer(buffer)),
        );
    } else if expanded {
        // Expanded: the whole body is the click target; empty bodies still
        // offer a place to type.
        let shown = if row.body.trim().is_empty() {
            "add body…".to_owned()
        } else {
            row.body.clone()
        };
        let colour = if row.body.trim().is_empty() {
            theme::hex(theme::text::FAINT)
        } else {
            theme::hex(theme::text::DIM)
        };
        body = body.child(div().text_xs().text_color(rgb(colour)).child(field_chip(
            shown,
            widget_id,
            &row.id,
            "body",
            row.body.clone(),
            cx,
        )));
    } else if !row.preview.is_empty() {
        body = body.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .truncate()
                .child(row.preview.clone()),
        );
    }

    let mut meta = div().flex().flex_row().flex_wrap().gap_1().pt(px(2.0));
    let show_meta = expanded || row.category.is_some() || !row.tags.is_empty();
    if show_meta {
        if editing_here("category") {
            meta = meta.child(field_buffer(buffer));
        } else if let Some(category) = &row.category {
            let chip = div()
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
                );
            meta = meta.child(if expanded {
                field_chip(chip, widget_id, &row.id, "category", category.clone(), cx)
                    .into_any_element()
            } else {
                chip.into_any_element()
            });
        } else if expanded {
            meta = meta.child(field_chip(
                "+ category",
                widget_id,
                &row.id,
                "category",
                String::new(),
                cx,
            ));
        }
        if editing_here("tags") {
            meta = meta.child(field_buffer(buffer));
        } else {
            for tag in &row.tags {
                meta = meta.child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!("#{tag}")),
                );
            }
            if expanded {
                meta = meta.child(field_chip(
                    "+ tags",
                    widget_id,
                    &row.id,
                    "tags",
                    row.tags.join(", "),
                    cx,
                ));
            }
        }
        body = body.child(meta);
    }

    // Swatches: the category's palette entry when the note has one, the
    // note's own color otherwise. Clicking one journals the recolor.
    if expanded {
        let mut swatches = div().flex().flex_row().gap_1().pt(px(2.0));
        for color in PALETTE {
            let wid = widget_id.to_owned();
            let note_id = row.id.clone();
            let category = row.category.clone();
            let swatch = div()
                .size(px(10.0))
                .rounded_full()
                .bg(rgb(color))
                .cursor_pointer()
                .when(color == row.color, |el| {
                    el.border_1()
                        .border_color(rgb(theme::hex(theme::text::NORMAL)))
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        cx.stop_propagation();
                        let action = match &category {
                            Some(name) => json!({
                                "op": "note_recolor",
                                "category": name,
                                "color": format!("#{color:06x}"),
                            }),
                            None => json!({
                                "op": "note_update",
                                "id": note_id,
                                "patch": { "color": format!("#{color:06x}") },
                            }),
                        };
                        this.widget_command(&wid, action, cx);
                    }),
                );
            swatches = swatches.child(swatch);
        }
        body = body.child(swatches);
    }

    card.child(body)
        .on_mouse_down(MouseButton::Left, toggle_handler)
}

/// The open editor's text with a caret — the DOM input stand-in.
fn field_buffer(buffer: &str) -> impl IntoElement {
    div()
        .px_1()
        .text_xs()
        .bg(rgb(theme::hex(theme::monochrome::ELEVATED)))
        .text_color(rgb(theme::hex(theme::text::NORMAL)))
        .child(format!("{buffer}▏"))
}

pub fn notes_pane(
    widget: &slate_app::projection::Widget,
    editing: Option<(&str, &str)>,
    buffer: &str,
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
    // Header row: count on the left, + journals a blank note (`note_create`,
    // i.e. POST /notes) and expands it for the follow-up edit.
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
                            let id = new_note_id();
                            this.widget_command(&wid, json!({ "op": "note_create", "id": id }), cx);
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
    for (index, row) in rows.iter().enumerate() {
        let prev = index
            .checked_sub(1)
            .map(|i| (rows[i].id.clone(), rows[i].order));
        let next = rows.get(index + 1).map(|n| (n.id.clone(), n.order));
        list = list.child(note_card(
            row,
            expanded == Some(row.id.as_str()),
            editing,
            buffer,
            (prev, next),
            &wid,
            cx,
        ));
    }
    col.child(list)
}
