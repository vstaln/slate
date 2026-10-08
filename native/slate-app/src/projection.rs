//! Folding the command journal into a canvas snapshot.
//!
//! Block 2 of the migration (see docs/RUST-MIGRATION.md). The criterion is that
//! replaying the same journal here and in `CanvasStore.reduce`
//! (src/main/canvasState.ts) produces the same snapshot — same widgets, in the
//! same order, with the same versions, camera, strokes and connections.
//!
//! Ported deliberately literally. Several behaviours below look like details
//! and are not:
//!
//! * a widget whose `kind` is unknown is rejected outright, not defaulted, so a
//!   canvas written by a newer build loses the widget rather than mistyping it;
//! * connections are de-duplicated by the `from`→`to` pair and not by id, so a
//!   journal recording the same link twice does not stack two arcs;
//! * orphaned connections are pruned only in the branches that can orphan one.
//!   A blanket pass would run on every `widget.update`, i.e. once per frame of
//!   every drag.
//!
//! Widget insertion order is preserved because the snapshot lists widgets in
//! that order; a sorted map would produce a different, equally "correct"
//! snapshot that does not match what Electron wrote.

use crate::journal::JournalEntry;
use indexmap::IndexMap;
use serde_json::Value;

pub const MAX_WIDGETS: usize = 200;
pub const MAX_STROKE_POINTS: usize = 200_000;
pub const MAX_POINTS_PER_STROKE: usize = 10_000;
pub const MAX_CONNECTIONS: usize = 2_000;

