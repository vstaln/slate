//! `workspace-state.json` — the shell's `appState.ts` workspace slice: the
//! directory every terminal spawns into plus the recents list the picker
//! showed. Code-workspace groups and view/settings keys live in the same
//! file in the original; unknown fields are preserved on rewrite so a file
//! written by the fuller schema round-trips.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// `MAX_RECENT` — the picker's cap before pinning is the only way to keep
/// an entry.
const MAX_RECENT: usize = 12;

#[derive(Clone, Debug)]
pub struct RecentDir {
    pub path: String,
    pub name: String,
    pub pinned: bool,
    pub last_opened_at: f64,
}

/// The persisted shape: `{workspaceDir, recent:[…], …}` — every field not
/// managed here is kept verbatim in `extra`.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceState {
    pub workspace_dir: Option<String>,
    pub recent: Vec<RecentDir>,
    extra: Map<String, Value>,
}

impl WorkspaceState {
    pub fn path() -> PathBuf {
        crate::ipc::user_data_dir().join("workspace-state.json")
    }

    pub fn load_from(path: &Path) -> Self {
        let Some(bytes) = crate::ipc::read_store_recovered(path) else {
            return Self::default();
        };
        let Ok(Value::Object(mut raw)) = serde_json::from_slice::<Value>(&bytes) else {
            return Self::default();
        };
        let workspace_dir = raw
            .remove("workspaceDir")
            .and_then(|v| v.as_str().map(str::to_owned))
            .filter(|v| !v.is_empty());
        let recent = raw
            .remove("recent")
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                let path = entry.get("path")?.as_str()?.to_owned();
                Some(RecentDir {
                    name: entry
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            Path::new(&path)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| path.clone())
                        }),
                    pinned: entry
                        .get("pinned")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    last_opened_at: entry
                        .get("lastOpenedAt")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                    path,
                })
            })
            .collect();
        Self {
            workspace_dir,
            recent,
            extra: raw,
        }
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    fn to_json(&self) -> Value {
        let mut map = self.extra.clone();
        match &self.workspace_dir {
            Some(dir) => {
                map.insert("workspaceDir".into(), Value::from(dir.clone()));
            }
            None => {
                map.remove("workspaceDir");
            }
        }
        map.insert(
            "recent".into(),
            Value::Array(
                self.recent
                    .iter()
                    .map(|r| {
                        json!({
                            "path": r.path,
                            "name": r.name,
                            "pinned": r.pinned,
                            "lastOpenedAt": r.last_opened_at,
                        })
                    })
                    .collect(),
            ),
        );
        Value::Object(map)
    }

    /// `sortedRecent` — pinned first, then most recently opened.
    fn sorted(&self) -> Vec<RecentDir> {
        let mut sorted = self.recent.clone();
        sorted.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.last_opened_at.total_cmp(&a.last_opened_at))
        });
        sorted
    }

    /// `trim` — keep at most MAX_RECENT entries; pinned entries never fall
    /// off the back.
    fn trim(&mut self) {
        let keep: Vec<String> = self
            .sorted()
            .into_iter()
            .take(MAX_RECENT)
            .map(|r| r.path)
            .collect();
        self.recent.retain(|r| r.pinned || keep.contains(&r.path));
    }

    /// `setWorkspaceDir` — point the app at `dir` (or clear it), bump the
    /// matching recent entry, and persist. Returns whether anything changed.
    pub fn set_workspace_dir(&mut self, dir: Option<String>) -> bool {
        let dir = dir.filter(|d| !d.is_empty());
        if self.workspace_dir == dir {
            return false;
        }
        self.workspace_dir = dir.clone();
        if let Some(dir) = dir {
            let now = now_ms();
            match self.recent.iter_mut().find(|r| r.path == dir) {
                Some(entry) => entry.last_opened_at = now,
                None => {
                    let name = Path::new(&dir)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| dir.clone());
                    self.recent.push(RecentDir {
                        path: dir,
                        name,
                        pinned: false,
                        last_opened_at: now,
                    });
                }
            }
            self.trim();
        }
        true
    }

    pub fn pin_recent(&mut self, path: &str) {
        if let Some(entry) = self.recent.iter_mut().find(|r| r.path == path) {
            entry.pinned = !entry.pinned;
        }
    }

    pub fn forget_recent(&mut self, path: &str) {
        self.recent.retain(|r| r.path != path);
    }

    /// The picker's rename affordance — display name only. Mirrors the
    /// shell's `renameRecent` validation: ≤80 chars, not `.`/`..`, no
    /// `<>:"/\|?*`, no trailing dot or space.
    pub fn rename_recent(&mut self, path: &str, name: &str) {
        let name = name.trim();
        let valid = !name.is_empty()
            && name.len() <= 80
            && name != "."
            && name != ".."
            && !name.chars().any(|c| "<>:\"/\\|?*".contains(c))
            && !name.ends_with('.')
            && !name.ends_with(' ');
        if !valid {
            return;
        }
        if let Some(entry) = self.recent.iter_mut().find(|r| r.path == path) {
            entry.name = name.to_owned();
        }
    }

    pub fn save(&self) {
        let bytes = crate::jsjson::to_js_json_pretty(&self.to_json(), 2).into_bytes();
        let _ = crate::ipc::write_file_atomic(&Self::path(), &bytes);
    }

    pub fn to_value(&self) -> Value {
        json!({
            "workspaceDir": self.workspace_dir,
            "recent": self.sorted().iter().map(|r| json!({
                "path": r.path,
                "name": r.name,
                "pinned": r.pinned,
                "lastOpenedAt": r.last_opened_at,
            })).collect::<Vec<_>>(),
        })
    }
}

/// `getDir` — the workspace the app is anchored to, `None` until one is
/// picked (the picker's empty state, which folds into the
/// `__no-workspace__` canvas slot).
pub fn current() -> Option<String> {
    WorkspaceState::load().workspace_dir
}

/// Startup hydration: a picked workspace persists; a fresh install adopts
/// the launch directory, which is what `slate --gui` from a project means.
/// Either way the dir lands in `recent` so the list has a first entry.
/// Adoption also claims the pre-workspace journal entries — untagged
/// deltas belong to the only workspace this install has ever known.
pub fn ensure() -> Option<String> {
    let mut state = WorkspaceState::load();
    if state.workspace_dir.is_none() {
        let dir = std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().into_owned())?;
        let journal_path = crate::ipc::user_data_dir().join("command-journal.ndjson");
        if let Ok(mut log) = crate::journal_log::JournalLog::open(&journal_path) {
            if let Err(error) = log.retag_workspace(&dir) {
                eprintln!("workspace adoption: journal retag failed: {error}");
            }
        }
        state.set_workspace_dir(Some(dir));
        state.save();
    }
    state.workspace_dir
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}
