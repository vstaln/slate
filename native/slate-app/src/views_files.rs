//! Files pane — the Electron `FilesWidget` equivalent: a directory listing
//! rooted at the path the caller passes (the app cwd today). Navigation is
//! per-widget and journaled: `widget.state.cwd` holds the path relative to
//! the root, so a widget that drilled into `src/` reopens there. `..` climbs
//! one level but never past the root; file rows open the preview overlay —
//! the original's `openItem` → `viewFile` — not `xdg-open`.
//!
//! The original's row actions are ported too, kept pure like every other
//! pane — clicks emit `{"op": …}` actions on `CanvasView::widget_command`
//! and the dispatcher does the filesystem work:
//!   * `file_preview`        — bounded `fs:read-file` port → the preview
//!     overlay (left-click on a file row, or the row's eye button)
//!   * `file_preview_close`  — the overlay's ✕ (its Escape lives in
//!     `CanvasView::on_key`, which owns the modal's keys)
//!   * `file_copy_content`   — the overlay's "Copy Text"
//!   * `file_open`           — `xdg-open <path>` (the row's ↗ and the
//!     overlay's "Open with OS App")
//!   * `file_reveal`         — file manager at `path` (dirs open themselves,
//!     files open their parent directory)
//!   * `file_copy_path`      — absolute path onto the clipboard
//!   * `file_new`            — create `untitled`/`untitled-N` of `kind` in
//!     `dir`, then arm the inline rename like the original's create flow
//!   * `file_rename`         — `fs.rename` inside the jail; the row's ✎
//!     Save button and the rename field's Enter
//!   * `file_delete`         — remove the file / dir tree at `path`; only
//!     sent for the path `state.confirmDelete` already armed
//!   * `refresh`             — unknown-op no-op; `widget_command` still
//!     notifies, so it re-reads the directory for free
//!
//! Pane-local keys journaled on `widget.state`: `cwd`, `search` (the
//! original's `search` state — localStorage there, journaled here — the
//! listing filters on it live), `showHidden`, `sort`
//! (`name`/`mtime`/`size`), `confirmDelete` (abs path armed for a second
//! click — the port of the original's danger confirm dialog), and `limit`
//! (visible rows — the original's 200-row "Show more" pagination).
//!
//! Keyboard-owning UI — the search field, the row rename input, and the
//! preview overlay — is CanvasView state (`files_editing`/`files_preview`),
//! keyed by widget id like `browser_editing`/`browser_address`: the pane
//! sets it from its own clicks, `CanvasView::on_key` routes keystrokes,
//! and Enter/Escape/Save/Cancel settle it. Preview content deliberately
//! lives there too and not in `widget.state` — file bytes never enter the
//! journal.

use gpui::prelude::FluentBuilder;
use gpui::*;
use serde_json::{json, Value};
use slate_app::theme;
use std::path::{Path, PathBuf};

/// One listing row — the metadata the Electron table showed (size, mtime)
/// rides along so `state.sort` can order by it without a second stat.
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    /// Byte length for files; directory sizes are never shown (`—`), so this
    /// is just the raw st_size kept for the `size` sort key.
    pub size: u64,
    /// Seconds since the Unix epoch; formatted to local time at render.
    pub mtime: Option<u64>,
}

/// The listing plus the original's `FsListResult.truncated`: the backend
/// capped one folder read at 2,000 entries and set the flag when more
/// existed, and `FilesWidget` showed a "Showing the first 2,000 items"
/// note — a folder that looked shorter than it is was the bug it fixed.
pub fn list_entries(path: &std::path::Path) -> Result<(Vec<FileEntry>, bool), String> {
    let mut entries: Vec<FileEntry> = Vec::new();
    let mut truncated = false;
    for entry in std::fs::read_dir(path).map_err(|e| e.to_string())? {
        if entries.len() >= 2000 {
            truncated = true;
            break;
        }
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.path().is_dir();
        let (size, mtime) = match entry.metadata() {
            Ok(meta) => (
                meta.len(),
                meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs()),
            ),
            Err(_) => (0, None),
        };
        entries.push(FileEntry {
            name,
            is_dir,
            size,
            mtime,
        });
    }
    // Same canonical order as files_panel.rs — dirs first, then
    // case-insensitive name. `state.sort` re-orders the slice at render;
    // this stays the stable fallback a hand-edited sort key lands on.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((entries, truncated))
}