/// Must stay in step with WIDGET_KINDS in src/main/canvasState.ts: a kind this
/// list does not know is not merely ignored, it makes sanitize_widget drop the
/// whole widget, so a replay silently loses every widget of that kind.
const WIDGET_KINDS: [&str; 14] = [
    "terminal",
    "timer",
    "planner",
    "files",
    "sys-monitor",
    "browser",
    "image",
    "links",
    "music-player",
    "orchestration",
    "chat",
    "notes",
    "calendar",
    "kanban",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Widget {
    pub id: String,
    pub title: String,
    pub kind: Option<String>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub z: f64,
    pub maximized: bool,
    /// An image widget is nothing without these: they are what it displays.
    /// widget.update rebuilds the widget from widget_to_value before merging
    /// the patch, so anything missing here is erased by the next update.
    pub image_path: Option<String>,
    pub image_name: Option<String>,
    /// Free-form per-widget UI state (calendar month cursor, collapsed
    /// columns, scroll offsets). Journaled like everything else, so a pane's
    /// local navigation survives restart.
    pub state: Option<Value>,
    pub version: f64,
    pub updated_at: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Camera {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stroke {
    pub id: String,
    pub points: Vec<Point>,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Connection {
    pub id: String,
    pub from: String,
    pub to: String,
    pub born_at: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CanvasState {
    pub widgets: IndexMap<String, Widget>,
    pub camera: Camera,
    pub strokes: Vec<Stroke>,
    pub connections: Vec<Connection>,
    pub version: f64,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            widgets: IndexMap::new(),
            camera: Camera::default(),
            strokes: Vec::new(),
            connections: Vec::new(),
            version: 1.0,
        }
    }
}

/// `Date.now()` stands in for missing timestamps in the TypeScript sanitizers,
/// which makes the reducer non-deterministic for entries that omit them. The
/// clock is therefore injected rather than read, so a replay is reproducible
/// and a parity test compares like with like.
#[derive(Debug, Clone, Copy)]
pub struct Clock(pub f64);

/// JavaScript's `Number.isFinite` on an already-typed value: `NaN` and the
/// infinities are not numbers for these purposes, and neither is a non-number.
fn num(value: Option<&Value>) -> Option<f64> {
    let n = value?.as_f64()?;
    if n.is_finite() {
        Some(n)
    } else {
        None
    }
}

fn text(value: Option<&Value>) -> Option<&str> {
    value?.as_str()
}

pub fn sanitize_widget(value: &Value, clock: Clock) -> Option<Widget> {
    let object = value.as_object()?;
    let id = text(object.get("id"))?.to_owned();
    let title = text(object.get("title"))?.to_owned();
    let x = num(object.get("x"))?;
    let y = num(object.get("y"))?;
    let w = num(object.get("w"))?;
    let h = num(object.get("h"))?;
    let z = num(object.get("z"))?;

    // An explicit kind must be one this build knows; absent is allowed.
    let kind = match object.get("kind") {
        None | Some(Value::Null) => None,
        Some(other) => {
            let spelled = match other {
                Value::String(s) => s.clone(),
                // String(value) in JS, which is what the TypeScript check does.
                other => other.to_string(),
            };
            if !WIDGET_KINDS.contains(&spelled.as_str()) {
                return None;
            }
            Some(spelled)
        }
    };

    // Bounds counted in UTF-16 units, which is what `String.length` means on
    // the TypeScript side these limits are mirrored from.
    let image_path = text(object.get("imagePath"))
        .filter(|value| value.encode_utf16().count() <= 4096)
        .map(str::to_owned);
    let image_name = text(object.get("imageName"))
        .filter(|value| value.encode_utf16().count() <= 256)
        .map(str::to_owned);

    let version = match num(object.get("version")) {
        Some(v) if v > 0.0 => v,
        _ => 1.0,
    };
    let updated_at = num(object.get("updatedAt")).unwrap_or(clock.0);

    Some(Widget {
        id,
        title,
        kind,
        x,
        y,
        w,
        h,
        z,
        maximized: object.get("maximized") == Some(&Value::Bool(true)),
        image_path,
        image_name,
        // `state` is free-form pane UI state — capped so a hostile journal
        // cannot smuggle a giant blob into memory.
        state: object
            .get("state")
            .cloned()
            .filter(|v| v.to_string().len() <= 16_384),
        version,
        updated_at,
    })
}

pub fn sanitize_camera(value: Option<&Value>) -> Camera {
    let Some(object) = value.and_then(Value::as_object) else {
        return Camera::default();
    };
    let (Some(x), Some(y), Some(zoom)) = (
        num(object.get("x")),
        num(object.get("y")),
        num(object.get("zoom")),
    ) else {
        return Camera::default();
    };
    Camera {
        x,
        y,
        zoom: zoom.clamp(0.2, 4.0),
    }
}

pub fn sanitize_strokes(value: Option<&Value>) -> Vec<Stroke> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut strokes = Vec::new();
    let mut total_points = 0usize;
    for entry in entries {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let (Some(id), Some(color)) = (text(object.get("id")), text(object.get("color"))) else {
            continue;
        };
        let Some(raw_points) = object.get("points").and_then(Value::as_array) else {
            continue;
        };

        let mut points = Vec::new();
        for raw in raw_points {
            let Some(point) = raw.as_object() else {
                continue;
            };
            let (Some(x), Some(y)) = (num(point.get("x")), num(point.get("y"))) else {
                continue;
            };
            points.push(Point { x, y });
            if points.len() >= MAX_POINTS_PER_STROKE {
                break;
            }
        }
        // A single point is not a line; the TypeScript reader drops these too.
        if points.len() < 2 {
            continue;
        }
        total_points += points.len();
        strokes.push(Stroke {
            id: id.to_owned(),
            points,
            color: color.to_owned(),
        });
        if total_points >= MAX_STROKE_POINTS {
            break;
        }
    }
    strokes
}

pub fn sanitize_connections(value: Option<&Value>, clock: Clock) -> Vec<Connection> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut connections: Vec<Connection> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let Some(object) = entry.as_object() else {
            continue;
        };
        let (Some(id), Some(from), Some(to)) = (
            text(object.get("id")),
            text(object.get("from")),
            text(object.get("to")),
        ) else {
            continue;
        };
        if from.is_empty() || to.is_empty() || from == to {
            continue;
        }
        // Identified by the pair, not the id: replaying a journal that recorded
        // the same link twice must not stack two arcs.
        let pair = format!("{from} {to}");
        if !seen.insert(pair) {
            continue;
        }
        connections.push(Connection {
            id: id.to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            born_at: num(object.get("bornAt")).unwrap_or(clock.0),
        });
        if connections.len() >= MAX_CONNECTIONS {
            break;
        }
    }
    connections
}

