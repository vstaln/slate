//! Resource locks with a TTL.
//!
//! Mirrors src/main/core/locks.ts. The shape that matters for parity: an
//! expired lock is *absent*, not present-and-stale. Every read path drops it on
//! sight, which is why a `release` by an actor whose lock has just expired
//! succeeds quietly instead of reporting `forbidden` about a holder that no
//! longer exists.
//!
//! The clock is injected rather than read so tests can step time instead of
//! sleeping, and so a replay is reproducible.

use crate::command::{CommandError, CommandResult, ErrorCode};
use crate::resources::parse_resource;
use indexmap::IndexMap;
use serde_json::json;

pub const DEFAULT_LOCK_TTL_MS: i64 = 30_000;
pub const MIN_LOCK_TTL_MS: i64 = 1_000;
pub const MAX_LOCK_TTL_MS: i64 = 10 * 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ResourceLock {
    pub resource: String,
    pub actor_id: String,
    pub acquired_at: i64,
    pub expires_at: i64,
    pub reason: Option<String>,
    /// Taken by the bus on the caller's behalf for the duration of one command,
    /// rather than asked for. Implicit locks are skipped by `heartbeat`: an
    /// agent keeping its own locks alive must not also extend one the bus took
    /// for a command that has since finished.
    pub implicit: bool,
}

impl ResourceLock {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "resource": self.resource,
            "actorId": self.actor_id,
            "acquiredAt": self.acquired_at,
            "expiresAt": self.expires_at,
            "reason": self.reason,
            "implicit": self.implicit,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockEvent {
    Acquired,
    Renewed,
    Released,
    Expired,
}

pub struct LockManager {
    locks: IndexMap<String, ResourceLock>,
    default_ttl: i64,
    /// Emitted events, in order, for callers that need to observe them. The
    /// TypeScript manager is an EventEmitter; nothing in the migrated code
    /// subscribes yet, so they are recorded rather than dispatched.
    events: Vec<(LockEvent, ResourceLock)>,
}

fn clamp_ttl(value: Option<i64>, default_ttl: i64) -> i64 {
    value
        .unwrap_or(default_ttl)
        .clamp(MIN_LOCK_TTL_MS, MAX_LOCK_TTL_MS)
}

pub struct AcquireInput<'a> {
    pub resource: &'a str,
    pub actor_id: &'a str,
    pub ttl_ms: Option<i64>,
    pub reason: Option<String>,
    pub implicit: bool,
}

/// `core.locks` in the TypeScript — one lock manager per process, shared by
/// the control server and the GUI. The canvas's own submits gate against the
/// same table the `/locks` routes administer, so a `user` drag and an `api`
/// command cannot both think they own `widget:x`.
pub fn shared() -> &'static std::sync::Mutex<LockManager> {
    static SHARED: std::sync::LazyLock<std::sync::Mutex<LockManager>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(LockManager::new(None)));
    &SHARED
}

impl LockManager {
    pub fn new(default_ttl_ms: Option<i64>) -> Self {
        Self {
            locks: IndexMap::new(),
            default_ttl: clamp_ttl(default_ttl_ms, DEFAULT_LOCK_TTL_MS),
            events: Vec::new(),
        }
    }

    pub fn take_events(&mut self) -> Vec<(LockEvent, ResourceLock)> {
        std::mem::take(&mut self.events)
    }

    pub fn acquire(&mut self, input: AcquireInput<'_>, now: i64) -> CommandResult<ResourceLock> {
        if parse_resource(input.resource).is_none() {
            return Err(CommandError::new(
                ErrorCode::Invalid,
                format!(
                    "\"{}\" is not a resource id (expected scheme:id)",
                    input.resource
                ),
            ));
        }
        let actor_id = input.actor_id.trim();
        if actor_id.is_empty() {
            return Err(CommandError::new(
                ErrorCode::Invalid,
                "actorId is required to take a lock",
            ));
        }

        let current = self.live(input.resource, now);
        if let Some(held) = &current {
            if held.actor_id != actor_id {
                return Err(CommandError::with_details(
                    ErrorCode::Locked,
                    format!("{} is locked by {}", input.resource, held.actor_id),
                    json!({ "lock": held.to_json() }),
                ));
            }
        }

        let lock = ResourceLock {
            resource: input.resource.to_owned(),
            actor_id: actor_id.to_owned(),
            // Re-acquiring keeps the original acquisition time: the lock was
            // never let go, only extended.
            acquired_at: current.as_ref().map(|c| c.acquired_at).unwrap_or(now),
            expires_at: now + clamp_ttl(input.ttl_ms, self.default_ttl),
            reason: input
                .reason
                .or_else(|| current.as_ref().and_then(|c| c.reason.clone())),
            // An explicit re-acquire promotes an implicit lock to explicit, but
            // an implicit re-acquire never demotes an explicit one.
            implicit: input.implicit && current.as_ref().map(|c| c.implicit).unwrap_or(true),
        };

        let event = if current.is_some() {
            LockEvent::Renewed
        } else {
            LockEvent::Acquired
        };
        self.locks.insert(lock.resource.clone(), lock.clone());
        self.events.push((event, lock.clone()));
        Ok(lock)
    }