/// Which files-pane field owns the keyboard — `CanvasView::files_editing`.
/// The original kept these as component state (`search` lived in
/// `widget.state` here since it also persisted); the port keys them by
/// widget id like `browser_editing`/`browser_address` so several panes
/// can't fight over the keys.
pub enum FilesEditing {
    /// The `Search files…` input. The query itself rides
    /// `widget.state.search` so the listing filters live; the enum only
    /// marks that this pane's field holds the keyboard.
    Search { id: String },
    /// `renamingPath` + `renamingName` — the row's absolute path and the
    /// edit buffer. Armed by the row's ✎ or by `file_new` (the original's
    /// create → rename flow); Enter/Save dispatch `file_rename`.
    Rename {
        id: String,
        path: String,
        buffer: String,
    },
}

impl FilesEditing {
    pub fn widget_id(&self) -> &str {
        match self {
            FilesEditing::Search { id } => id,
            FilesEditing::Rename { id, .. } => id,
        }
    }
}

/// What the preview overlay draws — the `FileReadResult` cases:
/// image (`isImage`/`dataUrl`), plain text (`content`), or the
/// binary-file notice (`isBinary`).
pub enum FilesPreviewBody {
    /// Rendered straight from `preview.path` with `img()` — the native
    /// port doesn't need the original's base64 `dataUrl` hop.
    Image,
    /// UTF-8 text (lossy like the shell's `buffer.toString('utf8')`).
    Text(String),
    /// A NUL byte in the read window — the original's `buffer.includes(0)`.
    Binary,
}

/// `previewFile` — the loaded preview for one files pane. Kept on
/// `CanvasView` (never journaled): up to 5 MB of file text has no business
/// in `widget.state`.
pub struct FilesPreview {
    pub id: String,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub body: FilesPreviewBody,
}

/// `media.hasImageExtension` — the `MIME_BY_EXT` keys the shell's
/// `fs:read-file` routed to the image branch.
fn has_image_extension(path: &Path) -> bool {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "svg" | "ico" | "tif" | "tiff"
            | "heic"
    )
}

/// `fs:read-file`, minus the IPC: images get the shell's
/// `MAX_DATA_URL_BYTES` ceiling (8 MB — though the native port renders the
/// path rather than a data URL, the cap still gates the preview), every
/// other file the `MAX_FS_READ_BYTES` one (5 MB). A NUL byte marks the
/// file binary; anything else is shown as lossy UTF-8 text.
pub fn preview_load(path: &Path) -> Result<(FilesPreviewBody, u64), String> {
    const MAX_FS_READ_BYTES: u64 = 5 * 1024 * 1024;
    const MAX_DATA_URL_BYTES: u64 = 8 * 1024 * 1024;
    // `lstat` like the shell — the jail already resolved the path, so a
    // symlink reaching here resolved inside the root.
    let stat = std::fs::metadata(path).map_err(|_| "File does not exist".to_owned())?;
    if !stat.is_file() {
        return Err("Path is not a regular file".to_owned());
    }
    let size = stat.len();
    if has_image_extension(path) {
        if size > MAX_DATA_URL_BYTES {
            return Err(format!(
                "File is too large to preview ({:.1} MB).",
                size as f64 / (1024.0 * 1024.0)
            ));
        }
        return Ok((FilesPreviewBody::Image, size));
    }
    if size > MAX_FS_READ_BYTES {
        return Err(format!(
            "File is too large to preview ({:.1} MB). Limit is 5 MB.",
            size as f64 / (1024.0 * 1024.0)
        ));
    }
    let bytes = std::fs::read(path).map_err(|_| "Unable to read this file".to_owned())?;
    if bytes.contains(&0) {
        return Ok((FilesPreviewBody::Binary, size));
    }
    Ok((
        FilesPreviewBody::Text(String::from_utf8_lossy(&bytes).into_owned()),
        size,
    ))
}

