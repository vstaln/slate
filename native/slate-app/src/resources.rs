//! Resource ids: `scheme:id`.
//!
//! Mirrors src/main/core/resources.ts. The scheme list is closed — an id whose
//! scheme is not in it is not a resource id at all, which is what stops a typo
//! from taking a lock on a namespace nothing else uses.

/// The schemes the bus recognises, in the order src/main/core/types.ts lists
/// them.
pub const RESOURCE_SCHEMES: [&str; 14] = [
    "widget", "note", "terminal", "file", "task", "canvas", "git", "run", "orctask", "dispatch",
    "gate", "plan", "search", "system",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedResource {
    pub scheme: String,
    pub id: String,
}

/// Splits on the *first* colon, so an id may itself contain one — a Windows
/// path in a `file:` resource does.
pub fn parse_resource(target: &str) -> Option<ParsedResource> {
    let at = target.find(':')?;
    if at == 0 {
        return None;
    }
    let (scheme, rest) = target.split_at(at);
    let id = &rest[1..];
    if !RESOURCE_SCHEMES.contains(&scheme) || id.is_empty() {
        return None;
    }
    Some(ParsedResource {
        scheme: scheme.to_owned(),
        id: id.to_owned(),
    })
}

pub fn is_resource_id(value: &str) -> bool {
    parse_resource(value).is_some()
}

pub fn resource_id(scheme: &str, id: &str) -> String {
    format!("{scheme}:{id}")
}

/// Normalises a filesystem path into a `file:` resource.
///
/// Equivalent Windows spellings share a lock key: separators become forward
/// slashes, drive letters are upper-cased, and drive/UNC paths are folded to
/// lowercase. POSIX and relative paths retain their case, and filesystem roots
/// keep their trailing slash (`/` and `C:/`).
pub fn file_resource(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let bytes = unified.as_bytes();
    let has_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let normalized = if has_drive {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        let rest = unified[2..].trim_end_matches('/');
        let suffix = if rest.is_empty() && unified[2..].starts_with('/') {
            "/".to_owned()
        } else if let Some(tail) = rest.strip_prefix('/') {
            format!("/{}", tail.to_lowercase())
        } else {
            rest.to_lowercase()
        };
        format!("{drive}:{suffix}")
    } else if unified.starts_with("//") {
        let path = unified.trim_end_matches('/');
        if path.is_empty() {
            "//".to_owned()
        } else {
            path.to_lowercase()
        }
    } else if unified.starts_with('/') {
        let path = unified.trim_end_matches('/');
        if path.is_empty() {
            "/".to_owned()
        } else {
            path.to_owned()
        }
    } else {
        unified.trim_end_matches('/').to_owned()
    };
    resource_id("file", &normalized)
}

/// `isLocalPath` — an absolute path that is not a UNC/network spelling.
/// On POSIX `isAbsolute` is exactly "starts with `/`"; the `//` guard keeps
/// the POSIX spelling of a UNC path out the way Node's check did.
pub fn is_local_path(path: &str) -> bool {
    if path.starts_with("\\\\") || path.starts_with("//") {
        return false;
    }
    std::path::Path::new(path).is_absolute()
}