    pub fn renew(
        &mut self,
        resource: &str,
        actor_id: &str,
        ttl_ms: Option<i64>,
        now: i64,
    ) -> CommandResult<ResourceLock> {
        let Some(current) = self.live(resource, now) else {
            return Err(CommandError::new(
                ErrorCode::NotFound,
                format!("{resource} is not locked"),
            ));
        };
        if current.actor_id != actor_id {
            return Err(CommandError::with_details(
                ErrorCode::Forbidden,
                format!("{resource} is held by {}", current.actor_id),
                json!({ "lock": current.to_json() }),
            ));
        }
        let expires_at = now + clamp_ttl(ttl_ms, self.default_ttl);
        let lock = self.locks.get_mut(resource).expect("live() found it");
        lock.expires_at = expires_at;
        let lock = lock.clone();
        self.events.push((LockEvent::Renewed, lock.clone()));
        Ok(lock)
    }

    /// Extends every *explicit* lock this actor holds. Returns how many were
    /// renewed.
    pub fn heartbeat(&mut self, actor_id: &str, ttl_ms: Option<i64>, now: i64) -> usize {
        let until = now + clamp_ttl(ttl_ms, self.default_ttl);
        let mut renewed = Vec::new();
        for lock in self.locks.values_mut() {
            if lock.actor_id != actor_id || lock.implicit || lock.expires_at <= now {
                continue;
            }
            lock.expires_at = until;
            renewed.push(lock.clone());
        }
        let count = renewed.len();
        for lock in renewed {
            self.events.push((LockEvent::Renewed, lock));
        }
        count
    }

    /// Releasing a lock that is not held is not an error — it is the state the
    /// caller wanted.
    pub fn release(&mut self, resource: &str, actor_id: &str, now: i64) -> CommandResult<()> {
        let Some(current) = self.live(resource, now) else {
            return Ok(());
        };
        if current.actor_id != actor_id {
            return Err(CommandError::with_details(
                ErrorCode::Forbidden,
                format!("{resource} is held by {}", current.actor_id),
                json!({ "lock": current.to_json() }),
            ));
        }
        self.locks.shift_remove(resource);
        self.events.push((LockEvent::Released, current));
        Ok(())
    }

    pub fn release_all_for(&mut self, actor_id: &str) -> Vec<ResourceLock> {
        let dropped: Vec<ResourceLock> = self
            .locks
            .values()
            .filter(|lock| lock.actor_id == actor_id)
            .cloned()
            .collect();
        for lock in &dropped {
            self.locks.shift_remove(&lock.resource);
            self.events.push((LockEvent::Released, lock.clone()));
        }
        dropped
    }

    pub fn release_all(&mut self) {
        let all: Vec<ResourceLock> = self.locks.values().cloned().collect();
        self.locks.clear();
        for lock in all {
            self.events.push((LockEvent::Released, lock));
        }
    }

    pub fn holder(&mut self, resource: &str, now: i64) -> Option<ResourceLock> {
        self.live(resource, now)
    }

    pub fn is_locked_by_other(&mut self, resource: &str, actor_id: &str, now: i64) -> bool {
        self.live(resource, now)
            .is_some_and(|lock| lock.actor_id != actor_id)
    }

    pub fn is_held_by(&mut self, resource: &str, actor_id: &str, now: i64) -> bool {
        self.live(resource, now)
            .is_some_and(|lock| lock.actor_id == actor_id)
    }

    /// Sweeps first, so the listing never shows a lock that has already lapsed.
    pub fn list(&mut self, now: i64) -> Vec<ResourceLock> {
        self.sweep(now);
        self.locks.values().cloned().collect()
    }

    pub fn sweep(&mut self, now: i64) -> Vec<ResourceLock> {
        let expired: Vec<ResourceLock> = self
            .locks
            .values()
            .filter(|lock| lock.expires_at <= now)
            .cloned()
            .collect();
        for lock in &expired {
            self.locks.shift_remove(&lock.resource);
            self.events.push((LockEvent::Expired, lock.clone()));
        }
        expired
    }

    /// A lock only counts while it has not expired; reading drops a lapsed one.
    fn live(&mut self, resource: &str, now: i64) -> Option<ResourceLock> {
        let lock = self.locks.get(resource)?.clone();
        if lock.expires_at > now {
            return Some(lock);
        }
        self.locks.shift_remove(resource);
        self.events.push((LockEvent::Expired, lock));
        None
    }
}
