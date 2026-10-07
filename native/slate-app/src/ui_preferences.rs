use crate::arrange::ArrangeMode;
use crate::code_layout::CodeLayout;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::{io::Write, path::Path, path::PathBuf};

/// `orcspace-arrange-free-layout` — the rect one widget had before the last
/// non-free arrange, restored whole (including `maximized`) by Free mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FreeRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub maximized: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UiPreferences {
    pub sidebar_open: bool,
    pub auto_names: bool,
    pub layout: CodeLayout,
    pub three_way_split: [f32; 2],
    /// `orcspace-arrange-mode` — the TitleBar arrange pick, `free` default.
    pub arrange_mode: ArrangeMode,
    /// `orcspace-arrange-free-layout` — the pre-arrange snapshot; saved on
    /// the first non-free arrange and cleared when Free replays it.
    pub arrange_free_layout: HashMap<String, FreeRect>,
    /// `orcspace-flip-terminals` — the TitleBar toggle that swapped every
    /// terminal's xterm for its flip card; localStorage in the shell,
    /// `terminalsFlipped` here.
    pub terminals_flipped: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            sidebar_open: false,
            auto_names: true,
            layout: CodeLayout::Auto,
            three_way_split: [0.5, 0.5],
            arrange_mode: ArrangeMode::Free,
            arrange_free_layout: HashMap::new(),
            terminals_flipped: false,
            extra: Default::default(),
        }
    }
}

impl UiPreferences {
    pub fn path() -> PathBuf {
        crate::ipc::user_data_dir().join("native-ui.json")
    }

    /// The load path that never loses data to a torn file: a corrupt
    /// `native-ui.json` yields defaults without overwriting the bytes.
    pub fn load_or_default() -> Self {
        crate::ipc::read_store_recovered(&Self::path())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(error.to_string()),
        };
        let mut value: Self = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        for ratio in &mut value.three_way_split {
            *ratio = ratio.clamp(0.2, 0.8);
        }
        Ok(value)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let parent = path.parent().ok_or("Settings path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let temporary = parent.join(format!(".native-ui-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<(), String> {
            let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| error.to_string())?;
            drop(file);
            std::fs::rename(&temporary, path).map_err(|error| error.to_string())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_replace_atomically_and_keep_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("slate-ui-{}", uuid::Uuid::new_v4()));
        let path = dir.join("native-ui.json");
        let mut preferences = UiPreferences::load(&path).unwrap();
        preferences
            .extra
            .insert("futureSetting".into(), serde_json::json!({"enabled": true}));
        preferences.save(&path).unwrap();
        preferences.layout = CodeLayout::Focus;
        preferences.three_way_split = [0.3, 0.7];
        preferences.save(&path).unwrap();
        assert_eq!(UiPreferences::load(&path).unwrap(), preferences);
        std::fs::write(&path, "broken").unwrap();
        assert!(UiPreferences::load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
