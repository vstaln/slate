use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// `MAX_MEDIA_BYTES` in media.ts — the media store refuses anything bigger.
pub const MAX_MEDIA_BYTES: u64 = 256 * 1024 * 1024;

pub fn path_token(path: &str) -> Result<String, String> {
    if path.chars().any(|ch| ch <= '\u{1f}' || ch == '\u{7f}') {
        return Err("Attachment path contains control characters".into());
    }
    Ok(format!("\"{}\" ", path.replace('"', "\\\"")))
}

/// media.ts `MIME_BY_EXT` — the `image` media kind.
const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp", "svg", "ico", "tif", "tiff", "heic",
];
/// `AUDIO_MIME_BY_EXT`.
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "wav", "ogg", "oga", "flac", "aac", "m4a", "opus", "weba", "wma",
];
/// `VIDEO_MIME_BY_EXT`.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "wmv", "flv", "ogv", "mpg", "mpeg",
];
/// The renderer's `TEXT_EXTS` (BrowserWidget.tsx) — markup, documents and
/// source files the shell's media viewer rendered in a sandboxed frame.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "json", "csv", "tsv", "yaml", "yml", "xml", "html", "htm", "log", "js",
    "ts", "jsx", "tsx", "py", "sh", "bat", "cmd", "ps1",
];

/// The shell's `DroppedMediaKind` — `mediaKindForName`/`mediaKindOf` bucket a
/// file into one of these; `doc` is the fallback for everything the viewer
/// could not render (archives, office documents, unknown extensions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Pdf,
    Text,
    Doc,
}

impl MediaKind {
    /// The wire spelling — the `kind` field on the original's
    /// `control:open-media` broadcast and `DroppedMedia.kind`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Pdf => "pdf",
            Self::Text => "text",
            Self::Doc => "doc",
        }
    }

    /// The widget a dropped file spawns. The shell's MediaViewer could render
    /// every kind except `doc` (its open-externally fallback); the native
    /// build has a real `image` pane, so images land there and the remaining
    /// viewable kinds stay `browser`. `Doc` has no viewer → `None`, which the
    /// drop path answers with a refusal notice.
    pub fn widget_kind(self) -> Option<&'static str> {
        match self {
            Self::Image => Some("image"),
            Self::Video | Self::Audio | Self::Pdf | Self::Text => Some("browser"),
            Self::Doc => None,
        }
    }
}

fn extension_of(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// `mediaKindForName`/`mediaKindOf` — image first, `doc` the fallback for an
/// extension no table knows.
pub fn classify_media(path: &Path) -> MediaKind {
    let ext = extension_of(path);
    if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return MediaKind::Image;
    }
    if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        return MediaKind::Video;
    }
    if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
        return MediaKind::Audio;
    }
    if ext == "pdf" {
        return MediaKind::Pdf;
    }
    if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return MediaKind::Text;
    }
    MediaKind::Doc
}

/// controlServer.ts `mediaKindOf` — the coarser five-bucket spelling the
/// `/canvas/media` route reported and broadcast: `hasDocExtension` folds the
/// whole doc table (which includes the text extensions) into `doc`.
pub fn media_kind_of(path: &Path) -> &'static str {
    match classify_media(path) {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        MediaKind::Pdf => "pdf",
        MediaKind::Text | MediaKind::Doc => "doc",
    }
}

