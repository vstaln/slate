use crate::journal::JournalEntry;
use crate::planner::{self, PlannerState, Today};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// Snapshot adapter for the native planner. Refuses to overwrite externally
/// changed or newer-schema documents rather than silently discarding them.
pub struct PlannerDocument {
    path: PathBuf,
    original: Option<Vec<u8>>,
    document: Value,
    pub items: PlannerState,
}

impl PlannerDocument {
    pub fn recover(
        path: impl AsRef<Path>,
        log: &crate::journal_log::JournalLog,
        today: &Today,
    ) -> Result<Self, String> {
        let mut document = Self::open(path)?;
        let sequence = document.document["snapshotSeq"].as_u64().unwrap_or(0);
        if sequence > log.sequence()
            || log
                .entries()
                .first()
                .is_some_and(|entry| entry.seq > sequence.saturating_add(1))
        {
            return Err(
                "Planner and journal have a recovery gap; original files left unchanged".into(),
            );
        }
        for entry in log.entries() {
            document.apply(entry, today)?;
        }
        Ok(document)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_owned();
        let original = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        let document = match &original {
            Some(bytes) => serde_json::from_slice::<Value>(
                bytes.strip_prefix(&[239, 187, 191]).unwrap_or(bytes),
            )
            .map_err(|e| e.to_string())?,
            None => json!({"snapshotSeq": 0, "schemaVersion": 2, "items": []}),
        };
        if !document.is_object() || document.get("schemaVersion").and_then(Value::as_u64) != Some(2)
        {
            return Err("Unsupported planner schema; original file left unchanged".into());
        }
        let entries = document
            .get("items")
            .and_then(Value::as_array)
            .ok_or("Invalid planner items")?;
        let mut items = PlannerState::new();
        for entry in entries {
            let mut item: planner::PlanItem = serde_json::from_value(entry.clone())
                .map_err(|e| format!("Invalid planner item: {e}"))?;
            // revive() in the Electron store filled a missing/invalid status
            // from `done`, so every loaded item carried one.
            if item.status.is_none() {
                item.status = Some(if item.done {
                    planner::PlanStatus::Done
                } else {
                    planner::PlanStatus::Todo
                });
            }
            if item.id.is_empty() || item.title.trim().is_empty() || items.contains_key(&item.id) {
                return Err(
                    "Invalid or duplicate planner item; original file left unchanged".into(),
                );
            }
            items.insert(item.id.clone(), item);
        }
        Ok(Self {
            path,
            original,
            document,
            items,
        })
    }

    /// The journal sequence the on-disk items were folded through — entries
    /// at or below it are already reflected in `items`.
    pub fn snapshot_seq(&self) -> u64 {
        self.document["snapshotSeq"].as_u64().unwrap_or(0)
    }

    /// The read-only fold the panes use: snapshot items plus the journal tail,
    /// never written back — `recover` is the consolidating variant.
    pub fn folded_items(
        &self,
        log: &crate::journal_log::JournalLog,
        today: &Today,
    ) -> Result<PlannerState, String> {
        let snapshot_seq = self.snapshot_seq();
        let mut items = self.items.clone();
        for entry in log.entries() {
            if entry.phase != "commit" || entry.seq <= snapshot_seq {
                continue;
            }
            items = planner::reduce(&items, entry, today).map_err(|e| e.0)?;
        }
        Ok(items)
    }

    pub fn apply(&mut self, event: &JournalEntry, today: &Today) -> Result<(), String> {
        if event.phase != "commit"
            || event.seq <= self.document["snapshotSeq"].as_u64().unwrap_or(0)
        {
            return Ok(());
        }
        let next = planner::reduce(&self.items, event, today).map_err(|e| e.0)?;
        let mut document = self.document.clone();
        // Keep unknown fields on both the envelope and surviving items.
        let old = document["items"]
            .as_array()
            .ok_or("Invalid planner document")?;
        let values: Vec<Value> = next
            .values()
            .map(|item| {
                let mut value = old
                    .iter()
                    .find(|entry| entry["id"].as_str() == Some(&item.id))
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let object = value.as_object_mut().expect("validated planner item");
                for key in ["project", "day", "time", "attachments", "status"] {
                    object.remove(key);
                }
                object.extend(
                    serde_json::to_value(item)
                        .expect("finite planner fields")
                        .as_object()
                        .unwrap()
                        .clone(),
                );
                value
            })
            .collect();
        document["items"] = Value::Array(values);
        document["snapshotSeq"] = Value::from(event.seq);
        let bytes = crate::jsjson::to_js_json_pretty(&document, 2).into_bytes();
        let current = match std::fs::read(&self.path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        if current != self.original {
            return Err("Planner changed on disk; reopen before saving".into());
        }
        let parent = self
            .path
            .parent()
            .ok_or("Planner file has no parent directory")?;
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let temporary = parent.join(format!(".planner-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temporary, &self.path)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary);
            return Err(error.to_string());
        }
        self.original = Some(bytes);
        self.document = document;
        self.items = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64, kind: &str, payload: Value) -> JournalEntry {
        JournalEntry {
            seq,
            at: 1000 + seq,
            phase: "commit".into(),
            actor_id: "user".into(),
            command_id: None,
            entry_type: kind.into(),
            target: "plan:test".into(),
            payload: Some(payload),
            version: Some(seq as i64),
            error: None,
            prev_hash: None,
            hash: None,
            workspace_dir: None,
        }
    }

    #[test]
    fn committed_snapshot_reopens_without_replaying_older_changes() {
        let root = std::env::temp_dir().join(format!("slate-plan-test-{}", uuid::Uuid::new_v4()));
        let path = root.join("workspace-planner.json");
        let today = Today("2026-09-19".into());
        let mut planner = PlannerDocument::open(&path).unwrap();
        planner
            .apply(
                &event(1, "plan.create", json!({"title": "Task", "day": "today"})),
                &today,
            )
            .unwrap();
        planner
            .apply(&event(2, "plan.toggle", json!({"done": true})), &today)
            .unwrap();
        let mut reopened = PlannerDocument::open(&path).unwrap();
        assert!(reopened.items["test"].done);
        reopened
            .apply(&event(1, "plan.toggle", json!({"done": false})), &today)
            .unwrap();
        assert!(reopened.items["test"].done);
        assert_eq!(reopened.document["snapshotSeq"], 2);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn newer_schema_and_external_changes_are_not_overwritten() {
        let root = std::env::temp_dir().join(format!("slate-plan-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("workspace-planner.json");
        std::fs::write(&path, br#"{"schemaVersion":3,"items":[]}"#).unwrap();
        assert!(PlannerDocument::open(&path).is_err());
        std::fs::write(&path, br#"{"schemaVersion":2,"snapshotSeq":0,"items":[]}"#).unwrap();
        let mut planner = PlannerDocument::open(&path).unwrap();
        let external = br#"{"schemaVersion":2,"snapshotSeq":9,"items":[],"external":true}"#;
        std::fs::write(&path, external).unwrap();
        assert!(planner
            .apply(
                &event(1, "plan.create", json!({"title":"Task"})),
                &Today("2026-09-19".into())
            )
            .is_err());
        assert!(planner.items.is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), external);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