/// `widget.state.cwd` as a clean relative path — only `Normal` components
/// survive, so a hand-edited `../../etc` collapses to `etc` rather than
/// escaping the pane root.
fn relative_cwd(widget: &slate_app::projection::Widget) -> PathBuf {
    let raw = widget
        .state
        .as_ref()
        .and_then(|state| state.get("cwd"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    Path::new(raw)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(seg) => Some(seg.to_owned()),
            _ => None,
        })
        .collect()
}

/// `root.join(rel)`, verified — canonicalization collapses symlinks, and a
/// joined path that escapes the root resolves to None so the listing stays
/// pinned where it started.
fn resolve_in_root(root: &Path, rel: &Path) -> Option<PathBuf> {
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if rel.as_os_str().is_empty() {
        return Some(root_canon);
    }
    let joined = root.join(rel);
    match joined.canonicalize() {
        Ok(canon) if canon.starts_with(&root_canon) => Some(canon),
        _ => None,
    }
}

/// Icon glyph per extension — the same buckets the Electron `fileIcon`
/// helper switched on (code, config, docs, image, audio, video, sheets),
/// rendered as text instead of lucide svg.
fn entry_icon(name: &str, is_dir: bool) -> &'static str {
    if is_dir {
        return "▸";
    }
    let ext = Path::new(name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "ts" | "tsx" | "js" | "jsx" | "py" | "rs" | "go" | "c" | "cpp" | "java" | "php" | "rb"
        | "sh" | "bat" | "cmd" => "<>",
        "json" | "yaml" | "yml" | "toml" | "xml" | "env" | "config" => "{}",
        "md" | "markdown" | "txt" | "log" => "¶",
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => "◇",
        "mp3" | "wav" | "ogg" | "flac" | "m4a" => "♪",
        "mp4" | "mkv" | "webm" | "avi" | "mov" => "▶",
        "csv" | "xlsx" | "xls" => "▦",
        _ => "·",
    }
}

/// The original's `formatBytes`: 1024-based units, one decimal trimmed —
/// `1536` reads "1.5 KB", `1024` reads "1 KB".
fn human_size(size: u64) -> String {
    if size == 0 {
        return "0 B".to_owned();
    }
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let tier = ((size as f64).log2() / 10.0) as usize;
    let tier = tier.min(UNITS.len() - 1);
    let value = size as f64 / 1024f64.powi(tier as i32);
    let text = format!("{value:.1}");
    let text = text.strip_suffix(".0").unwrap_or(&text);
    format!("{text} {}", UNITS[tier])
}

#[cfg(unix)]
fn local_tm(secs: u64) -> Option<libc::tm> {
    unsafe {
        let raw = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&raw, &mut tm).is_null() {
            None
        } else {
            Some(tm)
        }
    }
}

/// Days since the epoch → (year, month, day), the civil-from-days
/// conversion — only reached where `localtime_r` is missing or fails, in
/// which case UTC is the honest clock to print.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// `YYYY-MM-DD HH:MM` in local time — same localtime_r pattern as
/// `canvas_view::clock_hms`, falling back to UTC arithmetic off-unix.
fn format_mtime(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return String::new();
    };
    #[cfg(unix)]
    {
        if let Some(tm) = local_tm(secs) {
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min
            );
        }
    }
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        year,
        month,
        day,
        rem / 3600,
        rem % 3600 / 60
    )
}

/// One header button — same minimal styling the crumbs use: faint text that
/// lifts on hover. `active` tints it, like the original's accent toggle.
fn header_button(
    label: String,
    active: bool,
    wid: &str,
    action: Value,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = wid.to_owned();
    div()
        .text_xs()
        .cursor_pointer()
        .px_1()
        .text_color(rgb(theme::hex(if active {
            theme::accent::SAND
        } else {
            theme::text::FAINT
        })))
        .hover(|el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(theme::text::DIM)))
        })
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                this.widget_command(&wid, action.clone(), cx);
            }),
        )
}

/// One row action button — sits in the hover-revealed strip, so it stops
/// propagation: a click on it must not fire the row's own navigate/open.
fn row_button(
    label: String,
    danger: bool,
    wid: &str,
    actions: Vec<Value>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let wid = wid.to_owned();
    div()
        .text_xs()
        .cursor_pointer()
        .px_0p5()
        .text_color(rgb(theme::hex(if danger {
            theme::status::DANGER
        } else {
            theme::text::FAINT
        })))
        .hover(move |el| {
            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                .text_color(rgb(theme::hex(if danger {
                    theme::status::DANGER
                } else {
                    theme::text::NORMAL
                })))
        })
        .child(label)
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                for action in &actions {
                    this.widget_command(&wid, action.clone(), cx);
                }
            }),
        )
}