/// media.ts `mimeTypeForPath`'s answer for an extension — the full
/// `MEDIA_MIME_BY_EXT` table (image + audio + video + doc), unknown
/// extensions falling back to `application/octet-stream`.
pub fn mime_type_for_extension(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "m4a" => "audio/mp4",
        "opus" => "audio/opus",
        "weba" => "audio/webm",
        "wma" => "audio/x-ms-wma",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "ogv" => "video/ogg",
        "mpg" | "mpeg" => "video/mpeg",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "yaml" | "yml" => "text/yaml",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "log" => "text/plain",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "doc" => "application/msword",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xls" => "application/vnd.ms-excel",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "ppt" => "application/vnd.ms-powerpoint",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        "gz" => "application/gzip",
        "js" | "jsx" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "py" => "text/x-python",
        "sh" => "text/x-sh",
        "bat" | "cmd" | "ps1" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// protocol.ts `ACTIVE_EXTS` — spellings the `orc://media` handler refused to
/// serve under their real type (they would execute markup/script inside the
/// shell). The original downgraded them to `text/plain` + an attachment
/// disposition; over the control socket only the type survives.
pub fn is_active_extension(ext: &str) -> bool {
    matches!(
        ext,
        "html" | "htm" | "svg" | "js" | "jsx" | "ts" | "tsx" | "xml" | "xhtml"
    )
}

/// media.ts `isLocalPath`: absolute, and not a `\\`/`//` share spelling. The
/// extended-drive `\\?\C:\` form is still a local path.
fn is_local_source(source: &Path) -> bool {
    if !source.is_absolute() {
        return false;
    }
    let Some(text) = source.to_str() else {
        return false;
    };
    let extended_drive = text.starts_with("\\\\?\\")
        && text
            .as_bytes()
            .get(4)
            .is_some_and(u8::is_ascii_alphabetic)
        && text.as_bytes().get(5) == Some(&b':')
        && text.as_bytes().get(6) == Some(&b'\\');
    if text.starts_with("\\\\") && !extended_drive {
        return false;
    }
    !text.starts_with("//")
}

/// `saveBytes`'s `safeExt`: `[a-z0-9_-]{1,16}` lowercased, else `bin` — the
/// content-addressed name never carries an exotic extension.
fn sanitize_extension(ext: &str) -> String {
    let clean = ext.trim_start_matches('.').to_ascii_lowercase();
    if !clean.is_empty()
        && clean.len() <= 16
        && clean
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        clean
    } else {
        "bin".to_owned()
    }
}

fn extension_for_path(path: &Path) -> String {
    sanitize_extension(&extension_of(path))
}

/// Publish `temporary` under `<sha256-of-contents>.<ext>` — the
/// content-addressed rename both import paths share. Because the name is the
/// hash, an existing target already holds these bytes; the temp file is just
/// dropped, matching `writeBytesIfMissing`'s concurrent-paste handling.
fn publish_hashed(
    directory: &Path,
    temporary: &Path,
    digest: &str,
    extension: &str,
) -> Result<PathBuf, String> {
    let target = directory.join(format!("{digest}.{extension}"));
    if target.is_file() {
        std::fs::remove_file(temporary).map_err(|e| e.to_string())?;
        return Ok(target);
    }
    match std::fs::rename(temporary, &target) {
        // Windows rename does not replace an existing file; handle both a
        // previous import and a concurrent one without turning an idempotent
        // paste into an error.
        Err(_) if target.is_file() => {
            std::fs::remove_file(temporary).map_err(|e| e.to_string())?;
        }
        Err(error) => return Err(error.to_string()),
        Ok(()) => {}
    }
    Ok(target)
}

