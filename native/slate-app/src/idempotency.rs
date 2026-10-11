//! Remembering what a retried command already answered.
//!
//! Mirrors src/main/core/idempotency.ts. An agent that retries after a dropped
//! connection must get the first answer back, not run the command twice — so a
//! replayed result is marked `cached` and the handler is never re-entered.
//!
//! The TypeScript tracks an in-flight Promise so a retry arriving *during* the
//! first run joins it rather than starting a second. Rust has no promise to
//! hand back here, so the same state is modelled as a `Pending` marker: callers
//! see that the key is already being worked on and can decide to wait or refuse,
//! and — importantly for parity — pruning still refuses to evict it.

use indexmap::IndexMap;
use serde_json::Value;

pub const DEFAULT_MAX_ENTRIES: usize = 2_000;
pub const DEFAULT_TTL_MS: i64 = 10 * 60_000;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A command with this key is running; its answer is not known yet.
    Pending,
    /// The answer a retry should be given back, already marked `cached`.
    Done(Value),
}

#[derive(Debug, Clone)]
struct Record {
    at: i64,
    entry: Entry,
}

pub struct IdempotencyCache {
    entries: IndexMap<String, Record>,
    max_entries: usize,
    ttl_ms: i64,
}

impl Default for IdempotencyCache {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ENTRIES, DEFAULT_TTL_MS)
    }
}

/// `flow.idempotency` in the TypeScript — the process-wide answered-command
/// cache. The HTTP layer and the GUI share it: a `slate` retry and a canvas
/// submit with the same key replay the same recorded result.
pub fn shared() -> &'static std::sync::Mutex<IdempotencyCache> {
    static SHARED: std::sync::LazyLock<std::sync::Mutex<IdempotencyCache>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(IdempotencyCache::default()));
    &SHARED
}

impl IdempotencyCache {
    pub fn new(max_entries: usize, ttl_ms: i64) -> Self {
        Self {
            entries: IndexMap::new(),
            max_entries,
            ttl_ms,
        }
    }

    /// Reading drops a lapsed entry, so a key past its TTL is simply unknown.
    pub fn get(&mut self, key: &str, now: i64) -> Option<Entry> {
        let record = self.entries.get(key)?;
        if now - record.at > self.ttl_ms {
            self.entries.shift_remove(key);
            return None;
        }
        Some(record.entry.clone())
    }

    pub fn has(&mut self, key: &str, now: i64) -> bool {
        self.get(key, now).is_some()
    }

    /// Marks a key as being worked on.
    pub fn track(&mut self, key: &str, now: i64) {
        self.prune(now);
        self.entries.insert(
            key.to_owned(),
            Record {
                at: now,
                entry: Entry::Pending,
            },
        );
    }

    /// Records the answer. The stored copy carries `cached: true` so a retry can
    /// tell a replay from a fresh run — the TypeScript spreads the same flag in.
    pub fn set(&mut self, key: &str, result: Value, now: i64) {
        self.prune(now);
        let mut stored = result;
        if let Some(object) = stored.as_object_mut() {
            object.insert("cached".into(), Value::Bool(true));
        }
        self.entries.insert(
            key.to_owned(),
            Record {
                at: now,
                entry: Entry::Done(stored),
            },
        );
    }

    /// A command that failed leaves no record: the retry should actually retry.
    pub fn forget(&mut self, key: &str) {
        self.entries.shift_remove(key);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn size(&self) -> usize {
        self.entries.len()
    }

    /// Drops everything past its TTL, then — only if still at capacity — evicts
    /// completed entries oldest-first. A pending entry is never evicted: losing
    /// it would let the command it stands for run a second time, which is the
    /// one thing this cache exists to prevent.
    fn prune(&mut self, now: i64) {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, record)| now - record.at > self.ttl_ms)
            .map(|(key, _)| key.clone())
            .collect();
        for key in expired {
            self.entries.shift_remove(&key);
        }
        if self.entries.len() < self.max_entries {
            return;
        }
        let evictable: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, record)| record.entry != Entry::Pending)
            .map(|(key, _)| key.clone())
            .collect();
        for key in evictable {
            self.entries.shift_remove(&key);
            if self.entries.len() < self.max_entries {
                return;
            }
        }
    }
}