pub fn files_pane(
    root: &std::path::Path,
    widget: &slate_app::projection::Widget,
    // The pane's slice of CanvasView keyboard/preview state — passed in
    // because `entity().read()` mid-render would re-borrow the view and
    // panic (same reason `browser_editing` rides a parameter).
    editing: Option<&FilesEditing>,
    preview: Option<&FilesPreview>,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let rel = relative_cwd(widget);
    let cur = resolve_in_root(root, &rel).unwrap_or_else(|| root.to_path_buf());
    let wid = widget.id.clone();

    let state = widget.state.as_ref();
    let show_hidden = state
        .and_then(|s| s.get("showHidden"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // `state.search` — journaled like the shell's localStorage copy; the
    // saved 500-char cap applies on the way out too.
    let search = state
        .and_then(|s| s.get("search"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(500)
        .collect::<String>();
    let sort = state
        .and_then(|s| s.get("sort"))
        .and_then(Value::as_str)
        .unwrap_or("name");
    let confirm_delete = state
        .and_then(|s| s.get("confirmDelete"))
        .and_then(Value::as_str);
    let limit = state
        .and_then(|s| s.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(200) as usize;
    // Only this pane's edit matters — `files_editing` is keyed by widget id.
    let search_editing = matches!(editing, Some(FilesEditing::Search { id }) if id == &wid);
    let renaming = match editing {
        Some(FilesEditing::Rename { id, path, buffer }) if id == &wid => {
            Some((path.as_str(), buffer.as_str()))
        }
        _ => None,
    };
    // Navigating anywhere resets pagination and disarms a pending delete —
    // the original reset its 200-row window on every path change.
    let nav_state = |cwd: String| {
        json!({ "op": "set", "state": {
            "cwd": cwd,
            "limit": 200,
            "confirmDelete": Value::Null,
        } })
    };

    let mut col = div().flex().flex_col().gap_1().size_full();

    // Header strip — the original's toolbar row: which directory this is,
    // then the toggles and create/reveal affordances.
    {
        let basename = cur
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| cur.display().to_string());
        let next_sort = match sort {
            "name" => "mtime",
            "mtime" => "size",
            _ => "name",
        };
        let cur_abs = cur.to_string_lossy().into_owned();
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(basename),
                )
                .child(header_button(
                    format!("sort: {sort}"),
                    sort != "name",
                    &wid,
                    json!({ "op": "set", "state": { "sort": next_sort } }),
                    cx,
                ))
                .child(header_button(
                    "+file".to_owned(),
                    false,
                    &wid,
                    json!({ "op": "file_new", "dir": cur_abs, "kind": "file" }),
                    cx,
                ))
                .child(header_button(
                    "+dir".to_owned(),
                    false,
                    &wid,
                    json!({ "op": "file_new", "dir": cur_abs, "kind": "dir" }),
                    cx,
                ))
                .child(header_button(
                    "⟳".to_owned(),
                    false,
                    &wid,
                    json!({ "op": "refresh" }),
                    cx,
                ))
                .child(header_button(
                    "↗".to_owned(),
                    false,
                    &wid,
                    json!({ "op": "file_reveal", "path": cur_abs }),
                    cx,
                )),
        );
    }

    // Breadcrumbs: ⌂ returns to the pane root, each segment jumps to that
    // prefix of the current relative path.
    let mut crumbs = div().flex().flex_row().flex_wrap().items_center();
    {
        let wid = wid.clone();
        crumbs = crumbs.child(
            div()
                .text_xs()
                .cursor_pointer()
                .px_1()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .hover(|el| {
                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .text_color(rgb(theme::hex(theme::text::DIM)))
                })
                .child("⌂")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.files_edit_release(&wid);
                        this.widget_command(&wid, nav_state(String::new()), cx);
                    }),
                ),
        );
    }
    let mut prefix = PathBuf::new();
    for seg in rel.components() {
        let std::path::Component::Normal(seg) = seg else {
            continue;
        };
        prefix.push(seg);
        let target = prefix.to_string_lossy().into_owned();
        let wid = wid.clone();
        crumbs = crumbs.child(
            div()
                .text_xs()
                .text_color(rgb(theme::hex(theme::hairline::FAINT)))
                .child("/"),
        );
        crumbs = crumbs.child(
            div()
                .text_xs()
                .cursor_pointer()
                .px_1()
                .text_color(rgb(theme::hex(theme::text::DIM)))
                .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                .child(seg.to_string_lossy().into_owned())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.files_edit_release(&wid);
                        this.widget_command(&wid, nav_state(target.clone()), cx);
                    }),
                ),
        );
    }
    col = col.child(crumbs);

    // The one directory read per render — hoisted so the truncated note
    // can sit between the toolbar and the listing like the original's.
    let listing = list_entries(&cur);
    let truncated = listing.as_ref().map(|(_, t)| *t).unwrap_or(false);

    // The original's search row: `Search files…` input + the Hidden
    // toggle. The field is a click-to-edit like the browser omnibox —
    // keystrokes reach it through `CanvasView::on_key` while
    // `files_editing` marks this pane's field.
    {
        let wid_search = wid.clone();
        col = col.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_1()
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child("⌕"),
                )
                .child(
                    div()
                        .id(format!("slate-files-search-{}", wid_search))
                        .flex_1()
                        .min_w_0()
                        .h(px(22.0))
                        .flex()
                        .flex_row()
                        .items_center()
                        .px_1()
                        .bg(rgb(theme::hex(theme::monochrome::RAISED)))
                        .cursor_text()
                        .text_xs()
                        .text_color(rgb(theme::hex(if search_editing {
                            theme::text::NORMAL
                        } else {
                            theme::text::DIM
                        })))
                        .child(if search.is_empty() {
                            "Search files…".to_owned()
                        } else {
                            search.clone()
                        })
                        .when(search_editing, |el| {
                            el.child(
                                div()
                                    .text_color(rgb(theme::hex(theme::text::DIM)))
                                    .child("▌".to_owned()),
                            )
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, window, cx| {
                                cx.stop_propagation();
                                window.focus(&this.focus, cx);
                                // The field's focus-tracking — the query
                                // itself is journaled `state.search`; this
                                // only marks who owns the keyboard, like
                                // the omnibox's `browser_editing`.
                                this.files_editing = Some(FilesEditing::Search {
                                    id: wid_search.clone(),
                                });
                                this.active = Some(wid_search.clone());
                                cx.notify();
                            }),
                        ),
                )
                .when(!search.is_empty(), |el| {
                    // The clear-✕ — the original's right-side button.
                    el.child(header_button(
                        "✕".to_owned(),
                        false,
                        &wid,
                        json!({ "op": "set", "state": { "search": "", "limit": 200 } }),
                        cx,
                    ))
                })
                .child(header_button(
                    if show_hidden {
                        "hidden: on".to_owned()
                    } else {
                        "hidden: off".to_owned()
                    },
                    show_hidden,
                    &wid,
                    json!({ "op": "set", "state": { "showHidden": !show_hidden } }),
                    cx,
                )),
        );
    }

    // The original's truncation note, verbatim — it only renders when the
    // 2,000-item cap dropped entries, and names the search caveat while a
    // query is active.
    if truncated {
        col = col.child(
            div()
                .px_1()
                .text_xs()
                .text_color(rgb(theme::hex(theme::text::FAINT)))
                .child(if search.is_empty() {
                    "Showing the first 2,000 items. Some files in this folder are not listed."
                        .to_owned()
                } else {
                    "Showing the first 2,000 items — search only covers those. Some files in this folder are not listed."
                        .to_owned()
                }),
        );
    }

    // The listing scrolls; the toolbar and crumbs above stay put.
    let mut list = div()
        .id(format!("slate-files-list-{}", wid))
        .flex()
        .flex_col()
        .gap_1()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();

    match listing {
        Ok((mut entries, _truncated)) => {
            // `state.showHidden` filters dotfiles — the `..` row below is
            // navigation, not an entry, so it never hides.
            if !show_hidden {
                entries.retain(|e| !e.name.starts_with('.'));
            }
            // `filteredItems` — `item.name.toLowerCase().includes(q)` over
            // the loaded slice, exactly what the truncation note warns
            // about when the 2,000-cap dropped the rest.
            let query = search.trim().to_lowercase();
            if !query.is_empty() {
                entries.retain(|e| e.name.to_lowercase().contains(&query));
            }
            match sort {
                // Dirs keep floating first whichever key is picked; the key
                // only decides inside each partition, name breaks the ties.
                "mtime" => entries.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| b.mtime.cmp(&a.mtime))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                }),
                "size" => entries.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| b.size.cmp(&a.size))
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                }),
                _ => {}
            }
            let total = entries.len();

            // `..` climbs one component of the relative cwd — it only exists
            // below the root, so escaping upward is impossible by shape.
            if !rel.as_os_str().is_empty() {
                let mut up = rel.clone();
                up.pop();
                let target = up.to_string_lossy().into_owned();
                let wid = wid.clone();
                list = list.child(
                    div()
                        .text_sm()
                        .cursor_pointer()
                        .px_1()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                        .child("▸ ..")
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                this.files_edit_release(&wid);
                                this.widget_command(&wid, nav_state(target.clone()), cx);
                            }),
                        ),
                );
            }

            if entries.is_empty() {
                // The original's two empty states — a live query that
                // matched nothing reads differently from an empty folder.
                list = list.child(
                    div()
                        .text_sm()
                        .px_1()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(if query.is_empty() {
                            "(empty)"
                        } else {
                            "No files match search filter"
                        }),
                );
            }
            for entry in entries.into_iter().take(limit) {
                let path = cur.join(&entry.name);
                let abs = path.to_string_lossy().into_owned();
                let armed = confirm_delete == Some(abs.as_str());
                // `isRenaming` — this row's name cell becomes the input.
                let renaming_here = matches!(renaming, Some((p, _)) if p == abs.as_str());
                // Per-row group name drives the action strip's hover reveal —
                // the original's `opacity-0 group-hover` tailwind pair.
                let group = format!("files-row-{abs}");
                let icon = entry_icon(&entry.name, entry.is_dir);
                let size_cell = if entry.is_dir {
                    "—".to_owned()
                } else {
                    human_size(entry.size)
                };
                let mtime_cell = format_mtime(entry.mtime);

                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_1()
                    .cursor_pointer()
                    .group(group.clone())
                    .text_sm()
                    .text_color(rgb(theme::hex(if entry.is_dir {
                        theme::text::NORMAL
                    } else {
                        theme::text::DIM
                    })))
                    .hover(|el| el.bg(rgb(theme::hex(theme::monochrome::RAISED))))
                    .child(
                        div()
                            .flex_none()
                            .w(px(12.))
                            .flex()
                            .justify_center()
                            .text_color(rgb(theme::hex(if entry.is_dir {
                                theme::accent::SAND
                            } else {
                                theme::text::FAINT
                            })))
                            .child(icon),
                    );

                if renaming_here {
                    // The original's renaming cell: icon, the input,
                    // Save/Cancel. The buffer lives on `files_editing`
                    // (the `renamingName` state) — every keystroke
                    // re-renders, so Save reads the live text.
                    let buffer = renaming
                        .and_then(|(p, b)| (p == abs.as_str()).then(|| b.to_owned()))
                        .unwrap_or_default();
                    let wid_cancel = wid.clone();
                    row = row
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .flex_1()
                                .min_w_0()
                                .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                .child(div().min_w_0().child(buffer.clone()))
                                .child(
                                    div()
                                        .text_color(rgb(theme::hex(theme::text::DIM)))
                                        .child("▌".to_owned()),
                                ),
                        )
                        .child(row_button(
                            "save".to_owned(),
                            false,
                            &wid,
                            vec![json!({ "op": "file_rename", "path": abs, "name": buffer })],
                            cx,
                        ))
                        .child(
                            // Cancel — `renameCancelled`: drop the edit
                            // without dispatching. Direct field access like
                            // the search field's own click.
                            div()
                                .text_xs()
                                .cursor_pointer()
                                .px_0p5()
                                .text_color(rgb(theme::hex(theme::text::FAINT)))
                                .hover(|el| {
                                    el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                                })
                                .child("cancel")
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _event, _window, cx| {
                                        cx.stop_propagation();
                                        if matches!(
                                            &this.files_editing,
                                            Some(FilesEditing::Rename { id, .. }) if id == &wid_cancel
                                        ) {
                                            this.files_editing = None;
                                        }
                                        cx.notify();
                                    }),
                                ),
                        );
                    list = list.child(row);
                    continue;
                }

                row = row
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .child(div().min_w_0().truncate().child(entry.name.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(56.))
                            .flex()
                            .justify_end()
                            .text_xs()
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .child(size_cell),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(104.))
                            .flex()
                            .justify_end()
                            .text_xs()
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .child(mtime_cell),
                    );

                if entry.is_dir {
                    // Directories descend — a single Normal component can
                    // never escape the root.
                    let target = rel.join(&entry.name).to_string_lossy().into_owned();
                    let wid = wid.clone();
                    row = row.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            this.files_edit_release(&wid);
                            this.widget_command(&wid, nav_state(target.clone()), cx);
                        }),
                    );
                } else {
                    // `openItem` → `viewFile`: a file click previews — the
                    // overlay is the dispatcher's job, the pane stays pure.
                    let wid = wid.clone();
                    let abs = abs.clone();
                    row = row.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            this.widget_command(
                                &wid,
                                json!({ "op": "file_preview", "path": abs }),
                                cx,
                            );
                        }),
                    );
                }

                // Row action strip — the original's hover-revealed five:
                // preview (files only), open in the default app, copy path,
                // rename, delete. Hidden until the row is hovered, except
                // while its delete is armed so the second click has
                // somewhere visible to land.
                let delete_label = if armed { "✕?" } else { "✕" };
                let delete_actions = if armed {
                    vec![
                        json!({ "op": "file_delete", "path": abs }),
                        json!({ "op": "set", "state": { "confirmDelete": Value::Null } }),
                    ]
                } else {
                    vec![json!({ "op": "set", "state": { "confirmDelete": abs } })]
                };
                let rename_abs = abs.clone();
                let rename_name = entry.name.clone();
                let wid_rename = wid.clone();
                let mut strip = div()
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_0p5()
                    .opacity(if armed { 1.0 } else { 0.0 })
                    .group_hover(group, |s| s.opacity(1.0));
                if !entry.is_dir {
                    strip = strip.child(row_button(
                        "◉".to_owned(),
                        false,
                        &wid,
                        vec![json!({ "op": "file_preview", "path": abs })],
                        cx,
                    ));
                }
                strip = strip
                    .child(row_button(
                        "↗".to_owned(),
                        false,
                        &wid,
                        vec![json!({ "op": "file_open", "path": abs })],
                        cx,
                    ))
                    .child(row_button(
                        "⧉".to_owned(),
                        false,
                        &wid,
                        vec![json!({ "op": "file_copy_path", "path": abs })],
                        cx,
                    ))
                    .child(
                        // ✎ `startRename` — arms the inline edit; the buffer
                        // seeds with the row's current name.
                        div()
                            .text_xs()
                            .cursor_pointer()
                            .px_0p5()
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .hover(|el| {
                                el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                    .text_color(rgb(theme::hex(theme::text::NORMAL)))
                            })
                            .child("✎")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _event, window, cx| {
                                    cx.stop_propagation();
                                    window.focus(&this.focus, cx);
                                    this.files_editing = Some(FilesEditing::Rename {
                                        id: wid_rename.clone(),
                                        path: rename_abs.clone(),
                                        buffer: rename_name.clone(),
                                    });
                                    this.active = Some(wid_rename.clone());
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(row_button(
                        delete_label.to_owned(),
                        armed,
                        &wid,
                        delete_actions,
                        cx,
                    ));
                row = row.child(strip);
                list = list.child(row);
            }

            // The original's "Show more" — 200 rows a page, each click adds
            // another window until the cap `list_entries` already imposed.
            if total > limit {
                let wid = wid.clone();
                let next = limit + 200;
                list = list.child(
                    div()
                        .text_xs()
                        .cursor_pointer()
                        .px_1()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .hover(|el| {
                            el.bg(rgb(theme::hex(theme::monochrome::RAISED)))
                                .text_color(rgb(theme::hex(theme::text::DIM)))
                        })
                        .child(format!("Show more ({} hidden)", total - limit))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, _window, cx| {
                                this.widget_command(
                                    &wid,
                                    json!({ "op": "set", "state": { "limit": next } }),
                                    cx,
                                );
                            }),
                        ),
                );
            }
        }
        Err(e) => {
            list = list.child(
                div()
                    .text_sm()
                    .text_color(rgb(theme::hex(theme::status::DANGER)))
                    .child(format!("files: {e}")),
            );
        }
    }

    let mut pane = col.child(list).relative();
    // The original's `createPortal` preview dialog — app-covering there,
    // pane-covering here; its Escape lives in `CanvasView::on_key`.
    if let Some(preview) = preview.filter(|p| p.id == wid) {
        pane = pane.child(preview_overlay(preview, &wid, cx));
    }
    pane
}