/// media.ts `importFile` — copy a local file into the content-addressed
/// media store. Same guards as the original: absolute local path, a regular
/// file, under the 256 MB cap. `source` is streamed while hashing so the
/// import never buffers the whole file, and the name carries the sanitized
/// extension like `saveBytes`.
pub fn import_media(source: &Path, directory: &Path) -> Result<PathBuf, String> {
    if !is_local_source(source) {
        return Err("File path must be absolute and local".into());
    }
    let extension = extension_for_path(source);
    let mut input = std::fs::File::open(source).map_err(|e| e.to_string())?;
    let metadata = input.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("File must be a regular file".into());
    }
    if metadata.len() > MAX_MEDIA_BYTES {
        return Err("File exceeds 256 MB limit".into());
    }
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let temporary = directory.join(format!(".import-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<PathBuf, String> {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut total = 0u64;
        loop {
            let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_MEDIA_BYTES {
                return Err("File exceeds 256 MB limit".into());
            }
            hash.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|e| e.to_string())?;
        }
        output.sync_all().map_err(|e| e.to_string())?;
        drop(output);
        publish_hashed(directory, &temporary, &format!("{:x}", hash.finalize()), &extension)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// media.ts `saveBytes` — bytes that never lived on disk (a clipboard image)
/// land in the same content-addressed store.
pub fn save_bytes(bytes: &[u8], ext: &str, directory: &Path) -> Result<PathBuf, String> {
    if bytes.len() as u64 > MAX_MEDIA_BYTES {
        return Err("File exceeds 256 MB limit".into());
    }
    let extension = sanitize_extension(ext);
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    let target = directory.join(format!("{digest}.{extension}"));
    // The name is the content hash, so pasting the same image twice reuses
    // the first file and skips the write entirely.
    if target.is_file() {
        return Ok(target);
    }
    let temporary = directory.join(format!(".import-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<PathBuf, String> {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|e| e.to_string())?;
        output.write_all(bytes).map_err(|e| e.to_string())?;
        output.sync_all().map_err(|e| e.to_string())?;
        drop(output);
        publish_hashed(directory, &temporary, &digest, &extension)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// imageAttachments.ts `resolveImage` — the image-only import the attach and
/// `/canvas/image` paths share: same store, same path check first, but the
/// extension must classify as `image`.
pub fn import_image(source: &Path, directory: &Path) -> Result<PathBuf, String> {
    if !is_local_source(source) {
        return Err(format!(
            "image path must be absolute and local: {}",
            source.display()
        ));
    }
    if classify_media(source) != MediaKind::Image {
        return Err(format!("{} is not an image", source.display()));
    }
    import_media(source, directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quote_paths_without_allowing_terminal_controls() {
        assert_eq!(path_token("a b.png").unwrap(), "\"a b.png\" ");
        for path in ["a\rb", "a\nb", "a\x1bb", "a\0b"] {
            assert!(path_token(path).is_err());
        }
    }
    #[test]
    fn remote_or_relative_images_are_not_imported() {
        assert!(import_image(Path::new("https://example.com/a.png"), Path::new("unused")).is_err());
        assert!(import_image(Path::new("a.png"), Path::new("unused")).is_err());
    }

    #[test]
    fn classification_matches_the_shells_tables() {
        assert_eq!(classify_media(Path::new("a.png")), MediaKind::Image);
        assert_eq!(classify_media(Path::new("a.ICO")), MediaKind::Image);
        assert_eq!(classify_media(Path::new("a.mp4")), MediaKind::Video);
        assert_eq!(classify_media(Path::new("a.flac")), MediaKind::Audio);
        assert_eq!(classify_media(Path::new("a.pdf")), MediaKind::Pdf);
        assert_eq!(classify_media(Path::new("a.md")), MediaKind::Text);
        assert_eq!(classify_media(Path::new("a.tsx")), MediaKind::Text);
        assert_eq!(classify_media(Path::new("a.zip")), MediaKind::Doc);
        assert_eq!(classify_media(Path::new("a.unknownext")), MediaKind::Doc);
        assert_eq!(classify_media(Path::new("noextension")), MediaKind::Doc);
    }

    #[test]
    fn imported_images_survive_source_removal_and_repeated_import() {
        let root =
            std::env::temp_dir().join(format!("slate-attachments-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("sample.png");
        let media = root.join("media");
        let bytes = b"\x89PNG\r\n\x1a\nfixture";
        std::fs::write(&source, bytes).unwrap();
        let imported = import_image(&source, &media).unwrap();
        assert_eq!(import_image(&source, &media).unwrap(), imported);
        std::fs::remove_file(&source).unwrap();
        assert_eq!(std::fs::read(&imported).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 1);
        std::fs::remove_file(imported).unwrap();
        std::fs::remove_dir(media).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn import_media_takes_any_extension_and_sanitizes_the_name() {
        let root =
            std::env::temp_dir().join(format!("slate-attachments-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("notes.md");
        let media = root.join("media");
        std::fs::write(&source, b"# hello").unwrap();
        let imported = import_media(&source, &media).unwrap();
        assert!(imported.ends_with(
            "ea67f39f2a707e536439ee31e49fdd586b4a8437d3408f0466112d040cd06681.md"
        ));
        // A non-image imports through the general path but is refused by
        // the image-only wrapper.
        assert!(import_image(&source, &media).is_err());
        std::fs::remove_file(imported).unwrap();
        std::fs::remove_file(source).unwrap();
        std::fs::remove_dir(media).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn save_bytes_is_content_addressed_and_caps_at_256mb() {
        let root =
            std::env::temp_dir().join(format!("slate-attachments-test-{}", uuid::Uuid::new_v4()));
        let media = root.join("media");
        let first = save_bytes(b"png-bytes", "png", &media).unwrap();
        assert_eq!(save_bytes(b"png-bytes", "PNG", &media).unwrap(), first);
        let binned = save_bytes(b"x", ".weird!!", &media).unwrap();
        assert_eq!(binned.extension().and_then(|e| e.to_str()), Some("bin"));
        std::fs::remove_file(first).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