pub fn live_connections(
    connections: Vec<Connection>,
    widgets: &IndexMap<String, Widget>,
) -> Vec<Connection> {
    connections
        .into_iter()
        .filter(|c| widgets.contains_key(&c.from) && widgets.contains_key(&c.to))
        .collect()
}

/// One journal entry applied to a canvas state. Mirrors `CanvasStore.reduce`.
pub fn reduce(state: &CanvasState, event: &JournalEntry, clock: Clock) -> CanvasState {
    if event.phase != "commit" {
        return state.clone();
    }

    let mut next = state.clone();
    let empty = Value::Object(Default::default());
    let payload = event.payload.as_ref().unwrap_or(&empty);
    let payload_object = payload.as_object();
    let get = |key: &str| payload_object.and_then(|object| object.get(key));

    let target_id = event
        .target
        .strip_prefix("widget:")
        .unwrap_or(&event.target)
        .to_owned();

    match event.entry_type.as_str() {
        "widget.create" => {
            let mut seed = payload.clone();
            if let Some(object) = seed.as_object_mut() {
                let id = if target_id == "new" {
                    match object.get("id").and_then(Value::as_str) {
                        Some(id) => id.to_owned(),
                        None => format!("widget-{}-{}", event.at, event.seq),
                    }
                } else {
                    target_id.clone()
                };
                object.insert("id".into(), Value::String(id));
                object.insert(
                    "version".into(),
                    json_number(event.version.unwrap_or(1) as f64),
                );
                object.insert("updatedAt".into(), json_number(event.at as f64));
            }
            if let Some(widget) = sanitize_widget(&seed, clock) {
                // `control.add-widget`'s `from` — the shell auto-wired a
                // connection from the named widget to the new one.
                if let Some(from) = payload.get("from").and_then(Value::as_str) {
                    if !from.is_empty()
                        && from != widget.id
                        && next.widgets.contains_key(from)
                    {
                        let pair_exists = next
                            .connections
                            .iter()
                            .any(|c| c.from == from && c.to == widget.id);
                        if !pair_exists && next.connections.len() < MAX_CONNECTIONS {
                            next.connections.push(Connection {
                                id: format!("conn-{}-{}", event.at, event.seq),
                                from: from.to_owned(),
                                to: widget.id.clone(),
                                born_at: event.at as f64,
                            });
                        }
                    }
                }
                next.widgets.insert(widget.id.clone(), widget);
            }
        }
        "widget.update" => {
            if let Some(existing) = next.widgets.get(&target_id).cloned() {
                let mut merged = widget_to_value(&existing);
                if let (Some(target), Some(patch)) = (merged.as_object_mut(), payload_object) {
                    for (key, value) in patch {
                        target.insert(key.clone(), value.clone());
                    }
                    target.insert("id".into(), Value::String(target_id.clone()));
                    let version = event
                        .version
                        .map(|v| v as f64)
                        .unwrap_or(existing.version + 1.0);
                    target.insert("version".into(), json_number(version));
                    target.insert("updatedAt".into(), json_number(event.at as f64));
                }
                if let Some(widget) = sanitize_widget(&merged, clock) {
                    // insert() on an existing key keeps its position, which is
                    // what replacing an entry in a JavaScript Map does.
                    next.widgets.insert(target_id.clone(), widget);
                }
            }
        }
        "widget.remove" => {
            if next.widgets.shift_remove(&target_id).is_some() {
                next.connections = live_connections(next.connections, &next.widgets);
            }
        }
        "canvas.camera" => {
            next.camera = sanitize_camera(Some(payload));
            next.version = event
                .version
                .map(|v| v as f64)
                .unwrap_or(next.version + 1.0);
        }
        "canvas.strokes" => {
            next.strokes = sanitize_strokes(get("strokes"));
            next.version = event
                .version
                .map(|v| v as f64)
                .unwrap_or(next.version + 1.0);
        }
        "canvas.connections" => {
            next.connections = live_connections(
                sanitize_connections(get("connections"), clock),
                &next.widgets,
            );
            next.version = event
                .version
                .map(|v| v as f64)
                .unwrap_or(next.version + 1.0);
        }
        "canvas.import" => {
            if let Some(widgets) = get("widgets").and_then(Value::as_array) {
                for raw in widgets {
                    if let Some(widget) = sanitize_widget(raw, clock) {
                        next.widgets.insert(widget.id.clone(), widget);
                    }
                }
            }
            if get("camera").is_some() {
                next.camera = sanitize_camera(get("camera"));
            }
            if get("strokes").is_some() {
                next.strokes = sanitize_strokes(get("strokes"));
            }
            // Filtered after the widget loop, so a snapshot carrying a widget
            // and an arc to it in one payload keeps the arc whatever order the
            // two appear in.
            if get("connections").is_some() {
                next.connections = live_connections(
                    sanitize_connections(get("connections"), clock),
                    &next.widgets,
                );
            }
            next.version = event
                .version
                .map(|v| v as f64)
                .unwrap_or(next.version + 1.0);
        }
        "canvas.restore" => {
            // The undo persist: the payload IS the canvas — widgets absent
            // from it are gone, unlike `canvas.import`'s additive merge.
            next.widgets = Default::default();
            if let Some(widgets) = get("widgets").and_then(Value::as_array) {
                for raw in widgets {
                    if let Some(widget) = sanitize_widget(raw, clock) {
                        next.widgets.insert(widget.id.clone(), widget);
                    }
                }
            }
            next.camera = sanitize_camera(get("camera"));
            next.strokes = sanitize_strokes(get("strokes"));
            next.connections = live_connections(
                sanitize_connections(get("connections"), clock),
                &next.widgets,
            );
            next.version = event
                .version
                .map(|v| v as f64)
                .unwrap_or(next.version + 1.0);
        }
        _ => {}
    }

    next
}