/// The preview overlay — `FilesWidget`'s portal dialog scoped to the pane.
/// Header: file glyph, name, `size · path`, then "Open with OS App",
/// "Copy Text" (text previews only — the original's `content !==
/// undefined` gate), and the ✕ close. Body: `img()` contain for images
/// (the path renders directly — no base64 `dataUrl` hop needed natively),
/// scrolling mono text, or the binary-file notice. The root eats presses
/// so a click can't fall through to the listing, like the settings
/// modal's panel.
fn preview_overlay(
    preview: &FilesPreview,
    wid: &str,
    cx: &mut Context<crate::canvas_view::CanvasView>,
) -> impl IntoElement {
    let path = preview.path.clone();
    let mut actions = div().flex_none().flex().flex_row().items_center().gap_1();
    actions = actions.child(row_button(
        "↗ open".to_owned(),
        false,
        wid,
        vec![json!({ "op": "file_open", "path": path.clone() })],
        cx,
    ));
    if matches!(preview.body, FilesPreviewBody::Text(_)) {
        actions = actions.child(row_button(
            "⧉ copy".to_owned(),
            false,
            wid,
            vec![json!({ "op": "file_copy_content" })],
            cx,
        ));
    }
    actions = actions.child(row_button(
        "✕".to_owned(),
        false,
        wid,
        vec![json!({ "op": "file_preview_close" })],
        cx,
    ));

    let header = div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_none()
                .text_sm()
                .text_color(rgb(theme::hex(theme::accent::SAND)))
                .child("<>"),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_sm()
                        .text_color(rgb(theme::hex(theme::text::NORMAL)))
                        .child(preview.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_xs()
                        .text_color(rgb(theme::hex(theme::text::FAINT)))
                        .child(format!("{} · {}", human_size(preview.size), preview.path)),
                ),
        )
        .child(actions);

    let body: AnyElement = match &preview.body {
        FilesPreviewBody::Image => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                img(PathBuf::from(&preview.path))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .with_fallback(|| {
                        div()
                            .text_xs()
                            .text_color(rgb(theme::hex(theme::text::FAINT)))
                            .child("Image could not be loaded")
                            .into_any_element()
                    }),
            )
            .into_any_element(),
        FilesPreviewBody::Text(text) => div()
            .id(format!("slate-files-preview-text-{wid}"))
            .size_full()
            .overflow_y_scroll()
            .child(
                div()
                    .font_family("Departure Mono")
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::DIM)))
                    .child(text.clone()),
            )
            .into_any_element(),
        FilesPreviewBody::Binary => div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::hex(theme::text::FAINT)))
                    .child("Binary file preview not available. Open with external application."),
            )
            .into_any_element(),
    };

    div()
        .id(format!("slate-files-preview-{wid}"))
        .absolute()
        .top(px(0.0))
        .left(px(0.0))
        .right(px(0.0))
        .bottom(px(0.0))
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .bg(rgb(theme::hex(theme::monochrome::BASE)))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_this, _event, _window, cx| {
                cx.stop_propagation();
            }),
        )
        .child(header)
        .child(
            div()
                .flex_1()
                .min_h_0()
                .border_1()
                .border_color(rgb(theme::hex(theme::hairline::SOFT)))
                .child(body),
        )
}