/// Replays a run of entries. Entries that are not commits are skipped by
/// `reduce` itself, so the caller does not have to pre-filter.
pub fn fold<'a>(
    events: impl IntoIterator<Item = &'a JournalEntry>,
    initial: CanvasState,
    clock: Clock,
) -> CanvasState {
    let mut state = initial;
    for event in events {
        state = reduce(&state, event, clock);
    }
    state
}

/// Widgets in insertion order, as `listWidgets()` returns them.
pub fn list_widgets(state: &CanvasState) -> Vec<&Widget> {
    state.widgets.values().collect()
}

fn json_number(value: f64) -> Value {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

pub fn widget_to_value(widget: &Widget) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".into(), Value::String(widget.id.clone()));
    object.insert("title".into(), Value::String(widget.title.clone()));
    match &widget.kind {
        Some(kind) => object.insert("kind".into(), Value::String(kind.clone())),
        None => object.insert("kind".into(), Value::Null),
    };
    object.insert("x".into(), json_number(widget.x));
    object.insert("y".into(), json_number(widget.y));
    object.insert("w".into(), json_number(widget.w));
    object.insert("h".into(), json_number(widget.h));
    object.insert("z".into(), json_number(widget.z));
    object.insert("maximized".into(), Value::Bool(widget.maximized));
    if let Some(path) = &widget.image_path {
        object.insert("imagePath".into(), Value::String(path.clone()));
    }
    if let Some(name) = &widget.image_name {
        object.insert("imageName".into(), Value::String(name.clone()));
    }
    if let Some(state) = &widget.state {
        object.insert("state".into(), state.clone());
    }
    object.insert("version".into(), json_number(widget.version));
    object.insert("updatedAt".into(), json_number(widget.updated_at));
    Value::Object(object)
}
