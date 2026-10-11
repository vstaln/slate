//! The control-server route surface, as a pure function.
//!
//! Block 4. `route` maps a request to a response without touching a socket, so
//! every shape below can be tested directly — and wired into axum by a thin
//! adapter that does nothing but move bytes. The TypeScript equivalent is the
//! `route()` in src/main/controlServer.ts.
//!
//! What is *not* here, deliberately: the loopback guard, the control-token
//! check and the rate limiter. Those run before routing in the TypeScript too
//! (`isTrustedCaller` gates `route`), and keeping them out of the router means
//! the authorization decision stays in one place instead of being repeated per
//! route — which is exactly the duplication that makes an auth bug possible.
//!
//! Response shapes are the contract: `slate --json` parses them. A renamed field
//! is a breaking change for every agent, so the spellings mirror the
//! TypeScript exactly, including that a rejection carries `error` and `code`
//! while an acceptance carries `ok`, `version`, `seq` and `data`.

use crate::command::{CommandError, ErrorCode};
use crate::orchestration::{OrchestrationStore, MESSAGE_TYPES, OUTCOMES, TASK_STATUSES};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: IndexMap<String, String>,
    pub body: Value,
    /// From `?agentId=` or the `x-agent-id` header, whichever the caller used.
    pub agent_id: Option<String>,
}

impl Request {
    pub fn get(method: &str, path: &str) -> Self {
        Self {
            method: method.to_owned(),
            path: path.to_owned(),
            query: IndexMap::new(),
            body: Value::Null,
            agent_id: None,
        }
    }

    pub fn with_query(mut self, key: &str, value: &str) -> Self {
        self.query.insert(key.to_owned(), value.to_owned());
        self
    }

    pub fn with_body(mut self, body: Value) -> Self {
        self.body = body;
        self
    }

    pub fn as_agent(mut self, agent_id: &str) -> Self {
        self.agent_id = Some(agent_id.to_owned());
        self
    }

    fn segments(&self) -> Vec<String> {
        self.path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(percent_decode)
            .collect()
    }

    fn param(&self, key: &str) -> Option<&str> {
        self.query
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn flag(&self, key: &str) -> bool {
        self.param(key) == Some("1")
    }

    fn field(&self, key: &str) -> Option<&Value> {
        self.body.get(key).filter(|value| !value.is_null())
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.field(key).and_then(Value::as_str)
    }

    fn strings(&self, key: &str) -> Vec<String> {
        self.field(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    pub fn json(status: u16, body: Value) -> Self {
        Self { status, body }
    }

    /// A rejection, shaped as the TypeScript `reply()` shapes it: the message
    /// under `error`, the machine-readable code beside it, and any details the
    /// error carried merged in at the top level rather than nested.
    pub fn from_error(error: &CommandError) -> Self {
        let mut body = Map::new();
        body.insert("error".into(), Value::from(error.message.clone()));
        body.insert("code".into(), Value::from(error.code.as_str()));
        if let Some(Value::Object(details)) = &error.details {
            for (key, value) in details {
                body.insert(key.clone(), value.clone());
            }
        }
        Self {
            status: error.code.http_status(),
            body: Value::Object(body),
        }
    }

    /// An acceptance, shaped as `reply()` shapes it.
    fn accepted(status: u16, version: u64, seq: u64, data: Value) -> Self {
        Self::json(
            status,
            json!({ "ok": true, "version": version, "seq": seq, "data": data }),
        )
    }

    /// A byte payload in the only shape the wire adapter can carry: `body`
    /// always serializes as JSON, so the bytes ride base64 inside a `data:`
    /// URL — the same string the Electron `media.dataUrl` IPC answered — and
    /// `type` carries what a raw-body route would have sent as
    /// `Content-Type`. `GET /media/{name}` is the consumer.
    pub fn bytes(status: u16, bytes: &[u8], content_type: &str) -> Self {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Self::json(
            status,
            json!({
                "type": content_type,
                "size": bytes.len(),
                "encoding": "base64",
                "dataUrl": format!("data:{content_type};base64,{encoded}"),
            }),
        )
    }
}

/// Minimal percent-decoding for path segments. Ids are generated and contain no
/// reserved characters, but a caller may still encode one.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn invalid(message: impl Into<String>) -> Response {
    Response::from_error(&CommandError::new(ErrorCode::Invalid, message))
}

fn not_found(message: impl Into<String>) -> Response {
    Response::from_error(&CommandError::new(ErrorCode::NotFound, message))
}

/// Routes the CLI already knows how to call but only the running app can
/// answer — the pure store sees neither the canvas, nor the renderer, nor the
/// code-workspace list. Rather than dropping to a bare 404 (which reads as
/// "unknown"), the route exists and names the missing piece, so the CLI fails
/// honestly until the axum side lands. `needsEngineHandler` is the flag the
/// coordinator wires on.
fn needs_engine_handler(route: &str, what: &str) -> Response {
    Response::from_error(&CommandError::with_details(
        ErrorCode::Failed,
        format!("{route} is routed but not yet wired in the app — {what}"),
        json!({ "needsEngineHandler": true, "route": route }),
    ))
}

pub struct RouteDeps<'a> {
    pub orchestration: &'a mut OrchestrationStore,
    pub app_version: &'a str,
    pub workspace_dir: Option<&'a str>,
    pub now: i64,
    /// The running app's environment — journal, terminals, locks, the data
    /// dir. `None` in unit tests, where the routes that need it answer
    /// `needs_engine_handler` rather than a bare 404.
    pub env: Option<&'a mut dyn RouteEnv>,
}

/// The seam between the pure router and the live app. Everything the engine
/// owns and the router cannot fake — the PTY table, the lock manager, the
/// journal on disk — is lent through this one trait, so the router itself
/// stays a function a test can drive.
pub trait RouteEnv: Send {
    /// Open `command-journal.ndjson` with the hash chain verified — the same
    /// object the GUI and the CLI write through, so commits from the socket
    /// replay identically.
    fn journal(&mut self) -> Result<crate::journal_log::JournalLog, String>;
    /// `terminals.list()` rows: `{id, name, title, alive, cwd, kind}`.
    fn terminals(&mut self) -> Vec<Value>;
    /// `terminals.has(id)` — membership by raw id, no name resolution.
    fn terminal_exists(&mut self, id: &str) -> bool;
    /// `terminals.dispose` — kill the PTY for `terminal.dispose`.
    fn dispose_terminal(&mut self, id: &str) -> Result<(), String>;
    /// `terminals.setTitle` — `widget.update` on a shell also renames it.
    fn set_terminal_title(&mut self, id: &str, title: &str);
    /// `terminals.resolve` — a name or raw id → canonical terminal id,
    /// `None` when nothing matches.
    fn resolve_terminal(
        &mut self,
        target: &str,
        caller: Option<&str>,
    ) -> Result<String, CommandError>;
    /// `terminals.resolveWriteTarget` — an empty/`new` target floats to the
    /// preferred running terminal, then the most recently spawned live one.
    /// `None` when nothing is running.
    fn resolve_write_target(&mut self, requested: &str) -> Option<String>;
    /// The next free `term-*` id — `terminal.create` journals the id before
    /// the PTY exists, so the route has to mint it ahead of the spawn.
    fn next_terminal_id(&mut self, prefix: &str) -> String;
    /// Open a fresh shell under `id` — the dispatch path's `terminals.reserve`.
    /// The canvas's orphan-adoption turns it into a widget on the next tick.
    fn spawn_terminal(&mut self, id: &str, title: &str, cwd: &str) -> Result<(), String>;
    /// `terminals.isRunning` — the PTY reached its read loop.
    fn terminal_running(&mut self, id: &str) -> bool;
    /// A line of text into the PTY with confirmed delivery; `press_enter`
    /// submits it. `Err` means the terminal is not reading input.
    fn write_terminal(&mut self, id: &str, text: &str, press_enter: bool) -> Result<(), String>;
    /// The last `max` bytes of scrollback — the launch-failure sniff a
    /// dispatch runs right after starting the agent command.
    fn tail_terminal(&mut self, id: &str, max: usize) -> Option<String>;
    /// The live resource-lock table for `/locks`. The guard, not the
    /// manager: the engine keeps it behind an `Arc<Mutex>`, so the borrow
    /// arrives already locked.
    fn locks(&self) -> std::sync::MutexGuard<'_, crate::locks::LockManager>;
    /// The user data dir — `workspace-notes.json` and `media/` beneath it.
    fn data_dir(&self) -> std::path::PathBuf;
    /// The socket path the app listens on, for the `/snapshot` presence block.
    fn socket_path(&self) -> Option<String>;
    /// `versionOf(target)` — the version the store registries would report for
    /// this target right now: the folded object's `version` for the schemes
    /// that track one, 0 for everything else (terminal/git/file have no
    /// registry — `flow.registerVersions` never named them).
    fn target_version(&mut self, target: &str) -> u64;
    /// The server's cross-request idempotency cache — `submit` consults it
    /// before the gates so a retry is answered, not re-run.
    fn idempotency(
        &mut self,
    ) -> std::sync::MutexGuard<'_, crate::idempotency::IdempotencyCache>;
    /// `rateLimiter.tryConsume(actorId)` — `submit()` throttled
    /// non-privileged actors after the idempotency check and before any gate.
    /// HTTP agents are never privileged (the original reserved `user` and
    /// `system` for internal callers), so every submit consumes budget.
    /// `false` = over the limit → 429.
    fn rate_limit(&mut self, actor: &str, now: i64) -> bool;
}

/// Borrow the environment or answer like the half-wired TypeScript did —
/// the route exists, the handler is honest about the missing piece.
fn environment<'a, 'b>(
    deps: &'a mut RouteDeps<'b>,
    route: &str,
    what: &str,
) -> Result<&'a mut (dyn RouteEnv + 'b), Response> {
    deps.env
        .as_deref_mut()
        .ok_or_else(|| needs_engine_handler(route, what))
}

/// The actor a journal entry belongs to — `submit()`'s `actorId`.
fn actor(request: &Request) -> &str {
    request.agent_id.as_deref().unwrap_or("api")
}

/// `reply()` — the `{ok, version, seq, data}` envelope a flow result got.
fn accepted(entry: &crate::journal::JournalEntry, status: u16, data: Value) -> Response {
    Response::json(
        status,
        json!({
            "ok": true,
            "version": entry.version.unwrap_or(0),
            "seq": entry.seq,
            "data": data,
        }),
    )
}

/// The `registerDefinition` flags that decide which gates a journaled command
/// answers to — `(requires_lock, ignore_version)` per command type, from the
/// TypeScript registrations. Anything unlisted takes the registration
/// defaults: locked, versioned.
fn command_flags(entry_type: &str) -> (bool, bool) {
    match entry_type {
        // requiresLock: false — git reads and the terminal hot path.
        "git.refresh" | "git.branches" | "git.log" | "terminal.input" | "terminal.resize" => {
            (false, true)
        }
        // ignoreVersion — creates, recolors, and the terminal/git/
        // orchestration verbs whose targets no version registry tracks.
        "widget.create" | "canvas.camera" | "canvas.import" | "terminal.create"
        | "terminal.spawn" | "terminal.write" | "terminal.attach" | "terminal.dispose"
        | "plan.create" | "note.create" | "note.recolor" | "git.commit" | "git.checkout"
        | "git.create-branch" | "run.create" | "run.close" | "orctask.create"
        | "dispatch.start" | "dispatch.settle" | "dispatch.account" | "orc.send" | "orc.ack"
        | "gate.create" | "gate.resolve" | "file.write" | "file.create" | "file.mkdir"
        | "file.delete" | "file.rename" => (true, true),
        _ => (true, false),
    }
}

/// `resolveResultVersion` — the `version` a commit entry stamps. Updates and
/// canvas writes record the target's *post*-write version (its registry was
/// bumped by the handler), creates record the fresh object's 1, deletes record
/// the version the object died at, and schemes no registry tracked (terminal,
/// git, file) record 0.
fn resolved_commit_version(entry_type: &str, current_version: u64) -> i64 {
    match entry_type {
        "plan.create" | "widget.create" | "note.create" | "run.create" | "orctask.create"
        | "dispatch.start" | "gate.create" => 1,
        "plan.delete" | "note.delete" | "widget.remove" => current_version as i64,
        // Mail rides the run's resource but settles nothing there — the
        // run's version is unchanged, so the commit records it as read.
        "orc.send" | "orc.ack" | "note.recolor" => current_version as i64,
        t if t.starts_with("terminal.") || t.starts_with("git.") || t.starts_with("file.") => 0,
        _ => current_version as i64 + 1,
    }
}

/// The orchestration store's version registries — `registerVersions('run' |
/// 'orctask' | 'dispatch' | 'gate', …)` in the TypeScript. They live in
/// `deps.orchestration`, not the journal, so the gate reads them there; a
/// missing entity reports 0 like an unwritten registry key did.
fn orchestration_target_version(
    store: &crate::orchestration::OrchestrationStore,
    target: &str,
) -> Option<u64> {
    let parsed = crate::resources::parse_resource(target)?;
    if parsed.id == "new" {
        return Some(0);
    }
    Some(match parsed.scheme.as_str() {
        "run" => store.require_run(&parsed.id).map(|r| r.version).unwrap_or(0),
        "orctask" => store.require_task(&parsed.id).map(|t| t.version).unwrap_or(0),
        "dispatch" => store
            .require_dispatch(&parsed.id)
            .map(|d| d.version)
            .unwrap_or(0),
        "gate" => store.require_gate(&parsed.id).map(|g| g.version).unwrap_or(0),
        _ => return None,
    })
}

/// `flow.versionOf(target)` rebuilt from the journal fold — the same answer
/// the store registries gave the gate, since both seed from committed entries.
/// Schemes without a store (`terminal`, `git`, `file`) answer 0; the
/// orchestration schemes answer from `orchestration_target_version` instead.
pub fn folded_target_version(
    entries: &[crate::journal::JournalEntry],
    data_dir: &std::path::Path,
    target: &str,
) -> u64 {
    let Some(parsed) = crate::resources::parse_resource(target) else {
        return 0;
    };
    // `*:new` is the create target — nothing exists at it yet.
    if parsed.id == "new" {
        return 0;
    }
    match parsed.scheme.as_str() {
        "plan" => crate::planner::fold(
            entries,
            crate::planner::PlannerState::default(),
            &crate::planner::today_local(),
        )
        .ok()
        .and_then(|items| items.get(&parsed.id).map(|item| item.version as u64))
        .unwrap_or(0),
        "widget" => crate::projection::fold(
            entries,
            crate::projection::CanvasState::default(),
            crate::projection::Clock(deps_now_f64()),
        )
        .widgets
        .get(&parsed.id)
        .map(|widget| widget.version as u64)
        .unwrap_or(0),
        // Every `canvas:*` target seeds the one registry id — CANVAS_TARGET_ID.
        "canvas" => crate::projection::fold(
            entries,
            crate::projection::CanvasState::default(),
            crate::projection::Clock(deps_now_f64()),
        )
        .version as u64,
        "note" => notes_document(data_dir)
            .get("items")
            .and_then(Value::as_array)
            .and_then(|items| {
                items.iter().find(|item| {
                    item.get("id").and_then(Value::as_str) == Some(parsed.id.as_str())
                })
            })
            .and_then(|item| item.get("version").and_then(Value::as_u64))
            .unwrap_or(0),
        _ => 0,
    }
}

static COMMAND_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `cmd-${now}-${counter}` — the id every submitted command mints whether it
/// needs one or not.
fn mint_command_id(now: i64) -> String {
    let counter = COMMAND_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    format!("cmd-{now}-{counter}")
}

/// The cached answer an idempotent replay returns. Successes synthesize the
/// commit entry plus the `data` the first run's handler returned — the
/// TypeScript cached the whole `CommandResult`, so a retry reads back what
/// the first caller saw. Failures carry the exact response the first
/// attempt produced, since `flow.submit` cached resolved rejections too.
fn idempotent_replay(
    actor: &str,
    entry_type: &str,
    target: &str,
    cached: &Value,
) -> Result<(crate::journal::JournalEntry, Value), Response> {
    if cached.get("ok").and_then(Value::as_bool) == Some(false) {
        let status = cached.get("status").and_then(Value::as_u64).unwrap_or(400) as u16;
        let body = cached.get("body").cloned().unwrap_or_else(|| {
            json!({ "error": "command failed", "code": "failed" })
        });
        return Err(Response::json(status, body));
    }
    Ok((
        crate::journal::JournalEntry {
            seq: cached.get("seq").and_then(Value::as_u64).unwrap_or(0),
            at: cached.get("at").and_then(Value::as_u64).unwrap_or(0),
            phase: "commit".into(),
            actor_id: actor.to_owned(),
            command_id: None,
            entry_type: entry_type.to_owned(),
            target: target.to_owned(),
            payload: None,
            version: cached.get("version").and_then(Value::as_i64),
            error: None,
            prev_hash: None,
            hash: None,
            workspace_dir: None,
        },
        cached.get("data").cloned().unwrap_or(Value::Null),
    ))
}

/// The result payload an idempotency record keeps — the CommandResult shape
/// `flow.submit`'s `track` stored (`{ok, seq, version, data, …}` on success,
/// `{ok:false, code, message, …}` on failure; `set` adds `cached`).
fn idempotent_record(
    entry_type: &str,
    command_id: &str,
    outcome: &Result<(crate::journal::JournalEntry, Value), Response>,
) -> Value {
    match outcome {
        Ok((entry, data)) => json!({
            "ok": true,
            "seq": entry.seq,
            "at": entry.at,
            "version": entry.version,
            "commandId": command_id,
            "data": data,
        }),
        Err(response) => json!({
            "ok": false,
            "commandId": command_id,
            "type": entry_type,
            "status": response.status,
            "body": response.body,
        }),
    }
}

/// `handler.extraLocks` — the TypeScript registered one: `file.rename`
/// declares `file:{to}` so the destination contends for locks the same as
/// the source. Non-local or empty `to` values declare nothing, exactly as
/// `isLocalPath(destination)` filtered them there.
fn extra_lock_targets(entry_type: &str, payload: &Value) -> Vec<String> {
    if entry_type != "file.rename" {
        return Vec::new();
    }
    let Some(to) = payload.get("to").and_then(Value::as_str).map(str::trim) else {
        return Vec::new();
    };
    if to.is_empty() || !crate::resources::is_local_path(to) {
        return Vec::new();
    }
    vec![crate::resources::file_resource(to)]
}

/// `apply`'s `finally` lock release, as a function so the flag can move
/// freely between borrow and assignment.
fn release_implicit(
    env: &mut dyn RouteEnv,
    target: &str,
    actor: &str,
    now: i64,
    took: bool,
) {
    if took {
        let _ = env.locks().release(target, actor, now);
    }
}

/// The front half of `flow.apply` — `assertUnlockedFor`,
/// `takeImplicitLockFor`, `assertVersionGate`, then the `intent` append —
/// run under one environment borrow. Returns `(current_version,
/// took_implicit_lock, implicit_extra_locks)`; on failure every acquired
/// lock is already released, as `apply`'s `finally` did.
#[allow(clippy::too_many_arguments)]
fn submit_gate(
    env: &mut dyn RouteEnv,
    entry_type: &str,
    target: &str,
    actor: &str,
    requires_lock: bool,
    ignore_version: bool,
    base_version: Option<f64>,
    payload: &Value,
    now: i64,
    store_version: Option<u64>,
) -> Result<(u64, bool, Vec<String>), Response> {
    let mut implicit_lock = false;
    let mut implicit_extras: Vec<String> = Vec::new();

    if requires_lock {
        // `handler.extraLocks` — the one definer in the TypeScript was
        // `file.rename`, locking the destination the same as the source so a
        // lock held on `file:{to}` can't be renamed over invisibly.
        let extras = extra_lock_targets(entry_type, payload);
        {
            let mut locks = env.locks();
            // `assertUnlockedFor` checked the target and the extras together
            // before `takeImplicitLockFor` acquired any of them — a conflict
            // anywhere refuses the command without a partial acquisition.
            for resource in std::iter::once(target).chain(extras.iter().map(String::as_str)) {
                if let Some(lock) = if locks.is_locked_by_other(resource, actor, now) {
                    locks.holder(resource, now)
                } else {
                    None
                } {
                    drop(locks);
                    return Err(Response::from_error(&CommandError::with_details(
                        ErrorCode::Locked,
                        format!("{resource} is locked by {}", lock.actor_id),
                        json!({
                            "lock": {
                                "resource": lock.resource,
                                "actorId": lock.actor_id,
                                "expiresAt": lock.expires_at,
                            }
                        }),
                    )));
                }
            }
            for resource in std::iter::once(target).chain(extras.iter().map(String::as_str)) {
                if locks.is_held_by(resource, actor, now) {
                    continue;
                }
                match locks.acquire(
                    crate::locks::AcquireInput {
                        resource,
                        actor_id: actor,
                        ttl_ms: None,
                        reason: Some(entry_type.to_owned()),
                        implicit: true,
                    },
                    now,
                ) {
                    Ok(_) => {
                        if resource == target {
                            implicit_lock = true;
                        } else {
                            implicit_extras.push(resource.to_owned());
                        }
                    }
                    Err(error) => {
                        drop(locks);
                        for held in &implicit_extras {
                            let _ = env.locks().release(held, actor, now);
                        }
                        release_implicit(env, target, actor, now, implicit_lock);
                        return Err(Response::from_error(&error));
                    }
                }
            }
        }
    }

    let current_version = store_version.unwrap_or_else(|| env.target_version(target));
    // `typeof body.baseVersion === 'number'` — every numeric spelling gates,
    // including ones that can never match a version.
    if !ignore_version && base_version.is_some_and(|base| base != current_version as f64) {
        for held in &implicit_extras {
            let _ = env.locks().release(held, actor, now);
        }
        release_implicit(env, target, actor, now, implicit_lock);
        return Err(Response::from_error(&CommandError::with_details(
            ErrorCode::Conflict,
            format!(
                "{target} moved on: expected version {}, found {current_version}",
                base_version.unwrap_or_default()
            ),
            json!({
                "target": target,
                "expected": base_version,
                "actual": current_version,
            }),
        )));
    }

    if let Err(message) = env.journal().and_then(|mut log| {
        log.append(&crate::journal_log::AppendSpec {
            phase: "intent",
            actor,
            entry_type,
            target,
            payload: Some(payload.clone()),
            version: None,
            command_id: None,
            error: None,
        })
    }) {
        for held in &implicit_extras {
            let _ = env.locks().release(held, actor, now);
        }
        release_implicit(env, target, actor, now, implicit_lock);
        return Err(Response::from_error(&CommandError::new(
            ErrorCode::Failed,
            message,
        )));
    }
    Ok((current_version, implicit_lock, implicit_extras))
}

/// A command past the gates: intent is in the journal, the implicit lock is
/// held, and the handler's work is all that stands between it and the commit
/// (or `abort`). Owned data only, so an async caller can carry it across an
/// `await` — the axum handlers run their handler inside `spawn_blocking`.
pub struct PendingSubmit {
    entry_type: String,
    target: String,
    actor: String,
    now: i64,
    command_id: String,
    idempotency_key: Option<String>,
    tracked_key: bool,
    implicit_lock: bool,
    implicit_extras: Vec<String>,
    /// The payload the commit will carry — work may edit it in place (the
    /// TypeScript's handlers mutated `cmd.payload`, e.g. `p.id = id`).
    pub payload: Value,
    version: i64,
}

/// The two ways `submit_begin` can come back: a cached replay to return
/// verbatim, or a command that cleared the gates and needs its handler run.
/// The replayed `Ok` pairs the synthesized commit entry with the `data` the
/// first run returned — the cached `CommandResult` carried it.
pub enum Submit {
    Replay(Result<(crate::journal::JournalEntry, Value), Response>),
    Run(PendingSubmit),
}

/// `controlServer.submit` → the front half of `flow.apply`: lock heartbeat,
/// idempotent replay/track, lock gate, `baseVersion` gate, intent append.
/// Split from `submit_finish` so a handler that must `.await` its effect
/// (the axum terminal routes) can run the work between the two halves
/// without holding a borrow of `deps`.
///
/// `actor`, `base_version`, and `idempotency_key` are explicit rather than
/// pulled from a `Request` — the engine's typed axum handlers call this
/// without dressing their request up as one first.
#[allow(clippy::too_many_arguments)]
pub fn submit_begin(
    deps: &mut RouteDeps<'_>,
    actor: &str,
    base_version: Option<f64>,
    idempotency_key: Option<String>,
    entry_type: &str,
    target: &str,
    payload: Value,
    what: &str,
) -> Result<Submit, Response> {
    let (requires_lock, ignore_version) = command_flags(entry_type);
    let now = deps.now;
    let command_id = mint_command_id(now);

    // `registerVersions('run'|'orctask'|'dispatch'|'gate')` read the
    // orchestration store — reachable on `deps` before the env borrow opens.
    let store_version = orchestration_target_version(deps.orchestration, target);

    let env = environment(deps, what, "a journaled command needs the running app")?;

    // `locks.heartbeat(agentId)` — every submit renews the caller's held
    // locks before anything else looks at them.
    env.locks().heartbeat(actor, None, now);

    // `idempotency.get` — a retry carrying a known key is answered from
    // the cache before admission control, exactly like flow.submit.
    let mut tracked_key = false;
    if let Some(key) = &idempotency_key {
        let mut cached = env.idempotency().get(key, now);
        // The TypeScript joined the in-flight promise; a synchronous
        // route can only poll for the pending command to settle.
        for _ in 0..80 {
            match cached {
                Some(crate::idempotency::Entry::Pending) => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                    cached = env.idempotency().get(key, now);
                }
                _ => break,
            }
        }
        if let Some(crate::idempotency::Entry::Done(cached)) = cached {
            return Ok(Submit::Replay(idempotent_replay(
                actor, entry_type, target, &cached,
            )));
        }
    }

    // `rateLimiter.tryConsume` — after the replay check, before the command
    // is tracked or gated, exactly where submit() spent budget.
    if !env.rate_limit(actor, now) {
        return Err(Response::from_error(&CommandError::new(
            ErrorCode::RateLimited,
            "429 Rate limit exceeded for actor",
        )));
    }

    if let Some(key) = &idempotency_key {
        // `idempotency.track` — mark the key in flight so a concurrent
        // retry sees Pending rather than starting a second run.
        env.idempotency().track(key, now);
        tracked_key = true;
    }

    let (current_version, implicit_lock, implicit_extras) = match submit_gate(
        env,
        entry_type,
        target,
        actor,
        requires_lock,
        ignore_version,
        base_version,
        &payload,
        now,
        store_version,
    ) {
        Ok(gate) => gate,
        Err(response) => {
            if tracked_key {
                if let Some(key) = &idempotency_key {
                    env.idempotency().set(
                        key,
                        idempotent_record(entry_type, &command_id, &Err(response.clone())),
                        now,
                    );
                }
            }
            return Err(response);
        }
    };

    Ok(Submit::Run(PendingSubmit {
        entry_type: entry_type.to_owned(),
        target: target.to_owned(),
        actor: actor.to_owned(),
        now,
        command_id,
        idempotency_key,
        tracked_key,
        implicit_lock,
        implicit_extras,
        version: resolved_commit_version(entry_type, current_version),
        payload,
    }))
}

/// The back half of `flow.apply`: commit on success, `abort` on a post-intent
/// failure, implicit-lock release and the idempotent record either way —
/// apply()'s `finally` in the TypeScript. `result` is the handler's answer —
/// its `data` — so the commit, the caller's response, and the idempotent
/// record all carry the same thing.
pub fn submit_finish(
    deps: &mut RouteDeps<'_>,
    pending: PendingSubmit,
    result: Result<Value, Response>,
    what: &str,
) -> Result<(crate::journal::JournalEntry, Value), Response> {
    let outcome = result.and_then(|data| {
        environment(deps, what, "a journaled command needs the running app").and_then(|env| {
            env.journal()
                .and_then(|mut log| {
                    log.append(&crate::journal_log::AppendSpec {
                        phase: "commit",
                        actor: &pending.actor,
                        entry_type: &pending.entry_type,
                        target: &pending.target,
                        payload: Some(pending.payload.clone()),
                        version: Some(pending.version),
                        command_id: Some(pending.command_id.as_str()),
                        error: None,
                    })
                })
                .map(|entry| (entry, data))
                .map_err(|message| {
                    Response::from_error(&CommandError::new(ErrorCode::Failed, message))
                })
        })
    });

    // The `finally` half of apply(): a post-intent failure appends `abort`,
    // the implicit lock releases regardless, and the resolved result —
    // rejection included — is what an idempotent retry replays.
    let env = match environment(deps, what, "a journaled command needs the running app") {
        Ok(env) => env,
        Err(_) => {
            return outcome;
        }
    };
    if let Err(response) = &outcome {
        let _ = env.journal().and_then(|mut log| {
            log.append(&crate::journal_log::AppendSpec {
                phase: "abort",
                actor: &pending.actor,
                entry_type: &pending.entry_type,
                target: &pending.target,
                payload: None,
                version: None,
                command_id: None,
                error: Some(match response.body.get("error") {
                    Some(Value::String(message)) => message.clone(),
                    _ => "command failed".to_owned(),
                }),
            })
        });
    }
    for held in &pending.implicit_extras {
        let _ = env.locks().release(held, &pending.actor, pending.now);
    }
    release_implicit(
        env,
        &pending.target,
        &pending.actor,
        pending.now,
        pending.implicit_lock,
    );
    if pending.tracked_key {
        if let Some(key) = &pending.idempotency_key {
            env.idempotency().set(
                key,
                idempotent_record(&pending.entry_type, &pending.command_id, &outcome),
                pending.now,
            );
        }
    }
    outcome
}

/// `controlServer.submit` → `flow.apply`, minus the pieces this port does not
/// carry (the in-process queue, the speculative overlay). The gate order is
/// the contract the TypeScript fixed: the caller's locks heartbeat first, an
/// idempotent replay is answered before any gate, a held lock beats a stale
/// `baseVersion`, the intent lands before the work, and a post-intent failure
/// leaves an `abort` behind.
///
/// `work` is the handler half — for journal-fold mutations it just returns
/// the `data` (the commit entry *is* the write); for effects like a git
/// invocation it runs between intent and commit exactly as the original's
/// handler did. Its second argument is the payload the commit will carry:
/// the TypeScript journaled `cmd.payload` after `apply()` had run, so a
/// handler that mints an id (`p.id = id` in `widget.create`) writes it into
/// the commit even though the intent above recorded the original. The
/// returned `Value` is both the response's `data` and what an idempotent
/// replay reads back.
fn submit_command(
    deps: &mut RouteDeps<'_>,
    request: &Request,
    entry_type: &str,
    target: &str,
    payload: Value,
    what: &str,
    work: impl FnOnce(&mut RouteDeps<'_>, &mut Value) -> Result<Value, Response>,
) -> Result<(crate::journal::JournalEntry, Value), Response> {
    let base_version = request.body.get("baseVersion").and_then(Value::as_f64);
    let idempotency_key = request
        .body
        .get("idempotencyKey")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let actor = actor(request).to_owned();
    match submit_begin(
        deps,
        &actor,
        base_version,
        idempotency_key,
        entry_type,
        target,
        payload,
        what,
    )? {
        Submit::Replay(result) => result,
        Submit::Run(mut pending) => {
            let result = work(deps, &mut pending.payload);
            submit_finish(deps, pending, result, what)
        }
    }
}

/// One journaled route arm: submit through the gates, run `work`, answer
/// `accepted` — the `reply(await submit(…), status)` shape, collapsed.
#[allow(clippy::too_many_arguments)]
fn submitted(
    deps: &mut RouteDeps<'_>,
    request: &Request,
    entry_type: &str,
    target: &str,
    payload: Value,
    what: &str,
    status: u16,
    work: impl FnOnce(&mut RouteDeps<'_>, &mut Value) -> Result<Value, Response>,
) -> Response {
    match submit_command(deps, request, entry_type, target, payload, what, work) {
        Ok((entry, data)) => accepted(&entry, status, data),
        Err(response) => response,
    }
}

/// Routes one request. `None` means no route matched — the caller answers 404,
/// as the TypeScript does after falling through every branch.
///
/// Every mutation is attributed (`registerHttpAgent` on each POST/PATCH/DELETE
/// in controlServer.ts) and every successful mutation answers in the
/// `{ok, version, seq, data}` envelope `reply()` produces. Flat fields stay
/// alongside `data` so `slate --json` callers keep reading what they read.
pub fn route(request: &Request, deps: &mut RouteDeps<'_>) -> Option<Response> {
    let method = request.method.as_str();
    if method != "GET" && !request.segments().is_empty() {
        if let Err(response) = register_http_agent(request) {
            return Some(response);
        }
    }
    let response = route_domains(request, deps)?;
    Some(enveloped(method, response))
}

/// The `reply()` envelope for a flat mutation answer. Responses that already
/// carry `ok` (the journal-backed ones) pass through untouched.
fn enveloped(method: &str, response: Response) -> Response {
    let Value::Object(flat) = &response.body else {
        return response;
    };
    if method == "GET" || response.status >= 400 || flat.contains_key("ok") {
        return response;
    }
    let mut body = Map::new();
    body.insert("ok".into(), Value::Bool(true));
    body.insert("version".into(), json!(0));
    body.insert("seq".into(), json!(0));
    body.insert("data".into(), response.body.clone());
    for (key, value) in flat {
        body.insert(key.clone(), value.clone());
    }
    Response::json(response.status, Value::Object(body))
}

fn route_domains(request: &Request, deps: &mut RouteDeps<'_>) -> Option<Response> {
    let segments = request.segments();
    let method = request.method.as_str();
    let head = segments.first().map(String::as_str);

    match head {
        // The shell's /health advertises the live surface: `terminals` is
        // the count `terminal list` would return and `commands` is every
        // journaled command type this build accepts — a CLI learns the
        // whole verb surface from one unauthenticated-adjacent read.
        None | Some("health") if method == "GET" => {
            let terminals = deps
                .env
                .as_deref_mut()
                .map(|env| env.terminals().len())
                .unwrap_or(0);
            Some(Response::json(
                200,
                json!({
                    "ok": true,
                    "app": "slate",
                    "server": "slate-control",
                    "version": deps.app_version,
                    "workspaceDir": deps.workspace_dir,
                    "terminals": terminals,
                    "commands": COMMAND_TYPES,
                }),
            ))
        }
        Some("orchestration") => orchestration_route(request, &segments, deps),
        Some("snapshot") if method == "GET" => Some(snapshot_route(request, deps)),
        Some("journal") if method == "GET" => Some(journal_route(request, deps)),
        Some("locks") => Some(locks_route(request, &segments, deps)),
        Some("git") => Some(git_route(request, &segments, deps)),
        Some("planner") => Some(planner_route(request, &segments, deps)),
        Some("note" | "notes") => Some(notes_route(request, &segments, deps)),
        Some("links") => Some(links_route(request, &segments, deps)),
        Some("timers") => Some(timers_route(request, &segments, deps)),
        Some("widgets") => Some(widgets_route(request, &segments, deps)),
        Some("browser") => Some(browser_route(request, &segments, deps)),
        Some("media") if method == "GET" => Some(media_route(request, &segments, deps)),
        Some("workspace") => match (method, segments.get(1).map(String::as_str)) {
            // Code workspaces were a Code-mode feature; the canvas app has
            // none, but `context` still asks — answer honestly.
            ("GET", Some("code")) => Some(Response::json(
                200,
                json!({
                    "workspaces": [],
                    "activeId": null,
                    "folder": deps.workspace_dir,
                }),
            )),
            _ => None,
        },
        Some("canvas") => Some(canvas_route(request, &segments, deps)),
        Some("screenshot") if method == "POST" => Some(needs_engine_handler(
            "POST /screenshot",
            "capturing pixels needs the running renderer",
        )),
        _ => None,
    }
}

/// `imageList` + `importImages` — the body's `images[]` merged with a lone
/// `image`, trimmed, emptied entries dropped, capped at 8, then every path
/// copied into the media store. The TypeScript ran this before `submit`, so
/// a bad path answers 400 without an intent entry ever landing.
fn import_body_images(
    deps: &mut RouteDeps<'_>,
    request: &Request,
    what: &str,
) -> Result<Vec<String>, Response> {
    let paths: Vec<String> = request
        .strings("images")
        .into_iter()
        .chain(request.text("image").map(str::to_owned))
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .take(8)
        .collect();
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let media_dir = environment(deps, what, "attachments import into the media store")?
        .data_dir()
        .join("media");
    paths
        .iter()
        .map(|p| {
            crate::attachments::import_image(std::path::Path::new(p), &media_dir)
                .map(|path| path.to_string_lossy().into_owned())
                .map_err(invalid)
        })
        .collect()
}

fn orchestration_route(
    request: &Request,
    segments: &[String],
    deps: &mut RouteDeps<'_>,
) -> Option<Response> {
    let method = request.method.as_str();
    let run_id_param = request.param("runId");
    let second = segments.get(1).map(String::as_str);
    let third = segments.get(2).map(String::as_str);
    let fourth = segments.get(3).map(String::as_str);
    let store = &mut *deps.orchestration;

    match (method, second, third, fourth) {
        // --- reads ----------------------------------------------------------
        ("GET", None, _, _) => Some(Response::json(200, snapshot(store, run_id_param))),

        ("GET", Some("runs"), None, _) => Some(Response::json(
            200,
            json!({
                "runs": store.list_runs().iter().map(run_json).collect::<Vec<_>>(),
                "active": store.active_run().map(run_json),
            }),
        )),
        ("GET", Some("runs"), Some(id), _) => Some(match store.require_run(id) {
            Ok(run) => Response::json(200, json!({ "run": run_json(run) })),
            Err(error) => not_found(error.message),
        }),

        ("GET", Some("tasks"), None, _) => {
            if let Some(status) = request.param("status") {
                if !TASK_STATUSES.contains(&status) {
                    return Some(invalid(format!("unknown task status \"{status}\"")));
                }
            }
            let tasks =
                store.list_tasks(run_id_param, request.param("status"), request.flag("ready"));
            Some(Response::json(
                200,
                json!({ "tasks": tasks.iter().map(task_json).collect::<Vec<_>>() }),
            ))
        }
        ("GET", Some("tasks"), Some(id), _) => Some(match store.require_task(id) {
            Ok(task) => Response::json(200, json!({ "task": task_json(task) })),
            Err(error) => not_found(error.message),
        }),

        ("GET", Some("dispatches"), None, _) => {
            let dispatches = store.list_dispatches(run_id_param, request.param("taskId"), None);
            let unaccounted: Vec<String> = store
                .unaccounted_dispatches(run_id_param)
                .iter()
                .map(|d| d.id.clone())
                .collect();
            Some(Response::json(
                200,
                json!({
                    "dispatches": dispatches.iter().map(dispatch_json).collect::<Vec<_>>(),
                    "unaccounted": unaccounted,
                }),
            ))
        }
        ("GET", Some("dispatches"), Some(id), _) => Some(match store.require_dispatch(id) {
            Ok(dispatch) => Response::json(200, json!({ "dispatch": dispatch_json(dispatch) })),
            Err(error) => not_found(error.message),
        }),

        ("GET", Some("gates"), None, _) => Some(Response::json(
            200,
            json!({
                "gates": store
                    .list_gates(run_id_param, request.flag("open"))
                    .iter()
                    .map(gate_json)
                    .collect::<Vec<_>>(),
            }),
        )),
        ("GET", Some("gates"), Some(id), _) => Some(match store.require_gate(id) {
            Ok(gate) => Response::json(200, json!({ "gate": gate_json(gate) })),
            Err(error) => not_found(error.message),
        }),

        ("GET", Some("messages"), Some(id), _) => Some(match store.message_by_id(id) {
            Some(message) => Response::json(200, json!({ "message": message_json(message) })),
            None => not_found(format!("no message \"{id}\"")),
        }),

        ("GET", Some("inbox"), _, _) => {
            let Some(agent_id) = request.agent_id.as_deref() else {
                return Some(Response::from_error(&CommandError::new(
                    ErrorCode::UnknownActor,
                    "agentId is required",
                )));
            };
            let types: Vec<String> = request
                .param("types")
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            for message_type in &types {
                if !MESSAGE_TYPES.contains(&message_type.as_str()) {
                    return Some(invalid(format!("unknown message type \"{message_type}\"")));
                }
            }
            // `wait`/`timeoutMs` long-polling belongs to the transport, not the
            // router — the caller still gets the truthful immediate answer.
            // `runId`, `all`, and `limit` (clamped 1..=200, default 50) match
            // the shell's inbox query.
            let run_id = request.param("runId").map(str::to_owned);
            let all = request.flag("all");
            let limit = request
                .param("limit")
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(50)
                .clamp(1, 200);
            let messages = store.inbox(
                agent_id,
                if types.is_empty() { None } else { Some(&types) },
                run_id.as_deref(),
                all,
                limit,
            );
            Some(Response::json(
                200,
                json!({
                    "messages": messages.iter().map(message_json).collect::<Vec<_>>(),
                    "waited": false,
                }),
            ))
        }

        ("GET", Some("replies"), Some(ask_id), _) => {
            if store.message_by_id(ask_id).is_none() {
                return Some(not_found(format!("no message \"{ask_id}\"")));
            }
            Some(Response::json(
                200,
                json!({
                    "reply": store.reply_to(ask_id).map(message_json),
                    "waited": false,
                }),
            ))
        }

        // --- writes ---------------------------------------------------------
        ("POST", Some("runs"), None, _) => {
            // `submit(body, 'run.create', 'run:new', body)` — the store write
            // is the handler's apply, so it runs between intent and commit.
            let coordinator = request.agent_id.as_deref().unwrap_or("api").to_owned();
            Some(submitted(
                deps,
                request,
                "run.create",
                "run:new",
                request.body.clone(),
                "POST /orchestration/runs",
                201,
                |deps, payload| {
                    deps.orchestration
                        .create_run(
                            payload.get("objective").and_then(Value::as_str).unwrap_or(""),
                            &coordinator,
                            deps.now,
                        )
                        .map(|run| run_json(&run))
                        .map_err(|error| Response::from_error(&error))
                },
            ))
        }
        ("POST", Some("runs"), Some(id), Some("close")) => Some(submitted(
            deps,
            request,
            "run.close",
            &format!("run:{id}"),
            json!({}),
            "POST /orchestration/runs/{id}/close",
            200,
            |deps, _| {
                deps.orchestration
                    .close_run(id, deps.now)
                    .map(|run| run_json(&run))
                    .map_err(|error| Response::from_error(&error))
            },
        )),

        ("POST", Some("tasks"), None, _) => {
            // `submit(body, 'orctask.create', 'orctask:new', {...body, images})`
            // — images were media-imported before the submit, so a bad path
            // 400s with no intent entry.
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            let images = match import_body_images(deps, request, "POST /orchestration/tasks") {
                Ok(images) => images,
                Err(response) => return Some(response),
            };
            payload["images"] = json!(images);
            let created_by = request.agent_id.as_deref().unwrap_or("api").to_owned();
            Some(submitted(
                deps,
                request,
                "orctask.create",
                "orctask:new",
                payload,
                "POST /orchestration/tasks",
                201,
                |deps, payload| {
                    let run_id = deps
                        .orchestration
                        .resolve_run_id(payload.get("runId").and_then(Value::as_str))
                        .map_err(|error| Response::from_error(&error))?;
                    let deps_list: Vec<String> = payload
                        .get("deps")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
                        })
                        .unwrap_or_default();
                    let images_list: Vec<String> = payload
                        .get("images")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
                        })
                        .unwrap_or_default();
                    deps.orchestration
                        .create_task(
                            &run_id,
                            payload.get("title").and_then(Value::as_str),
                            payload.get("spec").and_then(Value::as_str).unwrap_or(""),
                            &deps_list,
                            &images_list,
                            &created_by,
                            deps.now,
                        )
                        .map(|task| task_json(&task))
                        .map_err(|error| Response::from_error(&error))
                },
            ))
        }
        ("PATCH", Some("tasks"), Some(id), _) => Some(submitted(
            deps,
            request,
            "orctask.update",
            &format!("orctask:{id}"),
            request.body.clone(),
            "PATCH /orchestration/tasks/{id}",
            200,
            |deps, payload| {
                let title = payload.get("title").and_then(Value::as_str).map(str::to_owned);
                let spec = payload.get("spec").and_then(Value::as_str).map(str::to_owned);
                let status = payload.get("status").and_then(Value::as_str).map(str::to_owned);
                deps.orchestration
                    .update_task(id, status.as_deref(), title.as_deref(), spec.as_deref(), deps.now)
                    .map(|task| task_json(&task))
                    .map_err(|error| Response::from_error(&error))
            },
        )),

        ("POST", Some("dispatches"), None, _) => {
            // Faithful port of the shell's `dispatch.start` command: resolve or
            // open a terminal, record the dispatch, launch the agent command,
            // sniff for an immediate launch failure, inject the preamble, and
            // mail the coordinator a dispatch notice. All of it is the apply —
            // a failed spawn or injection now leaves intent+abort rather than
            // a bare 400 with no record.
            let agent_id = request.agent_id.as_deref().unwrap_or("api").to_owned();
            Some(submitted(
                deps,
                request,
                "dispatch.start",
                "dispatch:new",
                request.body.clone(),
                "POST /orchestration/dispatches",
                201,
                |deps, _| {
                    let task_id = request.text("taskId").unwrap_or("").to_owned();
                    if task_id.is_empty() {
                        return Err(invalid("taskId is required"));
                    }
                    let agent = request.text("agent").unwrap_or("").to_owned();
                    let agent = if agent.is_empty() {
                        "claude".to_owned()
                    } else {
                        agent
                    };
                    if agent.len() > 64
                        || !agent
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
                    {
                        return Err(invalid("agent must be 1-64 chars of [A-Za-z0-9_.-]"));
                    }
                    let task = match deps.orchestration.require_task(&task_id) {
                        Ok(task) => task.clone(),
                        Err(error) => return Err(Response::from_error(&error)),
                    };
                    let run = match deps.orchestration.require_run(&task.run_id) {
                        Ok(run) => run.clone(),
                        Err(error) => return Err(Response::from_error(&error)),
                    };

                    let mut opened = false;
                    let terminal_id = match request
                        .text("terminalId")
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                    {
                        Some(target) => {
                            let id = match environment(
                                deps,
                                "POST /orchestration/dispatches",
                                "dispatches open terminals",
                            ) {
                                Ok(env) => {
                                    match env.resolve_terminal(target, request.agent_id.as_deref()) {
                                        Ok(id) => id,
                                        Err(error) => return Err(Response::from_error(&error)),
                                    }
                                }
                                Err(response) => return Err(response),
                            };
                            if let Some(busy) = deps.orchestration.dispatch_for_terminal(&id) {
                                return Err(Response::from_error(&CommandError::new(
                                    ErrorCode::Conflict,
                                    format!(
                                        "terminal \"{id}\" is already running dispatch {}",
                                        busy.id
                                    ),
                                )));
                            }
                            id
                        }
                        None => {
                            // `terminals.reserve` — a fresh shell the canvas adopts
                            // into a widget on its next tick.
                            let title = format!("{agent}: {}", task.title);
                            let cwd = deps.workspace_dir.unwrap_or_default().to_owned();
                            match environment(
                                deps,
                                "POST /orchestration/dispatches",
                                "dispatches open terminals",
                            ) {
                                Ok(env) => {
                                    let id = env.next_terminal_id("agent");
                                    match env.spawn_terminal(&id, &title, &cwd) {
                                        Ok(()) => {
                                            opened = true;
                                            id
                                        }
                                        Err(error) => {
                                            return Err(Response::from_error(&CommandError::new(
                                                ErrorCode::Failed,
                                                error,
                                            )))
                                        }
                                    }
                                }
                                Err(response) => return Err(response),
                            }
                        }
                    };

                    macro_rules! cleanup {
                        () => {
                            if opened {
                                if let Ok(env) = environment(
                                    deps,
                                    "POST /orchestration/dispatches",
                                    "dispatches open terminals",
                                ) {
                                    let _ = env.dispose_terminal(&terminal_id);
                                }
                            }
                        };
                    }

                    // A fresh shell needs a beat to reach its read loop — the shell's
                    // `waitUntilRunning` window, shortened by the write ack.
                    if opened {
                        std::thread::sleep(std::time::Duration::from_millis(400));
                    }

                    let dispatch = match deps.orchestration.create_dispatch(
                        &task_id,
                        &terminal_id,
                        &agent,
                        "",
                        deps.now,
                    ) {
                        Ok(dispatch) => dispatch,
                        Err(error) => {
                            cleanup!();
                            return Err(Response::from_error(&error));
                        }
                    };
                    let preamble = build_preamble(&run, &task, &dispatch.id, &agent);
                    let _ = deps
                        .orchestration
                        .set_dispatch_preamble(&dispatch.id, &preamble);

                    let mut injected = false;
                    let inject = request
                        .field("inject")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if inject {
                        if opened {
                            // Launch the agent CLI in the fresh shell, give it a
                            // moment, and sniff the tail for a launch failure — the
                            // shell's `submitShellLine` + `terminalShowsLaunchFailure`.
                            let start = request.text("command").unwrap_or(&agent).to_owned();
                            let started = match environment(
                                deps,
                                "POST /orchestration/dispatches",
                                "dispatches open terminals",
                            ) {
                                Ok(env) => env.write_terminal(&terminal_id, &start, true).is_ok(),
                                Err(response) => {
                                    // The shell settles a doomed dispatch `failed`
                                    // before cleaning up its fresh terminal.
                                    let _ = deps.orchestration.settle_dispatch(
                                        &dispatch.id,
                                        "failed",
                                        None,
                                        deps.now,
                                    );
                                    cleanup!();
                                    return Err(response);
                                }
                            };
                            if !started {
                                let _ = deps.orchestration.settle_dispatch(
                                    &dispatch.id,
                                    "failed",
                                    None,
                                    deps.now,
                                );
                                cleanup!();
                                return Err(Response::from_error(&CommandError::new(
                                    ErrorCode::Failed,
                                    format!("could not start {start} in terminal {terminal_id}"),
                                )));
                            }
                            std::thread::sleep(std::time::Duration::from_millis(2500));
                            let tail = match environment(
                                deps,
                                "POST /orchestration/dispatches",
                                "dispatches open terminals",
                            ) {
                                Ok(env) => {
                                    env.tail_terminal(&terminal_id, 16_000).unwrap_or_default()
                                }
                                Err(_) => String::new(),
                            };
                            if terminal_shows_launch_failure(&tail, &start) {
                                let _ = deps.orchestration.settle_dispatch(
                                    &dispatch.id,
                                    "failed",
                                    None,
                                    deps.now,
                                );
                                cleanup!();
                                return Err(Response::from_error(&CommandError::new(
                                    ErrorCode::Failed,
                                    format!("{start} is not available in terminal {terminal_id}"),
                                )));
                            }
                        }
                        // One-line inject, chunked like the shell's submitPtyMessage;
                        // Enter lands only on the last chunk.
                        let single = preamble.replace(['\r', '\n'], " ");
                        let mut ok = true;
                        for (i, chunk) in single
                            .as_bytes()
                            .chunks(8000)
                            .map(String::from_utf8_lossy)
                            .enumerate()
                        {
                            let last = (i + 1) * 8000 >= single.len();
                            let sent = match environment(
                                deps,
                                "POST /orchestration/dispatches",
                                "dispatches open terminals",
                            ) {
                                Ok(env) => env.write_terminal(&terminal_id, &chunk, last).is_ok(),
                                Err(_) => false,
                            };
                            if !sent {
                                ok = false;
                                break;
                            }
                        }
                        injected = ok;
                    }

                    let _ = deps.orchestration.send(
                        &run.id,
                        "dispatch",
                        &agent_id,
                        &terminal_id,
                        &format!("dispatch {}", task.id),
                        &format!("{} → {} ({})\n\n{}", task.id, terminal_id, agent, task.spec),
                        Some(&task.id),
                        Some(&dispatch.id),
                        None,
                        None,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                        deps.now,
                    );

                    Ok(json!({
                        "dispatch": dispatch_json(&dispatch),
                        "dispatchId": dispatch.id,
                        "taskId": task.id,
                        "terminalId": terminal_id,
                        "agent": agent,
                        "injected": injected,
                    }))
                },
            ))
        }
        ("POST", Some("dispatches"), Some(id), Some("settle")) => {
            let outcome = request.text("outcome").unwrap_or("").to_owned();
            let files = request
                .field("filesModified")
                .map(|_| request.strings("filesModified"));
            Some(match submit_command(
                deps,
                request,
                "dispatch.settle",
                &format!("dispatch:{id}"),
                request.body.clone(),
                "POST /orchestration/dispatches/{id}/settle",
                |deps, _| {
                    if !OUTCOMES.contains(&outcome.as_str()) {
                        return Err(invalid(format!(
                            "outcome must be one of {}",
                            OUTCOMES.join(", ")
                        )));
                    }
                    deps.orchestration
                        .settle_dispatch(id, &outcome, files.clone(), deps.now)
                        .map(|(dispatch, task, promoted)| {
                            json!({
                                "dispatchId": dispatch.id,
                                "taskId": task.id,
                                "status": task.status,
                                "promoted": promoted,
                            })
                        })
                        .map_err(|error| Response::from_error(&error))
                },
            ) {
                Ok((entry, data)) => {
                    // The TypeScript mailed the coordinator its worker_done
                    // *after* the submit answered ok — post-commit, not
                    // inside apply.
                    let actor = request.agent_id.as_deref().unwrap_or("api").to_owned();
                    let to = request.text("to").unwrap_or("@coordinator").to_owned();
                    let now = deps.now;
                    if let Some(dispatch_id) = data.get("dispatchId").and_then(Value::as_str) {
                        if let Ok(dispatch) = deps.orchestration.require_dispatch(dispatch_id) {
                            let (run_id, task_id, dispatch_id) = (
                                dispatch.run_id.clone(),
                                dispatch.task_id.clone(),
                                dispatch.id.clone(),
                            );
                            let _ = deps.orchestration.send(
                                &run_id,
                                "worker_done",
                                &actor,
                                &to,
                                &format!("worker_done {task_id}"),
                                &format!("dispatch {dispatch_id} settled as {outcome} via API"),
                                Some(&task_id),
                                Some(&dispatch_id),
                                None,
                                Some(&outcome),
                                files.clone().unwrap_or_default(),
                                Vec::new(),
                                Vec::new(),
                                now,
                            );
                        }
                    }
                    accepted(&entry, 200, data)
                }
                Err(response) => response,
            })
        }
        ("POST", Some("dispatches"), Some(id), Some("account")) => {
            // The original's apply validated `state` and — for a released
            // dispatch with closeTerminal — disposed the shell and removed
            // its widget, all inside apply, so a bad state leaves intent+abort.
            Some(submitted(
                deps,
                request,
                "dispatch.account",
                &format!("dispatch:{id}"),
                request.body.clone(),
                "POST /orchestration/dispatches/{id}/account",
                200,
                |deps, _| {
                    let Some(state) = request.text("state") else {
                        return Err(invalid("state must be retained|released"));
                    };
                    if state != "retained" && state != "released" {
                        return Err(invalid("state must be retained|released"));
                    }
                    let dispatch = deps
                        .orchestration
                        .set_dispatch_state(id, state)
                        .map_err(|error| Response::from_error(&error))?;
                    let close_terminal = request
                        .field("closeTerminal")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if state == "released" && close_terminal {
                        if let Ok(env) = environment(
                            deps,
                            "POST /orchestration/dispatches/:id/account",
                            "dispatch accounting closes terminals",
                        ) {
                            let _ = env.dispose_terminal(&dispatch.terminal_id);
                        }
                        // `requestWidgetRemoval` — the renderer answered with
                        // its own gated `widget.remove`. Different resource
                        // than this command's `dispatch:` target, so a nested
                        // submit acquires cleanly.
                        let known = environment(
                            deps,
                            "POST /orchestration/dispatches/:id/account",
                            "reading the canvas refolds the journal",
                        )
                        .ok()
                        .and_then(|env| folded_canvas(env).ok())
                        .is_some_and(|canvas| {
                            canvas.widgets.contains_key(&dispatch.terminal_id)
                        });
                        if known {
                            let terminal_id = dispatch.terminal_id.clone();
                            let _ = submit_command(
                                deps,
                                request,
                                "widget.remove",
                                &format!("widget:{terminal_id}"),
                                json!({}),
                                "POST /orchestration/dispatches/:id/account",
                                move |_, _| Ok(json!({ "id": terminal_id })),
                            );
                        }
                    }
                    Ok(json!({ "id": dispatch.id, "state": dispatch.state }))
                },
            ))
        }

        ("POST", Some("messages"), None, _) => {
            // `submit(body, 'orc.send', runTarget(body.runId), {…body, images})`
            // — the media import ran before submit, everything else is the
            // apply, so a bad type or a wrong-run dispatch leaves intent+abort.
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            let images = match import_body_images(deps, request, "POST /orchestration/messages") {
                Ok(images) => images,
                Err(response) => return Some(response),
            };
            payload["images"] = json!(images);
            // `runTarget` — absent runId targets `run:new`; a present one
            // must spell an id the resource grammar accepts.
            let run_target = match request.text("runId").map(str::trim).filter(|s| !s.is_empty())
            {
                Some(id) => format!("run:{id}"),
                None => "run:new".to_owned(),
            };
            let agent_id = request.agent_id.as_deref().unwrap_or("api").to_owned();
            Some(submitted(
                deps,
                request,
                "orc.send",
                &run_target,
                payload,
                "POST /orchestration/messages",
                201,
                |deps, payload| {
                    // The payload carries the media-store paths the route
                    // imported — `request.images` holds the raw sources.
                    let images: Vec<String> = payload
                        .get("images")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()
                        })
                        .unwrap_or_default();
                    let message_type = request.text("type").unwrap_or("note");
                    if !MESSAGE_TYPES.contains(&message_type) {
                        return Err(invalid(format!(
                            "type must be one of {}",
                            MESSAGE_TYPES.join(", ")
                        )));
                    }
                    let dispatch_id = request
                        .text("dispatchId")
                        .map(str::trim)
                        .filter(|id| !id.is_empty());
                    let dispatch = match dispatch_id {
                        Some(id) => match deps.orchestration.require_dispatch(id) {
                            Ok(dispatch) => Some(dispatch.clone()),
                            Err(error) => return Err(Response::from_error(&error)),
                        },
                        None => None,
                    };
                    let run_id = match request.text("runId") {
                        Some(id) if !id.trim().is_empty() => {
                            match deps.orchestration.resolve_run_id(Some(id)) {
                                Ok(id) => id,
                                Err(error) => return Err(Response::from_error(&error)),
                            }
                        }
                        _ => match dispatch.as_ref() {
                            Some(dispatch) => dispatch.run_id.clone(),
                            None => match deps.orchestration.resolve_run_id(None) {
                                Ok(id) => id,
                                Err(error) => return Err(Response::from_error(&error)),
                            },
                        },
                    };
                    if let Some(dispatch) = dispatch.as_ref() {
                        if dispatch.run_id != run_id {
                            return Err(invalid(format!(
                                "dispatch \"{}\" belongs to run \"{}\"",
                                dispatch.id, dispatch.run_id
                            )));
                        }
                        if request
                            .text("taskId")
                            .is_some_and(|task_id| task_id != dispatch.task_id.as_str())
                        {
                            return Err(invalid(format!(
                                "dispatch \"{}\" belongs to task \"{}\"",
                                dispatch.id, dispatch.task_id
                            )));
                        }
                    }
                    if let Some(reply_to) = request.text("replyTo") {
                        if deps.orchestration.message_by_id(reply_to).is_none() {
                            return Err(not_found(format!("no message \"{reply_to}\" to reply to")));
                        }
                    }
                    let settled = if message_type == "worker_done" {
                        let Some(dispatch) = dispatch.as_ref() else {
                            return Err(invalid("worker_done needs --dispatch-id"));
                        };
                        let outcome = request.text("outcome").unwrap_or("");
                        if !OUTCOMES.contains(&outcome) {
                            return Err(invalid(format!(
                                "worker_done needs --outcome {}",
                                OUTCOMES.join("|")
                            )));
                        }
                        let files = request
                            .field("filesModified")
                            .map(|_| request.strings("filesModified"));
                        match deps
                            .orchestration
                            .settle_dispatch(&dispatch.id, outcome, files, deps.now)
                        {
                            Ok((dispatch, task, promoted)) => Some(json!({
                                "dispatchId": dispatch.id,
                                "taskId": task.id,
                                "status": task.status,
                                "promoted": promoted,
                            })),
                            Err(error) => return Err(Response::from_error(&error)),
                        }
                    } else {
                        None
                    };
                    // The shell's `resolveRecipient`: empty → @coordinator, the
                    // handles pass through, `@<agent>` must name a running dispatch's
                    // agent, and a bare worker reference resolves to its terminal id.
                    // Without this `slate done` mail landed on `""` and never reached
                    // the coordinator it was addressed to.
                    let to = {
                        let raw = request.text("to").unwrap_or("").trim().to_owned();
                        let resolved = if raw.is_empty() {
                            Ok("@coordinator".to_owned())
                        } else if matches!(raw.as_str(), "*" | "@all" | "@idle" | "@coordinator")
                        {
                            Ok(raw)
                        } else if let Some(agent) = raw.strip_prefix('@') {
                            let agent = agent.to_owned();
                            let running = deps
                                .orchestration
                                .list_dispatches(Some(&run_id), None, Some("running"))
                                .into_iter()
                                .any(|d| d.agent.eq_ignore_ascii_case(&agent));
                            if running {
                                Ok(raw)
                            } else {
                                Err(CommandError::new(
                                    ErrorCode::NotFound,
                                    format!("no running dispatch for agent \"{agent}\""),
                                ))
                            }
                        } else {
                            let looked_up = match environment(
                                deps,
                                "/orchestration/messages",
                                "worker lookup",
                            ) {
                                Ok(env) => {
                                    env.resolve_terminal(&raw, request.agent_id.as_deref()).ok()
                                }
                                Err(response) => return Err(response),
                            };
                            // A recipient that is not a terminal, a name, or a
                            // prefix still passes through as an actor id — the
                            // store accepts opaque ids (api, cli, …).
                            Ok(looked_up.unwrap_or(raw))
                        };
                        match resolved {
                            Ok(to) => to,
                            Err(error) => return Err(Response::from_error(&error)),
                        }
                    };
                    let message = match deps.orchestration.send(
                        &run_id,
                        message_type,
                        &agent_id,
                        &to,
                        request.text("subject").unwrap_or(""),
                        request.text("body").unwrap_or(""),
                        request.text("taskId"),
                        dispatch_id,
                        request.text("replyTo"),
                        request.text("outcome"),
                        request.strings("filesModified"),
                        images,
                        request.strings("options"),
                        deps.now,
                    ) {
                        Ok(message) => message,
                        Err(error) => return Err(Response::from_error(&error)),
                    };
                    let mut data = message_json(&message);
                    if let (Some(settled), Value::Object(fields)) = (settled, &mut data) {
                        fields.insert("settled".to_owned(), settled);
                    }
                    Ok(data)
                },
            ))
        }
        ("POST", Some("messages"), Some(id), Some("ack")) => {
            // `orc.ack` — payload `{messageId}` on the run's target.
            let run_target = match request.text("runId").map(str::trim).filter(|s| !s.is_empty())
            {
                Some(run) => format!("run:{run}"),
                None => "run:new".to_owned(),
            };
            let actor = request.agent_id.as_deref().unwrap_or("api").to_owned();
            Some(submitted(
                deps,
                request,
                "orc.ack",
                &run_target,
                json!({ "messageId": id }),
                "POST /orchestration/messages/{id}/ack",
                200,
                |deps, _| {
                    deps.orchestration
                        .ack(id, &actor)
                        .map(|message| message_json(&message))
                        .map_err(|error| Response::from_error(&error))
                },
            ))
        }

        ("POST", Some("gates"), None, _) => {
            let created_by = request.agent_id.as_deref().unwrap_or("api").to_owned();
            Some(submitted(
                deps,
                request,
                "gate.create",
                "gate:new",
                request.body.clone(),
                "POST /orchestration/gates",
                201,
                |deps, _| {
                    let run_id = deps
                        .orchestration
                        .resolve_run_id(request.text("runId"))
                        .map_err(|error| Response::from_error(&error))?;
                    deps.orchestration
                        .create_gate(
                            &run_id,
                            request.text("question").unwrap_or(""),
                            &request.strings("options"),
                            &created_by,
                            request.text("taskId"),
                            deps.now,
                        )
                        .map(|gate| gate_json(&gate))
                        .map_err(|error| Response::from_error(&error))
                },
            ))
        }
        ("POST", Some("gates"), Some(id), Some("resolve")) => Some(submitted(
            deps,
            request,
            "gate.resolve",
            &format!("gate:{id}"),
            request.body.clone(),
            "POST /orchestration/gates/{id}/resolve",
            200,
            |deps, _| {
                deps.orchestration
                    .resolve_gate(id, request.text("resolution").unwrap_or(""), deps.now)
                    .map(|gate| gate_json(&gate))
                    .map_err(|error| Response::from_error(&error))
            },
        )),

        ("POST", Some("reset"), None, _) => {
            // Rebuilt through the store's own document so the private maps and
            // the id counter stay consistent — the store has no narrower
            // reset, and rewriting the JSON is what a fresh load sees.
            let tasks = request.field("tasks") == Some(&Value::Bool(true));
            let messages = request.field("messages") == Some(&Value::Bool(true));
            let all = request.field("all") == Some(&Value::Bool(true));
            if !tasks && !messages && !all {
                // Nothing asked for, nothing wiped — the TypeScript answers ok.
                return Some(Response::json(200, json!({ "ok": true })));
            }
            let mut doc = store.to_json();
            if let Some(doc) = doc.as_object_mut() {
                if all {
                    for key in ["runs", "tasks", "dispatches", "messages", "gates"] {
                        doc.insert(key.to_owned(), json!([]));
                    }
                } else {
                    if tasks {
                        // Dispatches and gates exist only to serve tasks, so a
                        // task wipe takes their dependents with it.
                        for key in ["tasks", "dispatches", "gates"] {
                            doc.insert(key.to_owned(), json!([]));
                        }
                    }
                    if messages {
                        doc.insert("messages".to_owned(), json!([]));
                    }
                }
            }
            *store = OrchestrationStore::load(&doc);
            Some(Response::accepted(
                200,
                1,
                0,
                json!({
                    "reset": true,
                    "tasks": tasks || all,
                    "messages": messages || all,
                    "all": all,
                }),
            ))
        }

        _ => None,
    }
}

fn snapshot(store: &OrchestrationStore, run_id: Option<&str>) -> Value {
    json!({
        "runs": store.list_runs().iter().map(run_json).collect::<Vec<_>>(),
        "tasks": store.list_tasks(run_id, None, false).iter().map(task_json).collect::<Vec<_>>(),
        "dispatches": store
            .list_dispatches(run_id, None, None)
            .iter()
            .map(dispatch_json)
            .collect::<Vec<_>>(),
        "messages": store.list_messages(run_id).iter().map(message_json).collect::<Vec<_>>(),
        "gates": store.list_gates(run_id, false).iter().map(gate_json).collect::<Vec<_>>(),
    })
}

// The record shapes the API returns are the persisted shapes, so they are
// borrowed from the store's own serializer rather than spelled out twice.
use crate::orchestration::{Dispatch, Gate, Message, OrchestrationTask, Run};

fn run_json(run: &Run) -> Value {
    crate::orchestration::json_for_run(run)
}
fn task_json(task: &OrchestrationTask) -> Value {
    crate::orchestration::json_for_task(task)
}
/// The worker brief the shell's `buildPreamble` typed into a dispatch
/// terminal — same sections and rules, `slate` verbs instead of `orc`.
fn build_preamble(
    run: &crate::orchestration::Run,
    task: &crate::orchestration::OrchestrationTask,
    dispatch_id: &str,
    agent: &str,
) -> String {
    let mut lines = vec![
        "--- SLATE DISPATCH ---".to_owned(),
        format!("You are a worker in Slate run {}.", run.id),
        format!("Objective of the run: {}", run.objective),
        String::new(),
        format!("Your task: {}", task.id),
        format!("Your dispatch: {dispatch_id}"),
        format!("Worker route: {agent}"),
        String::new(),
        "SPEC:".to_owned(),
        task.spec.clone(),
        String::new(),
    ];
    if !task.images.is_empty() {
        lines.push("IMAGES attached to this task — open them before you start:".to_owned());
        for path in &task.images {
            lines.push(format!("  {path}"));
        }
        lines.push(String::new());
    }
    lines.extend([
        "WORK RULES — keep this dispatch bounded:".to_owned(),
        "  • Read the relevant local instructions and inspect the current state before editing.".to_owned(),
        "  • Stay inside the scope and ownership in SPEC; do not rewrite unrelated files or".to_owned(),
        "    create competing dispatches. Preserve edits made by other workers.".to_owned(),
        "  • Meet every acceptance criterion and run the listed verification before reporting.".to_owned(),
        "  • If the spec is missing acceptance or verification details, use the smallest safe".to_owned(),
        "    check and state the gap in your report; do not expand the task silently.".to_owned(),
        "  • Never put credentials, tokens, or private environment values in messages or files.".to_owned(),
        String::new(),
        "CONTRACT — you talk to the coordinator by running shell commands:".to_owned(),
        String::new(),
        "  • Stuck, or the spec is wrong? Ask and wait for the answer:".to_owned(),
        format!("      slate ask --question \"...\" --task-id {} --dispatch-id {dispatch_id} --json", task.id),
        "    It blocks until the coordinator replies, then prints the reply.".to_owned(),
        String::new(),
        "  • Need a human/coordinator decision before you can continue?".to_owned(),
        format!("      slate escalate --body \"...\" --task-id {} --dispatch-id {dispatch_id} --json", task.id),
        String::new(),
        "  • Need explicit safety permission? Ask immediately and wait — do not silently".to_owned(),
        "    continue or forget the request:".to_owned(),
        format!("      slate ask --type permission --question \"...\" --task-id {} --dispatch-id {dispatch_id} --json", task.id),
        "    The request stays visible until the human answers it.".to_owned(),
        String::new(),
        "  • Long job? Say you are alive every few minutes:".to_owned(),
        format!("      slate heartbeat --task-id {} --dispatch-id {dispatch_id} --json", task.id),
        String::new(),
        "  • When finished — EXACTLY ONCE, and always, even on failure:".to_owned(),
        format!("      slate done --outcome succeeded --task-id {} --dispatch-id {dispatch_id} \\", task.id),
        "          --body \"what you changed and anything the coordinator must know\" \\".to_owned(),
        "          --files \"path/one.ts,path/two.ts\" --json".to_owned(),
        "    Use --outcome failed if you could not do it. A coordinator is blocked".to_owned(),
        "    waiting on this message; if you never send it, the run stalls.".to_owned(),
        "    Include changed files and concrete verification evidence in the body.".to_owned(),
        String::new(),
        "You also have the rest of the app: slate canvas, slate plan,".to_owned(),
        "slate terminal, slate git. Run `slate --help` for the full surface.".to_owned(),
        "--- END DISPATCH ---".to_owned(),
        String::new(),
        "Begin now.".to_owned(),
    ]);
    lines.join("\n")
}

/// The shell's `terminalShowsLaunchFailure`: the command name appears in the
/// tail next to a recognizer for "not found" in the usual shells.
fn terminal_shows_launch_failure(output: &str, command: &str) -> bool {
    let executable = command
        .trim_start()
        .trim_start_matches(['"', '\''])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();
    if executable.is_empty() {
        return false;
    }
    let tail: String = output
        .chars()
        .rev()
        .take(16_000)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .to_lowercase();
    if !tail.contains(&executable) {
        return false;
    }
    [
        "commandnotfoundexception",
        "cmdnotfound",
        "command not found",
        "is not recognized as an internal or external command",
        "is not recognized as the name of a cmdlet",
        "не является внутренней или внешней командой",
    ]
    .iter()
    .any(|marker| tail.contains(marker))
}

fn dispatch_json(dispatch: &Dispatch) -> Value {
    crate::orchestration::json_for_dispatch(dispatch)
}
fn message_json(message: &Message) -> Value {
    crate::orchestration::json_for_message(message)
}
fn gate_json(gate: &Gate) -> Value {
    crate::orchestration::json_for_gate(gate)
}

// ---------------------------------------------------------------------------
// Engine-backed domains — the routes below can only be answered by the
// running app, because they read the journal, the lock table, the media store
// or the PTY list. Each borrows what it needs through `RouteEnv`, so the
// dispatch stays in this file and the capability surface stays one page long.

/// `git`, run in the workspace dir. `readGitStatus`/`commitAll` are plain
/// subprocess calls in the TypeScript too; only the error mapping differs.
fn git(deps: &RouteDeps<'_>, args: &[&str]) -> Result<String, CommandError> {
    let cwd = deps.workspace_dir.unwrap_or_default();
    if cwd.is_empty() {
        return Err(CommandError::new(
            ErrorCode::Invalid,
            "no project folder is open",
        ));
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| CommandError::new(ErrorCode::Failed, format!("run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(CommandError::new(
            ErrorCode::Failed,
            if stderr.is_empty() {
                format!("git {} failed ({})", args[0], output.status)
            } else {
                stderr
            },
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A status read that never throws — `readGitStatus` returns EMPTY on a
/// non-repo rather than failing, because a CLI asking "is this a repo?" wants
/// the answer, not the error.
fn git_status(deps: &RouteDeps<'_>) -> Value {
    let cwd = deps.workspace_dir.unwrap_or_default();
    let empty = |extra: Value| -> Value {
        let mut base = json!({
            "repo": false, "ahead": 0, "behind": 0,
            "modified": 0, "untracked": 0, "staged": 0, "conflicted": 0,
            "readAt": deps.now,
        });
        if let (Some(map), Value::Object(extra)) = (base.as_object_mut(), extra) {
            map.extend(extra);
        }
        base
    };
    if cwd.is_empty() {
        return empty(Value::Null);
    }
    let root = match git(deps, &["rev-parse", "--show-toplevel"]) {
        Ok(root) if !root.trim().is_empty() => root.trim().replace('\r', ""),
        Ok(_) => return empty(Value::Null),
        Err(error) => {
            let raw = error.message.clone();
            if raw.to_lowercase().contains("not a git repository") {
                return empty(Value::Null);
            }
            return empty(json!({ "error": raw.trim().chars().take(300).collect::<String>() }));
        }
    };
    let porcelain = git(
        deps,
        &[
            "status",
            "--porcelain=v1",
            "-b",
            "-z",
            "--untracked-files=normal",
        ],
    )
    .unwrap_or_default();
    let normalized = porcelain.replace("\r\0", "\0").replace("\r\n", "\n");
    let entries: Vec<String> = normalized
        .split('\0')
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_end_matches('\r').to_owned())
        .collect();
    let header = entries
        .first()
        .filter(|line| line.starts_with("##"))
        .cloned()
        .unwrap_or_default();
    // `parseHeader` — "## main...origin/main [ahead 1, behind 2]".
    let body = header.get(2..).unwrap_or("").trim().to_owned();
    let (branches, tracking) = body
        .split_once(" [")
        .map(|(a, b)| (a.to_owned(), b.to_owned()))
        .unwrap_or((body.clone(), String::new()));
    let (branch, upstream) = branches
        .split_once("...")
        .map(|(a, b)| (a.to_owned(), Some(b.to_owned())))
        .unwrap_or((branches.clone(), None));
    let branch = branch.replace("No commits yet on ", "");
    let ahead = tracking
        .split("ahead ")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let behind = tracking
        .split("behind ")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);
    let mut status = empty(json!({
        "repo": true,
        "root": root,
        "branch": branch,
        "upstream": upstream,
        "ahead": ahead,
        "behind": behind,
    }));
    // The porcelain rows, kept as a `files` list for the CLI's plain-text
    // view — the original's GitStatus had counts only, the slate CLI prints
    // lines, so this carries both.
    let mut files: Vec<String> = Vec::new();
    let mut modified = 0i64;
    let mut untracked = 0i64;
    let mut staged = 0i64;
    let mut conflicted = 0i64;
    let mut iter = entries
        .iter()
        .skip(if header.is_empty() { 0 } else { 1 })
        .peekable();
    while let Some(entry) = iter.next() {
        if entry.len() < 2 {
            continue;
        }
        files.push(entry.clone());
        let code: Vec<char> = entry.chars().take(2).collect();
        if code == ['?', '?'] {
            untracked += 1;
        } else if code.contains(&'U') || code == ['A', 'A'] || code == ['D', 'D'] {
            conflicted += 1;
        } else {
            if code[0] != ' ' && code[0] != '?' {
                staged += 1;
            }
            if code[1] != ' ' && code[1] != '?' {
                modified += 1;
            }
        }
        // Renames and copies carry a second NUL-separated path.
        if code[0] == 'R' || code[0] == 'C' {
            if let Some(extra) = iter.next() {
                if let Some(row) = files.last_mut() {
                    row.push_str(&format!(" -> {extra}"));
                }
            }
        }
    }
    status["modified"] = json!(modified);
    status["untracked"] = json!(untracked);
    status["staged"] = json!(staged);
    status["conflicted"] = json!(conflicted);
    status["files"] = json!(files);
    if let Ok(log) = git(deps, &["log", "-1", "--format=%H%x1f%s%x1f%ct"]) {
        let log = log.trim().replace('\r', "");
        let parts: Vec<&str> = log.split('\x1f').collect();
        if parts.len() >= 3 {
            if let Ok(at) = parts[2].trim().parse::<i64>() {
                status["lastCommit"] = json!({
                    "hash": parts[0].chars().take(8).collect::<String>(),
                    "subject": parts[1],
                    "at": at * 1000,
                });
            }
        }
    }
    status
}

/// `assertSafeRef` — a ref the CLI supplies goes into `git checkout` verbatim.
fn assert_safe_ref(reference: &str) -> Result<(), CommandError> {
    let value = reference.trim();
    if value.is_empty() {
        return Err(CommandError::new(
            ErrorCode::Invalid,
            "a branch or commit is required",
        ));
    }
    if value.len() > 256 {
        return Err(CommandError::new(ErrorCode::Invalid, "ref is too long"));
    }
    if value.starts_with('-') || value.chars().any(|c| matches!(c, '\0' | '\n' | '\r')) {
        return Err(CommandError::new(ErrorCode::Invalid, "invalid ref"));
    }
    Ok(())
}

/// `isDirtyStatus` — untracked files ride along; only tracked changes block.
fn git_dirty(status: &Value) -> bool {
    status["modified"].as_i64().unwrap_or(0)
        + status["staged"].as_i64().unwrap_or(0)
        + status["conflicted"].as_i64().unwrap_or(0)
        > 0
}

fn git_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let second = segments.get(1).map(String::as_str);
    match (method, second) {
        ("GET", Some("status")) => Response::json(200, git_status(deps)),
        ("POST", Some("commit")) => {
            // `commitAll` was the handler — the message check and the git
            // invocation sit in `work` between intent and commit on
            // `git:repo`, so a rejected commit leaves intent+abort and a held
            // `git:repo` lock refuses it outright.
            let (entry, committed) = match submit_command(
                deps,
                request,
                "git.commit",
                "git:repo",
                json!({ "message": request.text("message") }),
                "POST /git/commit",
                |deps, _| {
                    let message = request
                        .text("message")
                        .map(str::trim)
                        .unwrap_or_default()
                        .replace("\r\n", "\n")
                        .replace('\r', "\n");
                    if message.is_empty() {
                        return Err(invalid("a commit message is required"));
                    }
                    match git(deps, &["add", "-A"])
                        .and_then(|_| git(deps, &["commit", "-m", &message]))
                        .and_then(|_| git(deps, &["rev-parse", "HEAD"]))
                    {
                        Ok(hash) => Ok(json!({
                            "hash": hash.trim().chars().take(8).collect::<String>(),
                            "message": message,
                        })),
                        Err(error) => Err(Response::from_error(&error)),
                    }
                },
            ) {
                Ok(pair) => pair,
                Err(response) => return response,
            };
            accepted(&entry, 200, committed)
        }
        ("GET", Some("branches")) => {
            let current = git(deps, &["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap_or_default()
                .trim()
                .replace('\r', "")
                .to_owned();
            match git(
                deps,
                &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
            ) {
                Ok(heads) => {
                    let mut branches: Vec<Value> = heads
                        .lines()
                        .map(|line| line.trim().replace('\r', ""))
                        .filter(|name| !name.is_empty())
                        .map(|name| json!({ "name": name, "current": name == current }))
                        .collect();
                    branches.sort_by(|a, b| {
                        let current_b = b["current"].as_bool().unwrap_or(false);
                        let current_a = a["current"].as_bool().unwrap_or(false);
                        current_b.cmp(&current_a).then_with(|| {
                            a["name"]
                                .as_str()
                                .unwrap_or("")
                                .cmp(b["name"].as_str().unwrap_or(""))
                        })
                    });
                    Response::json(200, json!({ "branches": branches, "current": current }))
                }
                Err(error) => Response::from_error(&error),
            }
        }
        ("GET", Some("log")) => {
            let requested = request
                .param("limit")
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(100)
                .clamp(1, 500);
            let query = request.param("query").unwrap_or("").trim().to_lowercase();
            // `--max-count` applies before the query filter, so a search has
            // to scan deeper than the page it returns.
            let limit = if query.is_empty() { requested } else { 500 };
            let head = git(deps, &["rev-parse", "HEAD"])
                .unwrap_or_default()
                .trim()
                .replace('\r', "")
                .to_owned();
            match git(
                deps,
                &[
                    "log",
                    "--all",
                    &format!("--max-count={limit}"),
                    "--format=%H%x1f%h%x1f%an%x1f%ct%x1f%D%x1f%s",
                ],
            ) {
                Ok(raw) => {
                    let commits: Vec<Value> = raw
                        .lines()
                        .map(|line| line.trim_end_matches('\r'))
                        .filter(|line| !line.is_empty())
                        .filter_map(|line| {
                            let mut parts = line.splitn(7, '\x1f');
                            let hash = parts.next()?;
                            let short = parts.next().unwrap_or("");
                            let author = parts.next().unwrap_or("");
                            let at = parts.next().unwrap_or("0");
                            let refs = parts.next().unwrap_or("");
                            let subject = parts.next().unwrap_or("");
                            if hash.is_empty() {
                                return None;
                            }
                            if !query.is_empty()
                                && !format!("{subject} {hash} {short} {author}")
                                    .to_lowercase()
                                    .contains(&query)
                            {
                                return None;
                            }
                            Some(json!({
                                "hash": hash,
                                "short": short,
                                "subject": subject,
                                "author": author,
                                "at": at.trim().parse::<i64>().unwrap_or(0) * 1000,
                                "refs": refs
                                    .split(',')
                                    .map(str::trim)
                                    .filter(|r| !r.is_empty())
                                    .collect::<Vec<_>>(),
                            }))
                        })
                        .take(requested)
                        .collect();
                    Response::json(200, json!({ "commits": commits, "head": head }))
                }
                Err(error) => Response::from_error(&error),
            }
        }
        ("POST", Some("checkout")) => {
            // `checkoutRef` was the handler — the ref check, the repo gate and
            // the dirty-tree conflict all ran inside apply, so they sit in
            // `work` and a refusal leaves intent+abort on `git:repo`.
            let reference = request.text("ref").unwrap_or("").trim().to_owned();
            let (entry, checked_out) = match submit_command(
                deps,
                request,
                "git.checkout",
                "git:repo",
                json!({ "ref": reference }),
                "POST /git/checkout",
                |deps, _| {
                    if let Err(error) = assert_safe_ref(&reference) {
                        return Err(Response::from_error(&error));
                    }
                    let status = git_status(deps);
                    if !status["repo"].as_bool().unwrap_or(false) {
                        return Err(Response::from_error(&CommandError::new(
                            ErrorCode::Failed,
                            "not a git repository",
                        )));
                    }
                    if git_dirty(&status) {
                        return Err(Response::from_error(&CommandError::new(
                            ErrorCode::Conflict,
                            "uncommitted tracked changes — commit or discard them before switching",
                        )));
                    }
                    match git(deps, &["checkout", &reference]) {
                        Ok(_) => {
                            let branch = git(deps, &["rev-parse", "--abbrev-ref", "HEAD"])
                                .unwrap_or_default()
                                .trim()
                                .replace('\r', "")
                                .to_owned();
                            let hash = git(deps, &["rev-parse", "HEAD"])
                                .unwrap_or_default()
                                .trim()
                                .replace('\r', "")
                                .chars()
                                .take(8)
                                .collect::<String>();
                            Ok(json!({"branch": branch, "hash": hash}))
                        }
                        Err(error) => Err(Response::from_error(&error)),
                    }
                },
            ) {
                Ok(pair) => pair,
                Err(response) => return response,
            };
            accepted(&entry, 200, checked_out)
        }
        ("POST", Some("create-branch")) => {
            let name = request.text("name").unwrap_or("").trim().to_owned();
            let start_point = request.text("startPoint").unwrap_or("").trim().to_owned();
            let (entry, branched) = match submit_command(
                deps,
                request,
                "git.create-branch",
                "git:repo",
                json!({ "name": name, "startPoint": start_point }),
                "POST /git/create-branch",
                |deps, _| {
                    // `createBranch`'s validation all ran inside apply.
                    if name.is_empty() {
                        return Err(invalid("a branch name is required"));
                    }
                    if name.len() > 256 {
                        return Err(invalid("branch name is too long"));
                    }
                    if git(deps, &["check-ref-format", "--branch", &name]).is_err() {
                        return Err(invalid("invalid branch name"));
                    }
                    if !start_point.is_empty() {
                        if let Err(error) = assert_safe_ref(&start_point) {
                            return Err(Response::from_error(&error));
                        }
                    }
                    let status = git_status(deps);
                    if !status["repo"].as_bool().unwrap_or(false) {
                        return Err(Response::from_error(&CommandError::new(
                            ErrorCode::Failed,
                            "not a git repository",
                        )));
                    }
                    if git_dirty(&status) {
                        return Err(Response::from_error(&CommandError::new(
                            ErrorCode::Conflict,
                            "uncommitted tracked changes — commit or discard them before switching",
                        )));
                    }
                    let mut args = vec!["checkout", "-b", name.as_str()];
                    if !start_point.is_empty() {
                        args.push(start_point.as_str());
                    }
                    match git(deps, &args) {
                        Ok(_) => {
                            let hash = git(deps, &["rev-parse", "HEAD"])
                                .unwrap_or_default()
                                .trim()
                                .replace('\r', "")
                                .chars()
                                .take(8)
                                .collect::<String>();
                            Ok(json!({"branch": name, "hash": hash}))
                        }
                        Err(error) => Err(Response::from_error(&error)),
                    }
                },
            ) {
                Ok(pair) => pair,
                Err(response) => return response,
            };
            accepted(&entry, 200, branched)
        }
        _ => not_found("no such git route"),
    }
}

/// The journal as `GET /journal` reports it — `entries` are the persisted
/// `{seq, at, phase, actorId, type, target, payload, …}` lines verbatim.
fn journal_route(request: &Request, deps: &mut RouteDeps<'_>) -> Response {
    let env = match environment(
        deps,
        "GET /journal",
        "the journal lives on disk next to the app",
    ) {
        Ok(env) => env,
        Err(response) => return response,
    };
    let log = match env.journal() {
        Ok(log) => log,
        Err(message) => {
            return Response::from_error(&CommandError::new(ErrorCode::Failed, message))
        }
    };
    let since = request
        .param("since")
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| *value >= 0.0)
        .unwrap_or(0.0) as u64;
    let entries: Vec<Value> = log
        .entries()
        .iter()
        .filter(|entry| entry.seq > since)
        .filter_map(|entry| serde_json::to_value(entry).ok())
        // `since(seq, limit=500)` — the shell capped a page at 500 entries.
        .take(500)
        .collect();
    Response::json(
        200,
        json!({ "lastSeq": log.sequence(), "entries": entries }),
    )
}

/// The canvas the journal describes — widgets keyed as the fold keeps them.
fn folded_canvas<'e>(
    env: &mut (dyn RouteEnv + 'e),
) -> Result<crate::projection::CanvasState, Response> {
    let log = env
        .journal()
        .map_err(|message| Response::from_error(&CommandError::new(ErrorCode::Failed, message)))?;
    Ok(crate::projection::fold(
        log.entries(),
        crate::projection::CanvasState::default(),
        crate::projection::Clock(deps_now_f64()),
    ))
}

fn deps_now_f64() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// A journaled widget as `/widgets` reports it — the field list the
/// TypeScript's `listWidgets` projection produced. Pub so the engine's axum
/// handlers can return the same row `patchWidget` handed back.
pub fn widget_row(widget: &crate::projection::Widget) -> Value {
    let mut row = json!({
        "id": widget.id,
        "title": widget.title,
        "kind": widget.kind,
        "version": widget.version,
        "x": widget.x,
        "y": widget.y,
        "w": widget.w,
        "h": widget.h,
        "z": widget.z,
        "maximized": widget.maximized,
    });
    if let Some(path) = &widget.image_path {
        row["imagePath"] = json!(path);
    }
    if let Some(name) = &widget.image_name {
        row["imageName"] = json!(name);
    }
    row
}

/// `widget.create`'s apply — shared by `POST /widgets` and the `/canvas/media`
/// import route, both of which submitted the same command. Mints the id,
/// stamps it on the commit payload (`p.id = id`), applies the registry
/// defaults, and returns the stored widget — the row the fold produces.
fn widget_create_apply(deps: &mut RouteDeps<'_>, payload: &mut Value) -> Result<Value, Response> {
    let kind = payload.get("kind").and_then(Value::as_str).map(str::to_owned);
    let generated = format!(
        "{}-{}-{}",
        kind.as_deref().unwrap_or("widget"),
        deps.now,
        &uuid::Uuid::new_v4().simple().to_string()[..5]
    );
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or(generated);
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    {
        return Err(invalid("widget id must be 1–128 chars of [A-Za-z0-9._:-]"));
    }
    let env = environment(deps, "POST /widgets", "creating a widget commits to the journal")?;
    let canvas = folded_canvas(env)?;
    if canvas.widgets.contains_key(&id) {
        return Err(Response::from_error(&CommandError::new(
            ErrorCode::Conflict,
            format!("widget {id} already exists"),
        )));
    }
    payload["id"] = json!(id.clone());
    // `(p.title?.trim() || widgetType(p.kind)?.label || id).slice(0, 120)` —
    // the TypeScript title ladder.
    let fallback = kind
        .as_deref()
        .map(|kind| {
            let mut chars = kind.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_else(|| kind.to_owned())
        })
        .unwrap_or_else(|| id.clone());
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(&fallback)
        .chars()
        .take(120)
        .collect::<String>();
    payload["title"] = json!(title);
    // `widgetType(kind)?.defaultSize ?? 520×360` — the registry's five
    // entries; every other kind falls through to the fallback.
    let (default_w, default_h) = match kind.as_deref() {
        Some("terminal") => (620.0, 380.0),
        Some("planner") => (420.0, 520.0),
        Some("files") => (580.0, 480.0),
        Some("browser") => (720.0, 480.0),
        Some("orchestration") => (520.0, 560.0),
        _ => (520.0, 360.0),
    };
    for (key, default) in [
        ("x", 0.0),
        ("y", 0.0),
        ("w", default_w),
        ("h", default_h),
        ("z", 1.0),
    ] {
        if payload.get(key).and_then(Value::as_f64).is_none() {
            payload[key] = json!(default);
        }
    }
    if payload.get("maximized").is_none() {
        payload["maximized"] = json!(false);
    }
    // `putWidget` returned the stored widget — build the same row the fold
    // will produce (version 1 on a create).
    let mut widget = json!({
        "id": id,
        "title": payload["title"],
        "kind": payload.get("kind").cloned().unwrap_or(Value::Null),
        "version": 1,
        "x": payload["x"],
        "y": payload["y"],
        "w": payload["w"],
        "h": payload["h"],
        "z": payload["z"],
        "maximized": payload["maximized"],
    });
    for key in ["imagePath", "imageName"] {
        if let Some(value) = payload.get(key) {
            widget[key] = value.clone();
        }
    }
    Ok(widget)
}

fn widgets_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let id = segments.get(1).map(String::as_str);
    match (method, id) {
        // `{widgets: [...shells, ...others]}` — terminals first, then every
        // journaled widget that is not a shell.
        ("GET", None) => {
            let env = match environment(
                deps,
                "GET /widgets",
                "listing widgets reads the journal and the PTY table",
            ) {
                Ok(env) => env,
                Err(response) => return response,
            };
            let canvas = match folded_canvas(env) {
                Ok(canvas) => canvas,
                Err(response) => return response,
            };
            let mut widgets = env.terminals();
            widgets.extend(
                canvas
                    .widgets
                    .values()
                    .filter(|widget| widget.kind.as_deref() != Some("terminal"))
                    .map(widget_row),
            );
            Response::json(200, json!({ "widgets": widgets }))
        }
        // `widget.create` — the TypeScript journaled the raw body as intent
        // and the handler's stamped payload as the commit, so the id minting,
        // the existence check and the registry defaults all live in `work`:
        // a bad id or a taken name leaves intent+abort, as `apply()` did.
        // The body is journaled verbatim — `agentId` included.
        ("POST", None) => {
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            match submit_command(
                deps,
                request,
                "widget.create",
                "widget:new",
                payload,
                "POST /widgets",
                widget_create_apply,
            ) {
                Ok((entry, widget)) => accepted(&entry, 201, widget),
                Err(response) => response,
            }
        }
        // controlServer.ts routes `POST /widgets/terminal` to the same
        // `terminal.create` submit as `POST /terminal` — title trimmed to
        // 200 chars, cwd to 1024, both optional. The spawn is the handler:
        // a failed PTY leaves intent+abort, not a bogus commit.
        ("POST", Some("terminal")) => {
            let title = request
                .text("title")
                .map(|text| text.trim().chars().take(200).collect::<String>())
                .filter(|text| !text.is_empty());
            // `body.cwd.slice(0, 1024)` — raw, not workspace-resolved at the
            // route; the apply's `p.cwd || defaultCwd()` resolved it.
            let cwd = request
                .text("cwd")
                .map(|text| text.chars().take(1024).collect::<String>());
            // `undefined` keys did not serialize — a missing title/cwd leaves
            // no key in the journaled payload rather than a null.
            let mut create_payload = json!({ "agentOwned": true });
            if let Some(title) = &title {
                create_payload["title"] = json!(title);
            }
            if let Some(cwd) = &cwd {
                create_payload["cwd"] = json!(cwd);
            }
            // `{id, title, cwd, ready}` — the original handler's return is
            // the response's `data`, so `work` hands it back.
            match submit_command(
                deps,
                request,
                "terminal.create",
                "terminal:new",
                create_payload,
                "POST /widgets/terminal",
                |deps, _payload| {
                    // `terminals.reserve` ran inside the apply — the mount's
                    // own `terminal.spawn` entry arrives later, when the widget
                    // adopts the running shell.
                    let id = environment(
                        deps,
                        "POST /widgets/terminal",
                        "spawning a terminal needs the running app",
                    )?
                    .next_terminal_id("agent");
                    environment(
                        deps,
                        "POST /widgets/terminal",
                        "spawning a terminal needs the running app",
                    )
                    .and_then(|env| {
                        env.spawn_terminal(
                            &id,
                            title.as_deref().unwrap_or(&id),
                            cwd.as_deref().unwrap_or(""),
                        )
                        .map_err(|error| {
                            Response::from_error(&CommandError::new(
                                ErrorCode::Failed,
                                error,
                            ))
                        })
                    })?;
                    // `waitUntilRunning(10_000)` — a shell that never reaches
                    // its read loop gets disposed and the command fails.
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        let running = environment(
                            deps,
                            "POST /widgets/terminal",
                            "spawning a terminal needs the running app",
                        )
                        .map(|env| env.terminal_running(&id))?;
                        if running {
                            break;
                        }
                        if std::time::Instant::now() >= deadline {
                            if let Ok(env) = environment(
                                deps,
                                "POST /widgets/terminal",
                                "spawning a terminal needs the running app",
                            ) {
                                let _ = env.dispose_terminal(&id);
                            }
                            return Err(Response::from_error(&CommandError::new(
                                ErrorCode::Failed,
                                "the Slate window did not open a terminal for this request",
                            )));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    Ok(json!({
                        "id": id,
                        "title": title.clone().unwrap_or_else(|| id.clone()),
                        "cwd": cwd.clone().unwrap_or_default(),
                        "ready": true,
                    }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        // `widget.update` — the title-trimming and terminal-setTitle the
        // original's apply() ran.
        ("PATCH", Some(raw)) => {
            if raw.is_empty()
                || raw.len() > 128
                || !raw
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return invalid("invalid id");
            }
            // The intent and the commit both journaled `body` verbatim —
            // `agentId`, the un-trimmed title, everything. The apply worked
            // on a normalized copy ({…p, title trimmed or deleted}), so the
            // clean-up happens on the response row, not the journaled payload.
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            let clean_title = payload
                .get("title")
                .and_then(Value::as_str)
                .map(|title| title.trim().chars().take(200).collect::<String>())
                .filter(|title| !title.is_empty());
            // `widget:new` + a title renamed whatever the write target
            // floated to — the focused terminal's widget — when no widget
            // was literally named `new`. The original redirected inside the
            // apply and still journaled `widget:new`; here the submit
            // happens on the resolved target so the fold actually applies
            // the rename (the original's replay silently lost it) — the
            // observable behavior is identical, the record better.
            let mut target_id = raw.to_owned();
            if target_id == "new" && clean_title.is_some() {
                if let Ok(env) = environment(
                    deps,
                    "PATCH /widgets/{id}",
                    "updating a widget commits to the journal",
                ) {
                    if let Ok(canvas) = folded_canvas(&mut *env) {
                        if !canvas.widgets.contains_key("new") {
                            if let Some(preferred) = env.resolve_write_target("") {
                                if canvas.widgets.contains_key(&preferred) {
                                    target_id = preferred;
                                }
                            }
                        }
                    }
                }
            }
            // `patchWidget`'s not_found and the `terminals.setTitle` side
            // effect both ran inside apply — inside `work`, so a stale id
            // leaves intent+abort rather than no entry at all. `work`
            // returns the merged row `patchWidget` answered with: the commit
            // hasn't landed yet, so it's built from the existing widget the
            // same way the fold will rebuild it.
            match submit_command(
                deps,
                request,
                "widget.update",
                &format!("widget:{target_id}"),
                payload,
                "PATCH /widgets/{id}",
                |deps, _| {
                    let env = environment(
                        deps,
                        "PATCH /widgets/{id}",
                        "updating a widget commits to the journal",
                    )?;
                    let canvas = folded_canvas(env)?;
                    let Some(existing) = canvas.widgets.get(&target_id) else {
                        return Err(not_found(format!("widget {target_id} not found")));
                    };
                    // `updated.kind === 'terminal' || !updated.kind` — the
                    // setTitle condition in the original's apply.
                    if existing.kind.as_deref().is_none_or(|k| k == "terminal") {
                        if let (Some(title), Some(env)) =
                            (clean_title.as_ref(), deps.env.as_deref_mut())
                        {
                            env.set_terminal_title(&target_id, title);
                        }
                    }
                    // `{…payload, title: cleanTitle | deleted}` — the copy the
                    // apply handed `patchWidget`, not the journaled raw body.
                    let mut patch = request.body.clone();
                    if !patch.is_object() {
                        patch = json!({});
                    }
                    match &clean_title {
                        Some(title) => patch["title"] = json!(title),
                        None => {
                            if let Some(map) = patch.as_object_mut() {
                                map.remove("title");
                            }
                        }
                    }
                    let mut merged = widget_row(existing);
                    if let (Some(row), Some(patch)) =
                        (merged.as_object_mut(), patch.as_object())
                    {
                        for (key, value) in patch {
                            row.insert(key.clone(), value.clone());
                        }
                    }
                    merged["version"] = json!(existing.version + 1.0);
                    Ok(merged)
                },
            ) {
                Ok((entry, widget)) => accepted(&entry, 200, widget),
                Err(response) => response,
            }
        }
        // `widget.remove` for widgets, `terminal.dispose` for shells — the
        // original chose by `terminals.has(id)`, so a name that only resolves
        // to a live shell takes the dispose path here as well.
        ("DELETE", Some(raw)) => {
            if raw.is_empty()
                || raw.len() > 128
                || !raw
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return invalid("invalid id");
            }
            let is_terminal = match environment(
                deps,
                "DELETE /widgets/{id}",
                "removing a widget commits to the journal",
            ) {
                Ok(env) => env.terminal_exists(raw),
                Err(response) => return response,
            };
            if is_terminal {
                // `terminal.dispose`'s apply killed the PTY, forgot the
                // snapshot and failed the shell's live dispatches — all
                // inside `work`, so a dead PTY leaves intent+abort.
                match submit_command(
                    deps,
                    request,
                    "terminal.dispose",
                    &format!("terminal:{raw}"),
                    json!({}),
                    "DELETE /widgets/{id}",
                    |deps, _| {
                        let env = environment(
                            deps,
                            "DELETE /widgets/{id}",
                            "disposing a shell needs the PTY table",
                        )?;
                        if let Err(message) = env.dispose_terminal(raw) {
                            return Err(Response::from_error(&CommandError::new(
                                ErrorCode::Failed,
                                message,
                            )));
                        }
                        // `failTerminalDispatches` — settle anything still
                        // running on the dead shell.
                        let live: Vec<String> = deps
                            .orchestration
                            .list_dispatches(None, None, Some(raw))
                            .iter()
                            .filter(|dispatch| dispatch.state == "running")
                            .map(|dispatch| dispatch.id.clone())
                            .collect();
                        let now = deps.now;
                        for dispatch_id in live {
                            let _ = deps
                                .orchestration
                                .settle_dispatch(&dispatch_id, "failed", None, now);
                        }
                        Ok(json!({ "id": raw }))
                    },
                ) {
                    Ok((entry, data)) => {
                        // `requestWidgetRemoval` — the shell asked the
                        // renderer to retire the tile; natively the fold is
                        // the widget list, so the remove is journaled here.
                        let on_canvas = environment(
                            deps,
                            "DELETE /widgets/{id}",
                            "reading the canvas refolds the journal",
                        )
                        .ok()
                        .and_then(|env| folded_canvas(env).ok())
                        .is_some_and(|canvas| canvas.widgets.contains_key(raw));
                        if on_canvas {
                            let _ = submit_command(
                                deps,
                                request,
                                "widget.remove",
                                &format!("widget:{raw}"),
                                json!({}),
                                "DELETE /widgets/{id}",
                                |_, _| Ok(json!({ "id": raw })),
                            );
                        }
                        return accepted(&entry, 200, data);
                    }
                    Err(response) => return response,
                }
            }
            let on_canvas = match environment(
                deps,
                "DELETE /widgets/{id}",
                "reading the canvas refolds the journal",
            ) {
                Ok(env) => match folded_canvas(env) {
                    Ok(canvas) => canvas.widgets.contains_key(raw),
                    Err(response) => return response,
                },
                Err(response) => return response,
            };
            match submit_command(
                deps,
                request,
                "widget.remove",
                &format!("widget:{raw}"),
                json!({}),
                "DELETE /widgets/{id}",
                |_, _| {
                    // `removeWidget` threw not_found inside apply.
                    if !on_canvas {
                        return Err(not_found(format!("widget {raw} not found")));
                    }
                    Ok(json!({ "id": raw }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        _ => not_found("no such widgets route"),
    }
}

/// `POST /canvas/*` — the journaled canvas verbs. `focus` is answered by the
/// axum route (the live camera is the view's business), so this arm never
/// sees it; what remains are the state commits.
fn canvas_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    match (method, segments.get(1).map(String::as_str)) {
        ("POST", Some("camera")) => {
            // `canvas.camera` — `submit(body, …, CANVAS_TARGET, body)` journaled
            // the body verbatim.
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            match submit_command(
                deps,
                request,
                "canvas.camera",
                "canvas:main",
                payload,
                "POST /canvas/camera",
                |_, payload| {
                    // `setCamera` returned the sanitized camera — same
                    // sanitize the fold runs, answered straight from the
                    // handler.
                    let camera = crate::projection::sanitize_camera(Some(payload));
                    Ok(json!({
                        "camera": {
                            "x": camera.x,
                            "y": camera.y,
                            "zoom": camera.zoom,
                        }
                    }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("POST", Some("strokes")) => {
            let payload =
                json!({ "strokes": request.body.get("strokes").cloned().unwrap_or(Value::Null) });
            match submit_command(
                deps,
                request,
                "canvas.strokes",
                "canvas:main",
                payload,
                "POST /canvas/strokes",
                |_, payload| {
                    // `setStrokes` answered the sanitized list's length.
                    let count = crate::projection::sanitize_strokes(payload.get("strokes")).len();
                    Ok(json!({ "count": count }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("POST", Some("connections")) => {
            let payload = json!({ "connections": request.body.get("connections").cloned().unwrap_or(Value::Null) });
            match submit_command(
                deps,
                request,
                "canvas.connections",
                "canvas:main",
                payload,
                "POST /canvas/connections",
                |_, payload| {
                    // `setConnections` answered the sanitized list's length.
                    let count = crate::projection::sanitize_connections(
                        payload.get("connections"),
                        crate::projection::Clock(deps_now_f64()),
                    )
                    .len();
                    Ok(json!({ "count": count }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("POST", Some("import")) => {
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            match submit_command(
                deps,
                request,
                "canvas.import",
                "canvas:main",
                payload,
                "POST /canvas/import",
                |_, payload| {
                    // `importFromRenderer` answered `{applied, skipped,
                    // removed, removedWidgets}`. The fold merges additively —
                    // every sanitizable widget lands and nothing is removed —
                    // so skipped counts the raws sanitize dropped.
                    let clock = crate::projection::Clock(deps_now_f64());
                    let incoming = payload
                        .get("widgets")
                        .and_then(Value::as_array)
                        .map(Vec::len)
                        .unwrap_or(0);
                    let applied = payload
                        .get("widgets")
                        .and_then(Value::as_array)
                        .map(|widgets| {
                            widgets
                                .iter()
                                .filter(|raw| {
                                    crate::projection::sanitize_widget(raw, clock).is_some()
                                })
                                .count()
                        })
                        .unwrap_or(0);
                    Ok(json!({
                        "applied": applied,
                        "skipped": incoming.saturating_sub(applied),
                        "removed": 0,
                        "removedWidgets": [],
                    }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        // `/canvas/media` and `/canvas/image` are the original's one route:
        // the file imports into the media store, then `widget.create` places
        // it — `image` keeps the image-only extension gate and lands an
        // `image` widget; `media` takes any file into a `browser` widget and
        // reports the classified `kind`. The shell then broadcast
        // `control:open-media` so the fresh widget learned what to show —
        // natively that reference rides in the widget's journaled
        // `state.media`, the same `{mediaUrl, path, name, kind}` shape.
        ("POST", Some(kind @ ("media" | "image"))) => {
            let image_widget = kind == "image";
            let route = if image_widget {
                "POST /canvas/image"
            } else {
                "POST /canvas/media"
            };
            let source = request
                .text("path")
                .or_else(|| request.text("image"))
                .or_else(|| request.text("file"))
                .map(str::trim)
                .filter(|source| !source.is_empty())
                .map(str::to_owned);
            let Some(source) = source else {
                return invalid("canvas media needs a file path");
            };
            let path = std::path::Path::new(&source);
            if source.starts_with("\\\\") || source.starts_with("//") || !path.is_absolute() {
                return invalid(format!("path must be absolute and local: {source}"));
            }
            let media_kind = crate::attachments::classify_media(path);
            if image_widget && media_kind != crate::attachments::MediaKind::Image {
                return invalid(format!("{source} is not an image"));
            }
            let media_dir = match environment(
                deps,
                route,
                "importing a file needs the media store",
            ) {
                Ok(env) => env.data_dir().join("media"),
                Err(response) => return response,
            };
            let imported = match crate::attachments::import_media(path, &media_dir) {
                Ok(path) => path,
                Err(message) => return invalid(message),
            };
            // `file.name` — the content-addressed store name.
            let name = imported
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = request
                .text("title")
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .map(str::to_owned)
                .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| {
                    if image_widget {
                        "Image".to_owned()
                    } else {
                        "Browser".to_owned()
                    }
                });
            let widget_kind = if image_widget { "image" } else { "browser" };
            let canvas = environment(deps, route, "placing the widget refolds the journal")
                .ok()
                .and_then(|env| folded_canvas(env).ok());
            let z = canvas
                .as_ref()
                .map(|canvas| canvas.widgets.values().map(|w| w.z).fold(0.0_f64, f64::max) + 1.0)
                .unwrap_or(1.0);
            let x = request
                .body
                .get("x")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| {
                    canvas
                        .as_ref()
                        .map(|canvas| canvas.camera.x + 120.0)
                        .unwrap_or(0.0)
                });
            let y = request
                .body
                .get("y")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| {
                    canvas
                        .as_ref()
                        .map(|canvas| canvas.camera.y + 80.0)
                        .unwrap_or(0.0)
                });
            // Widget defaults — the registry's `defaultSize` for each kind.
            let (w, h) = if image_widget {
                (480.0, 360.0)
            } else {
                (720.0, 480.0)
            };
            // `mediaKindOf` — the route's five-bucket kind, which the
            // response and the widget's media record both report.
            let kind = crate::attachments::media_kind_of(path);
            // The original let `widget.create`'s handler mint and stamp the
            // id (`p.id = id`), so the intent carries no id — same here.
            let mut payload = json!({
                "title": title, "kind": widget_kind,
                "x": x, "y": y, "w": w, "h": h, "z": z,
                "maximized": false,
            });
            if image_widget {
                payload["imagePath"] = json!(imported.to_string_lossy());
                payload["imageName"] = json!(name);
            } else {
                payload["state"] = json!({
                    "media": {
                        "mediaUrl": format!("orc://media/{name}"),
                        "path": imported.to_string_lossy(),
                        "name": path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| title.clone()),
                        "kind": kind,
                    }
                });
            }
            match submit_command(deps, request, "widget.create", "widget:new", payload, route, widget_create_apply) {
                Ok((entry, widget)) => {
                    let id = widget
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    // `control:open-media` — the renderer opened the file in
                    // the new browser widget; the fold's `state.media` block
                    // is the native port of that handoff.
                    accepted(
                        &entry,
                        201,
                        json!({
                            "id": id,
                            "title": widget.get("title").cloned().unwrap_or(json!(title)),
                            "path": imported.to_string_lossy(),
                            "name": name,
                            "kind": kind,
                            "widgetKind": widget_kind,
                        }),
                    )
                }
                Err(response) => response,
            }
        }
        _ => not_found("no such canvas route"),
    }
}

/// `routePlanner` — a journal-committed `plan.*` per mutation, a folded read
/// for the list. The summary is the original's, computed over the same fold.
fn planner_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let id = segments.get(1).map(String::as_str);
    let verb = segments.get(2).map(String::as_str);

    /// The fold as a read model — `planner.list()` sorts by `order`.
    fn plan_state<'e>(
        env: &mut (dyn RouteEnv + 'e),
    ) -> Result<crate::planner::PlannerState, Response> {
        let log = env.journal().map_err(|message| {
            Response::from_error(&CommandError::new(ErrorCode::Failed, message))
        })?;
        crate::planner::fold(
            log.entries(),
            crate::planner::PlannerState::default(),
            &crate::planner::today_local(),
        )
        .map_err(|invalid| {
            Response::from_error(&CommandError::new(ErrorCode::Failed, invalid.0))
        })
    }

    fn plan_items<'e>(
        env: &mut (dyn RouteEnv + 'e),
    ) -> Result<Vec<crate::planner::PlanItem>, Response> {
        let mut items: Vec<_> = plan_state(env)?.values().cloned().collect();
        items.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(items)
    }

    /// The commit `work` is about to journal, run through the real fold —
    /// the handler's `data` is then exactly what the journal produces, which
    /// is what the TypeScript's store answered inside `apply`.
    fn preview_plan<'e>(
        env: &mut (dyn RouteEnv + 'e),
        entry_type: &str,
        target: &str,
        payload: &Value,
        actor: &str,
        version: i64,
        now: i64,
    ) -> Result<crate::planner::PlannerState, Response> {
        let event = crate::journal::JournalEntry {
            seq: 0,
            at: now.max(0) as u64,
            phase: "commit".to_owned(),
            actor_id: actor.to_owned(),
            command_id: None,
            entry_type: entry_type.to_owned(),
            target: target.to_owned(),
            payload: Some(payload.clone()),
            version: Some(version),
            error: None,
            prev_hash: None,
            hash: None,
            workspace_dir: None,
        };
        crate::planner::reduce(&plan_state(env)?, &event, &crate::planner::today_local())
            .map_err(|bad| invalid(bad.0))
    }

    match (method, id, verb) {
        ("GET", None, _) => {
            let env = match environment(deps, "GET /planner", "the planner folds the journal") {
                Ok(env) => env,
                Err(response) => return response,
            };
            let items = match plan_items(env) {
                Ok(items) => items,
                Err(response) => return response,
            };
            Response::json(
                200,
                json!({ "items": items, "summary": planner_summary(&items) }),
            )
        }
        ("POST", None, _) => {
            // `submit(body, 'plan.create', …)` journaled the body verbatim.
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            // `createItem`'s throws ran inside apply — so the title and day
            // checks sit in `work` and leave intent+abort; the `order` stamp
            // joins the committed payload there, like `p.id` on a widget.
            let actor = actor(request).to_owned();
            match submit_command(
                deps,
                request,
                "plan.create",
                "plan:new",
                payload,
                "POST /planner",
                |deps, payload| {
                    let title = payload
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or_default();
                    if title.is_empty() {
                        return Err(invalid("title is required"));
                    }
                    if let Some(day) = payload.get("day") {
                        if !day.is_null() {
                            if let Err(invalid_day) = crate::planner::normalize_day(
                                Some(day),
                                &crate::planner::today_local(),
                            ) {
                                return Err(invalid(invalid_day.0));
                            }
                        }
                    }
                    let now = deps.now;
                    let env = environment(deps, "POST /planner", "the planner folds the journal")?;
                    let state = plan_state(env)?;
                    // The fold falls back to `plan-{at}-{seq}` — unknowable
                    // before the commit — so `work` mints the id and stamps
                    // it, the way `p.id = id` landed in the commit payload.
                    if payload
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .is_none_or(str::is_empty)
                    {
                        payload["id"] = json!(format!(
                            "plan-{}-{}",
                            now,
                            &uuid::Uuid::new_v4().simple().to_string()[..6]
                        ));
                    }
                    // `order: nextOrder(day)` — max order among same-day
                    // items + 1 — so new items sort after their day-mates.
                    let day = crate::planner::safe_day(
                        payload.get("day"),
                        &crate::planner::today_local(),
                    );
                    payload["order"] =
                        json!(crate::planner::next_order(state.values(), day.as_deref()));
                    let state = preview_plan(
                        env,
                        "plan.create",
                        "plan:new",
                        payload,
                        &actor,
                        1,
                        now,
                    )?;
                    let id = payload["id"].as_str().unwrap_or_default().to_owned();
                    Ok(state
                        .get(&id)
                        .map(|item| serde_json::to_value(item).unwrap_or_default())
                        .unwrap_or_else(|| json!({ "id": id })))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 201, data),
                Err(response) => response,
            }
        }
        ("POST", Some(raw), Some("toggle")) => {
            let payload = match request.body.get("done") {
                Some(Value::Bool(done)) => json!({ "done": done }),
                _ => json!({}),
            };
            // `toggleItem` threw not_found inside apply — inside `work`, so a
            // stale id leaves intent+abort like the TypeScript journal did.
            let actor = actor(request).to_owned();
            match submit_command(
                deps,
                request,
                "plan.toggle",
                &format!("plan:{raw}"),
                payload,
                "POST /planner/{id}/toggle",
                |deps, payload| {
                    let now = deps.now;
                    let env = environment(
                        deps,
                        "POST /planner/{id}/toggle",
                        "the planner folds the journal",
                    )?;
                    let existing = plan_state(env)?.get(raw).cloned().ok_or_else(|| {
                        not_found("plan item not found")
                    })?;
                    let version = (existing.version as i64) + 1;
                    let state = preview_plan(
                        env,
                        "plan.toggle",
                        &format!("plan:{raw}"),
                        payload,
                        &actor,
                        version,
                        now,
                    )?;
                    Ok(state
                        .get(raw)
                        .map(|item| serde_json::to_value(item).unwrap_or_default())
                        .unwrap_or_else(|| json!({ "id": raw })))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("PATCH", Some(raw), _) => {
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            // `updateItem` threw not_found and `normalizeDay` threw invalid —
            // both inside apply, so both live in `work` and leave intent+abort.
            let actor = actor(request).to_owned();
            match submit_command(
                deps,
                request,
                "plan.update",
                &format!("plan:{raw}"),
                payload,
                "PATCH /planner/{id}",
                |deps, payload| {
                    // `updateItem` looks the item up first — a stale id is
                    // not_found even when the day is malformed.
                    let now = deps.now;
                    let env = environment(
                        deps,
                        "PATCH /planner/{id}",
                        "the planner folds the journal",
                    )?;
                    let existing = plan_state(env)?.get(raw).cloned().ok_or_else(|| {
                        not_found("plan item not found")
                    })?;
                    if let Some(day) = payload.get("day") {
                        if !day.is_null() {
                            if let Err(invalid_day) = crate::planner::normalize_day(
                                Some(day),
                                &crate::planner::today_local(),
                            ) {
                                return Err(invalid(invalid_day.0));
                            }
                        }
                    }
                    let state = preview_plan(
                        env,
                        "plan.update",
                        &format!("plan:{raw}"),
                        payload,
                        &actor,
                        (existing.version as i64) + 1,
                        now,
                    )?;
                    Ok(state
                        .get(raw)
                        .map(|item| serde_json::to_value(item).unwrap_or_default())
                        .unwrap_or_else(|| json!({ "id": raw })))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("DELETE", Some(raw), _) => {
            // `deleteItem` threw not_found inside apply for a stale id.
            match submit_command(
                deps,
                request,
                "plan.delete",
                &format!("plan:{raw}"),
                json!({}),
                "DELETE /planner/{id}",
                |deps, _| {
                    let env = environment(
                        deps,
                        "DELETE /planner/{id}",
                        "the planner folds the journal",
                    )?;
                    if !match plan_items(env) {
                        Ok(items) => items.iter().any(|item| item.id == *raw),
                        Err(_) => false,
                    } {
                        return Err(not_found("plan item not found"));
                    }
                    Ok(json!({ "id": raw }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        _ => not_found("no such planner route"),
    }
}

/// `localDayKey`/`shiftLocalDay`/`uniqueProjects`/`summarizePlanner` — the
/// summary block `/planner` and `/snapshot` share. Days are the UTC-local
/// convention `planner.rs` already settled on.
fn planner_summary(items: &[crate::planner::PlanItem]) -> Value {
    let today = crate::planner::today_local().0;
    let week_end = shift_day(&today, 6);
    let open: Vec<_> = items.iter().filter(|item| !item.done).collect();
    let done: Vec<_> = items.iter().filter(|item| item.done).collect();
    let today_items: Vec<_> = items
        .iter()
        .filter(|item| item.day.as_deref() == Some(today.as_str()))
        .collect();
    let week_items: Vec<_> = items
        .iter()
        .filter(|item| {
            item.day
                .as_deref()
                .is_some_and(|day| day >= today.as_str() && day <= week_end.as_str())
        })
        .collect();
    let mut projects: Vec<String> = Vec::new();
    for item in items {
        if let Some(project) = item
            .project
            .as_deref()
            .map(str::trim)
            .filter(|project| !project.is_empty())
        {
            if !projects.iter().any(|seen| seen == project) {
                projects.push(project.to_owned());
            }
        }
    }
    json!({
        "total": items.len(),
        "open": open.len(),
        "done": done.len(),
        "today": today_items.len(),
        "todayOpen": today_items.iter().filter(|item| !item.done).count(),
        "week": week_items.len(),
        "weekOpen": week_items.iter().filter(|item| !item.done).count(),
        "projects": projects,
    })
}

/// `shiftLocalDay` — one-day calendar arithmetic on `YYYY-MM-DD`, the same
/// civil-date code `planner.rs` uses.
fn shift_day(key: &str, delta: i64) -> String {
    let parts: Vec<i64> = key.split('-').filter_map(|p| p.parse().ok()).collect();
    if parts.len() != 3 {
        return key.to_owned();
    }
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let days = {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (m + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    } + delta;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 - doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 3;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = (mp + 2) % 12 + 1;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

// --- notes: `workspace-notes.json`, the file the notes pane renders ---------

fn notes_document(data_dir: &std::path::Path) -> Value {
    crate::ipc::read_store_recovered(&data_dir.join("workspace-notes.json"))
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|document| document.is_object())
        .unwrap_or_else(|| json!({ "schemaVersion": 1, "items": [], "categoryColors": {} }))
}

fn notes_write(data_dir: &std::path::Path, document: &Value) -> Result<(), CommandError> {
    let path = data_dir.join("workspace-notes.json");
    let bytes = crate::jsjson::to_js_json_pretty(document, 2).into_bytes();
    crate::ipc::write_file_atomic(&path, &bytes)
        .map_err(|e| CommandError::new(ErrorCode::Failed, format!("notes write: {e}")))
}

/// `normalizeCategory`/`normalizeColor`/`normalizeTags` from notesStore.
fn normalize_category(value: Option<&Value>) -> Option<String> {
    let next = value?.as_str()?.trim().chars().take(60).collect::<String>();
    (!next.is_empty()).then_some(next)
}

fn normalize_color(value: Option<&Value>) -> Option<String> {
    let trimmed = value?.as_str()?.trim();
    if trimmed.len() == 7
        && trimmed.starts_with('#')
        && trimmed[1..].chars().all(|c| c.is_ascii_hexdigit())
    {
        Some(trimmed.to_lowercase())
    } else {
        None
    }
}

fn normalize_tags(value: Option<&Value>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let Some(tag) = entry.as_str() else { continue };
        let tag: String = tag.trim().to_lowercase().chars().take(40).collect();
        if tag.is_empty() || out.contains(&tag) {
            continue;
        }
        out.push(tag);
    }
    out
}

/// The resolved color a stored note wears — own override, else the category's,
/// else the palette default the notes pane falls back to.
fn note_color_for(document: &Value, category: Option<&str>, explicit: Option<String>) -> String {
    if let Some(color) = explicit {
        return color;
    }
    if let Some(name) = category {
        if let Some(color) = document
            .get("categoryColors")
            .and_then(Value::as_object)
            .and_then(|map| map.get(name))
            .and_then(|value| normalize_color(Some(value)))
        {
            return color;
        }
    }
    "#7aa2f7".to_owned()
}

fn notes_items(document: &Value) -> Vec<Value> {
    document
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn notes_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let id = segments.get(1).map(String::as_str);
    let third = segments.get(2).map(String::as_str);
    let now = deps.now as f64;
    let data_dir = match environment(deps, "/notes", "notes persist in the app's data dir") {
        Ok(env) => env.data_dir(),
        Err(response) => return response,
    };
    // The TS `submit()` committed a `note.*` to the journal first and the
    // store write was the apply — `body` verbatim, `agentId` included. The
    // journal entry is the audit; the file is the projection the notes pane
    // reads — both happen here, in that order.
    let mut payload = request.body.clone();
    if !payload.is_object() {
        payload = json!({});
    }
    match (method, id, third) {
        ("GET", None, _) => {
            let document = notes_document(&data_dir);
            let mut items = notes_items(&document);
            // `list()` orders by `order`.
            items.sort_by(|a, b| {
                a.get("order")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .partial_cmp(&b.get("order").and_then(Value::as_f64).unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Response::json(
                200,
                json!({
                    "schemaVersion": 1,
                    "items": items,
                    "categoryColors": document.get("categoryColors").cloned().unwrap_or(json!({})),
                }),
            )
        }
        ("POST", None, _) => {
            // `createItem` was the apply: the `workspace-notes.json` write —
            // and its `title is required` throw — sits between intent and
            // commit, so a failure aborts rather than leaving a committed
            // entry with no note behind it.
            let actor_id = actor(request).to_owned();
            let dir = data_dir.clone();
            match submit_command(
                deps,
                request,
                "note.create",
                "note:new",
                payload.clone(),
                "POST /notes",
                |_, note_payload| {
                    let title = note_payload
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect::<String>();
                    if title.is_empty() {
                        return Err(invalid("title is required"));
                    }
                    let mut document = notes_document(&dir);
                    let items = notes_items(&document);
                    let order = items
                        .iter()
                        .filter_map(|item| item.get("order").and_then(Value::as_f64))
                        .fold(0.0_f64, f64::max)
                        + 1.0;
                    let id = note_payload
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            format!(
                                "note-{}-{}",
                                now as i64,
                                &uuid::Uuid::new_v4().simple().to_string()[..6]
                            )
                        });
                    let category = normalize_category(note_payload.get("category"));
                    let color = note_color_for(
                        &document,
                        category.as_deref(),
                        normalize_color(note_payload.get("color")),
                    );
                    let item = json!({
                        "id": id,
                        "title": title,
                        "body": note_payload.get("body").and_then(Value::as_str).unwrap_or("").chars().take(20_000).collect::<String>(),
                        "tags": normalize_tags(note_payload.get("tags")),
                        "category": category,
                        "color": color,
                        "createdBy": actor_id,
                        "order": order,
                        "createdAt": now,
                        "updatedAt": now,
                        "version": 1,
                    });
                    let mut items = items;
                    items.push(item.clone());
                    document["items"] = Value::Array(items);
                    notes_write(&dir, &document)
                        .map_err(|error| Response::from_error(&error))?;
                    Ok(item)
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 201, data),
                Err(response) => response,
            }
        }
        ("POST", Some("recolor"), None) => {
            // `setCategoryColor` was the apply — the validation and the file
            // write both sit in `work`, so a bad color leaves intent+abort.
            let dir = data_dir.clone();
            match submit_command(
                deps,
                request,
                "note.recolor",
                "note:new",
                payload.clone(),
                "POST /notes/recolor",
                |_, note_payload| {
                    let category = normalize_category(note_payload.get("category"));
                    let color = normalize_color(note_payload.get("color"));
                    let (Some(category), Some(color)) = (category, color) else {
                        return Err(invalid("category and a #rrggbb color are required"));
                    };
                    let mut document = notes_document(&dir);
                    if !document["categoryColors"].is_object() {
                        document["categoryColors"] = json!({});
                    }
                    if let Some(map) = document["categoryColors"].as_object_mut() {
                        map.insert(category.clone(), json!(color.clone()));
                    }
                    let mut items = notes_items(&document);
                    for item in &mut items {
                        if item.get("category").and_then(Value::as_str)
                            == Some(category.as_str())
                        {
                            item["color"] = json!(&color);
                            item["updatedAt"] = json!(now);
                            item["version"] = json!(
                                item.get("version").and_then(Value::as_f64).unwrap_or(1.0) + 1.0
                            );
                        }
                    }
                    document["items"] = Value::Array(items);
                    notes_write(&dir, &document)
                        .map_err(|error| Response::from_error(&error))?;
                    Ok(json!({ "ok": true }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("PATCH", Some(raw), _) => {
            // `updateItem` was the apply — the not_found throw and the file
            // write sit in `work`, so a stale id leaves intent+abort.
            let dir = data_dir.clone();
            match submit_command(
                deps,
                request,
                "note.update",
                &format!("note:{raw}"),
                payload.clone(),
                "PATCH /notes/{id}",
                |_, note_payload| {
                    let mut document = notes_document(&dir);
                    let mut items = notes_items(&document);
                    let Some(index) = items
                        .iter()
                        .position(|item| item.get("id").and_then(Value::as_str) == Some(raw))
                    else {
                        return Err(not_found("note not found"));
                    };
                    {
                        let body = &note_payload;
                        let item = &mut items[index];
                        if let Some(title) = body.get("title").and_then(Value::as_str) {
                            let title: String = title.trim().chars().take(200).collect();
                            if !title.is_empty() {
                                item["title"] = json!(title);
                            }
                        }
                        if let Some(text) = body.get("body").and_then(Value::as_str) {
                            item["body"] =
                                json!(text.chars().take(20_000).collect::<String>());
                        }
                        if body.get("tags").is_some() {
                            item["tags"] = json!(normalize_tags(body.get("tags")));
                        }
                        let category_changed = body.get("category").is_some();
                        if category_changed {
                            match normalize_category(body.get("category")) {
                                Some(category) => item["category"] = json!(category),
                                None => {
                                    if let Some(map) = item.as_object_mut() {
                                        map.remove("category");
                                    }
                                }
                            }
                        }
                        if body.get("color").is_some() {
                            match normalize_color(body.get("color")) {
                                Some(color) => {
                                    item["color"] = json!(&color);
                                    // An explicit color joins the category's
                                    // shared palette entry, as the TypeScript
                                    // store did.
                                    if let Some(category) = item
                                        .get("category")
                                        .and_then(Value::as_str)
                                        .map(str::to_owned)
                                    {
                                        let mut colors = document
                                            .get("categoryColors")
                                            .and_then(Value::as_object)
                                            .cloned()
                                            .unwrap_or_default();
                                        colors.insert(category, json!(color));
                                        document["categoryColors"] = Value::Object(colors);
                                    }
                                }
                                None if category_changed => {
                                    item["color"] = json!(note_color_for(
                                        &document,
                                        item.get("category").and_then(Value::as_str),
                                        None,
                                    ));
                                }
                                None => {}
                            }
                        } else if category_changed {
                            item["color"] = json!(note_color_for(
                                &document,
                                item.get("category").and_then(Value::as_str),
                                None,
                            ));
                        }
                        if let Some(order) = body.get("order").and_then(Value::as_f64) {
                            if order.is_finite() {
                                item["order"] = json!(order);
                            }
                        }
                        item["updatedAt"] = json!(now);
                        item["version"] =
                            json!(item.get("version").and_then(Value::as_f64).unwrap_or(1.0) + 1.0);
                    }
                    let updated = items[index].clone();
                    document["items"] = Value::Array(items);
                    notes_write(&dir, &document).map_err(|error| Response::from_error(&error))?;
                    Ok(updated)
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        ("DELETE", Some(raw), _) => {
            // `deleteItem` never threw not_found — a stale id still committed
            // `note.delete` and answered `{id}` — so there is no check here,
            // just the file write in `work`.
            let dir = data_dir.clone();
            match submit_command(
                deps,
                request,
                "note.delete",
                &format!("note:{raw}"),
                json!({}),
                "DELETE /notes/{id}",
                |_, _| {
                    let mut document = notes_document(&dir);
                    let mut items = notes_items(&document);
                    items.retain(|item| item.get("id").and_then(Value::as_str) != Some(raw));
                    document["items"] = Value::Array(items);
                    notes_write(&dir, &document).map_err(|error| Response::from_error(&error))?;
                    Ok(json!({ "id": raw }))
                },
            ) {
                Ok((entry, data)) => accepted(&entry, 200, data),
                Err(response) => response,
            }
        }
        _ => not_found("no such notes route"),
    }
}

// fix-panes2: the links and timer panes' writes. Each POST/DELETE journals
// its `link.*` / `timer.*` entry first, then applies it to the file the pane
// reads — the same order `notes_route` uses.

// --- links: `workspace-links.json`, one list per links widget ---------------

/// The links document: the scoped `{widgetId: [...]}` object, or the bare
/// `[...]` the links pane's reader also accepts.
fn links_document(data_dir: &std::path::Path) -> Value {
    crate::ipc::read_store_recovered(&data_dir.join("workspace-links.json"))
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|document| document.is_object() || document.is_array())
        .unwrap_or_else(|| json!({}))
}

fn links_write(data_dir: &std::path::Path, document: &Value) -> Result<(), CommandError> {
    let path = data_dir.join("workspace-links.json");
    let bytes = crate::jsjson::to_js_json_pretty(document, 2).into_bytes();
    crate::ipc::write_file_atomic(&path, &bytes)
        .map_err(|e| CommandError::new(ErrorCode::Failed, format!("links write: {e}")))
}

/// The list one widget edits: `document[widget]` in the scoped form, the root
/// array itself in the bare form.
fn links_entries<'a>(document: &'a mut Value, widget: &str) -> &'a mut Vec<Value> {
    if document.is_array() {
        return document.as_array_mut().expect("checked above");
    }
    if !document.is_object() {
        *document = json!({});
    }
    let entry = &mut document[widget];
    if !entry.is_array() {
        *entry = json!([]);
    }
    entry.as_array_mut().expect("normalized above")
}

/// `looksDangerous`: control/space-stripped, lowercased prefix sniff —
/// catches `java\tscript:` tricks and plain `javascript:`/`data:`/`vbscript:`.
fn looks_dangerous(value: &str) -> bool {
    let compact: String = value
        .chars()
        .filter(|c| !c.is_ascii_control() && !c.is_whitespace() && *c != '\u{7f}')
        .collect::<String>()
        .to_lowercase();
    compact.starts_with("javascript:")
        || compact.starts_with("data:")
        || compact.starts_with("vbscript:")
}

/// Percent-decode for the second sniff pass (the original tried
/// `decodeURIComponent` then `decodeURI` before comparing). Non-UTF8 and
/// truncated escapes fall back to the raw text.
fn decoded_for_sniff(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| -> Option<u8> {
                match b {
                    b'0'..=b'9' => Some(b - b'0'),
                    b'a'..=b'f' => Some(b - b'a' + 10),
                    b'A'..=b'F' => Some(b - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| raw.to_owned())
}

/// The scheme run the original's `normalizeProtocol` extracted:
/// `^[a-zA-Z][a-zA-Z0-9+.-]*\s*:` → lowercased `name:`.
pub fn scheme_of(value: &str) -> Option<String> {
    let mut chars = value.trim_start().chars().peekable();
    if !chars.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut name = String::new();
    for c in chars.by_ref() {
        if c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-') || c.is_whitespace() {
            name.push(c);
            continue;
        }
        if c == ':' {
            let name: String = name.chars().filter(|c| !c.is_whitespace()).collect();
            if name.is_empty() {
                return None;
            }
            return Some(format!("{}:", name.to_lowercase()));
        }
        return None;
    }
    None
}

/// `isSafeUrl` (sanitizeUrl.ts): `javascript:`/`data:`/`vbscript:`/`file:`/
/// `blob:` and any other scheme outside `http:`/`https:`/`mailto:`/`tel:` is
/// rejected; relative addresses (`/`, `./`, `../`, `#`, or no scheme) pass —
/// `allowRelative` held true in the widget's renderer.
pub fn is_safe_url(url: &str) -> bool {
    let trimmed: String = url
        .trim()
        .chars()
        .filter(|c| !c.is_ascii_control() && *c != '\u{7f}')
        .collect();
    if trimmed.is_empty() {
        return false;
    }
    if looks_dangerous(&trimmed) || looks_dangerous(&decoded_for_sniff(&trimmed)) {
        return false;
    }
    if trimmed.starts_with('#')
        || trimmed.starts_with('/')
        || trimmed.starts_with("./")
        || trimmed.starts_with("../")
    {
        return true;
    }
    // `file%3a/etc/passwd` has no raw scheme and would pass as relative — the
    // original re-extracted the scheme from the *decoded* string and checked
    // it against BLOCKED_PROTOCOLS, which is what catches it.
    if let Some(decoded) = scheme_of(&decoded_for_sniff(&trimmed)) {
        if matches!(
            decoded.as_str(),
            "javascript:" | "data:" | "vbscript:" | "file:" | "blob:"
        ) || looks_dangerous(&decoded)
        {
            return false;
        }
    }
    match scheme_of(&trimmed).as_deref() {
        // No scheme at all → a plain relative path/name.
        None => true,
        Some(proto) => matches!(proto, "http:" | "https:" | "mailto:" | "tel:"),
    }
}

fn links_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let widget = segments.get(1).map(String::as_str);
    let link = segments.get(2).map(String::as_str);
    let now = deps.now;
    let data_dir = match environment(deps, "/links", "links persist in the app's data dir") {
        Ok(env) => env.data_dir(),
        Err(response) => return response,
    };
    let mut payload = request.body.clone();
    if !payload.is_object() {
        payload = json!({});
    }
    if let Some(map) = payload.as_object_mut() {
        map.remove("agentId");
    }
    let Some(widget) = widget else {
        return not_found("no such links route");
    };
    match (method, link) {
        ("GET", None) => {
            let document = links_document(&data_dir);
            let items = match document.get(widget).and_then(Value::as_array) {
                Some(items) => items.clone(),
                None => document.as_array().cloned().unwrap_or_default(),
            };
            Response::json(200, json!({ "items": items }))
        }
        ("POST", None) => {
            let title: String = payload
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .chars()
                .take(200)
                .collect();
            let url = payload
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            if title.is_empty() {
                return invalid("title is required");
            }
            if url.is_empty() || !is_safe_url(&url) {
                return invalid("url is required and must not use an unsafe scheme");
            }
            let id = payload
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("link-{now}"));
            let item = json!({ "id": id, "title": title, "url": url });
            submitted(
                deps,
                request,
                "link.create",
                &format!("widget:{widget}"),
                json!({ "widget": widget, "link": item.clone() }),
                "POST /links/{widget}",
                201,
                |_, _| {
                    let mut document = links_document(&data_dir);
                    links_entries(&mut document, widget).push(item.clone());
                    links_write(&data_dir, &document)
                        .map(|()| item.clone())
                        .map_err(|error| Response::from_error(&error))
                },
            )
        }
        ("DELETE", Some(id)) => {
            let mut document = links_document(&data_dir);
            let exists = links_entries(&mut document, widget)
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some(id));
            if !exists {
                return not_found("link not found");
            }
            submitted(
                deps,
                request,
                "link.delete",
                &format!("widget:{widget}"),
                json!({ "widget": widget, "id": id }),
                "DELETE /links/{widget}/{id}",
                200,
                |_, _| {
                    links_entries(&mut document, widget)
                        .retain(|item| item.get("id").and_then(Value::as_str) != Some(id));
                    links_write(&data_dir, &document)
                        .map(|()| json!({ "id": id }))
                        .map_err(|error| Response::from_error(&error))
                },
            )
        }
        _ => not_found("no such links route"),
    }
}

// --- timers: `workspace-timers.json`, one entry per timer widget ------------

/// The countdown a fresh timer starts from — 25 minutes, the original's boot.
pub const DEFAULT_TIMER_TOTAL_MS: i64 = 25 * 60_000;

/// Serializes the timer read-modify-write: the file holds one map for every
/// timer widget, so an unlocked RMW can drop a sibling's concurrent update.
static TIMERS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn timers_document(data_dir: &std::path::Path) -> Value {
    crate::ipc::read_store_recovered(&data_dir.join("workspace-timers.json"))
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// The Electron widget's start/pause toggle: pausing banks `deadline − now`
/// into `remaining`; starting arms a fresh `deadline` — a finished or idle
/// timer restarts from `totalMs` rather than resuming a negative remainder.
fn toggle_timer(entry: &mut serde_json::Map<String, Value>, now: f64) {
    let total = entry
        .get("totalMs")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TIMER_TOTAL_MS as f64);
    let running = entry
        .get("running")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    entry.insert("totalMs".into(), json!(total));
    if running {
        let deadline = entry.get("deadline").and_then(Value::as_f64).unwrap_or(now);
        entry.insert("remaining".into(), json!(deadline - now));
        entry.insert("running".into(), json!(false));
    } else {
        let remaining = entry
            .get("remaining")
            .and_then(Value::as_f64)
            .unwrap_or(total);
        let remaining = if remaining <= 0.0 { total } else { remaining };
        entry.insert("remaining".into(), json!(remaining));
        entry.insert("deadline".into(), json!(now + remaining));
        entry.insert("running".into(), json!(true));
        entry.insert("rang".into(), json!(false));
    }
}

/// The original's reset: stopped, full `remaining`, no deadline, `rang`
/// cleared so the next finish notifies again.
fn reset_timer_entry(entry: &mut serde_json::Map<String, Value>) {
    let total = entry
        .get("totalMs")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TIMER_TOTAL_MS as f64);
    entry.insert("totalMs".into(), json!(total));
    entry.insert("remaining".into(), json!(total));
    entry.insert("deadline".into(), json!(0.0));
    entry.insert("running".into(), json!(false));
    entry.insert("rang".into(), json!(false));
}

fn timers_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let widget = segments.get(1).map(String::as_str);
    let action = segments.get(2).map(String::as_str);
    let now = deps.now as f64;
    let data_dir = match environment(deps, "/timers", "timers persist in the app's data dir") {
        Ok(env) => env.data_dir(),
        Err(response) => return response,
    };
    match (method, widget, action) {
        ("POST", Some(widget), Some(action @ ("toggle" | "reset"))) => {
            let entry_type = if action == "toggle" {
                "timer.toggle"
            } else {
                "timer.reset"
            };
            submitted(
                deps,
                request,
                entry_type,
                &format!("widget:{widget}"),
                json!({ "widget": widget, "at": now }),
                "POST /timers/{widget}/{action}",
                200,
                |_, _| {
                    let _guard = TIMERS_LOCK
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let mut document = timers_document(&data_dir);
                    let mut timer = document
                        .get(widget)
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    if action == "toggle" {
                        toggle_timer(&mut timer, now);
                    } else {
                        reset_timer_entry(&mut timer);
                    }
                    document[widget] = Value::Object(timer.clone());
                    let bytes = crate::jsjson::to_js_json_pretty(&document, 2).into_bytes();
                    crate::ipc::write_file_atomic(&data_dir.join("workspace-timers.json"), &bytes)
                        .map(|()| Value::Object(timer))
                        .map_err(|error| {
                            Response::from_error(&CommandError::new(
                                ErrorCode::Failed,
                                format!("timers write: {error}"),
                            ))
                        })
                },
            )
        }
        _ => not_found("no such timers route"),
    }
}

// --- locks ------------------------------------------------------------------

/// `registerHttpAgent` without the actor table — the native server has no
/// actor registry to join, so the validation is what survives.
fn register_http_agent(request: &Request) -> Result<String, Response> {
    let agent_id = request.agent_id.as_deref().unwrap_or("").trim().to_owned();
    if agent_id.is_empty() {
        return Err(Response::json(
            401,
            json!({ "error": "agentId is required", "code": "unknown_actor" }),
        ));
    }
    if agent_id.len() > 128 {
        return Err(invalid("agentId must be 1–128 chars"));
    }
    if !agent_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-'))
    {
        return Err(invalid("agentId must be [A-Za-z0-9._@-] only"));
    }
    if matches!(agent_id.as_str(), "user" | "assistant" | "system") {
        return Err(Response::from_error(&CommandError::new(
            ErrorCode::Forbidden,
            format!("agentId \"{agent_id}\" is reserved"),
        )));
    }
    Ok(agent_id)
}

/// `normalizeLockResource` — a path is a `file:` resource, anything else is
/// already a resource id or nothing.
fn normalize_lock_resource(raw: Option<&Value>) -> String {
    let text = raw.and_then(Value::as_str).unwrap_or("").trim().to_owned();
    if let Some(parsed) = crate::resources::parse_resource(&text) {
        if parsed.scheme == "file" {
            return crate::resources::file_resource(&parsed.id);
        }
    }
    if (text.len() >= 3
        && text.as_bytes()[0].is_ascii_alphabetic()
        && text.as_bytes()[1] == b':'
        && matches!(text.as_bytes()[2], b'\\' | b'/'))
        || text.contains('\\')
        || text.starts_with('/')
    {
        return crate::resources::file_resource(&text);
    }
    text
}

fn lock_json(lock: &crate::locks::ResourceLock) -> Value {
    json!({
        "resource": lock.resource,
        "actorId": lock.actor_id,
        "acquiredAt": lock.acquired_at,
        "expiresAt": lock.expires_at,
        "reason": lock.reason,
        "implicit": lock.implicit,
    })
}

fn locks_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let now = deps.now;
    let env = match environment(deps, "/locks", "locks live on the running app") {
        Ok(env) => env,
        Err(response) => return response,
    };
    match (method, segments.get(1).map(String::as_str)) {
        ("GET", None) => Response::json(
            200,
            json!({ "locks": env.locks().list(now).iter().map(lock_json).collect::<Vec<_>>() }),
        ),
        ("POST", Some("heartbeat")) => {
            let agent_id = match register_http_agent(request) {
                Ok(agent_id) => agent_id,
                Err(response) => return response,
            };
            let ttl = request.body.get("ttlMs").and_then(Value::as_i64);
            Response::json(
                200,
                json!({ "renewed": env.locks().heartbeat(&agent_id, ttl, now) }),
            )
        }
        ("POST", _) => {
            let agent_id = match register_http_agent(request) {
                Ok(agent_id) => agent_id,
                Err(response) => return response,
            };
            let resource = normalize_lock_resource(request.body.get("resource"));
            match env.locks().acquire(
                crate::locks::AcquireInput {
                    resource: &resource,
                    actor_id: &agent_id,
                    ttl_ms: request.body.get("ttlMs").and_then(Value::as_i64),
                    reason: request
                        .body
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    implicit: false,
                },
                now,
            ) {
                Ok(lock) => Response::json(200, json!({ "lock": lock_json(&lock) })),
                Err(error) => Response::from_error(&error),
            }
        }
        ("DELETE", Some(raw)) => {
            let agent_id = match register_http_agent(request) {
                Ok(agent_id) => agent_id,
                Err(response) => return response,
            };
            let resource = normalize_lock_resource(Some(&Value::String(raw.to_owned())));
            match env.locks().release(&resource, &agent_id, now) {
                Ok(()) => Response::json(200, json!({ "ok": true })),
                Err(error) => Response::from_error(&error),
            }
        }
        _ => not_found("no such locks route"),
    }
}

// --- browser ---------------------------------------------------------------

/// `routeBrowser` — the control socket's browser surface. `GET /browser`
/// lists the canvas browser widgets (the shell's Code-surface browsers and
/// real-Chrome tabs never existed in the native build); `POST /browser/open`
/// mints a `kind: "browser"` widget at the camera-derived spot and navigates
/// it; the per-id verbs run `BrowserAgentAction`s over the wry eval bridge —
/// the same `{ok:true,data}`/`{error,code}` answers the broadcast version
/// produced, including the checkout guard on navigation.
fn browser_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let method = request.method.as_str();
    let now = deps.now;
    {
        let env = match environment(deps, "/browser", "browser control needs the running app") {
            Ok(env) => env,
            Err(response) => return response,
        };
        // `registerHttpAgent` + `locks.heartbeat` ran before every branch.
        let agent_id = match register_http_agent(request) {
            Ok(agent_id) => agent_id,
            Err(response) => return response,
        };
        env.locks().heartbeat(&agent_id, None, now);
    }

    match (method, segments.get(1).map(String::as_str)) {
        ("GET", None) => {
            let env = match environment(deps, "GET /browser", "the list folds the journal") {
                Ok(env) => env,
                Err(response) => return response,
            };
            let canvas = match folded_canvas(env) {
                Ok(canvas) => canvas,
                Err(response) => return response,
            };
            let mut widgets: Vec<&crate::projection::Widget> = canvas
                .widgets
                .values()
                .filter(|widget| widget.kind.as_deref() == Some("browser"))
                .collect();
            widgets.sort_by(|a, b| b.z.partial_cmp(&a.z).unwrap_or(std::cmp::Ordering::Equal));
            Response::json(
                200,
                json!({
                    "browsers": widgets
                        .iter()
                        .map(|w| json!({
                            "id": w.id, "title": w.title, "surface": "canvas",
                            "x": w.x, "y": w.y, "z": w.z,
                        }))
                        .collect::<Vec<_>>(),
                }),
            )
        }
        ("POST", Some("open")) if segments.len() == 2 => browser_open(request, deps),
        _ => browser_widget_action(request, segments, deps),
    }
}

/// Merge a patch into a browser widget's journaled `state` — the `set` op's
/// control-socket spelling: the shell persisted browser state in
/// `localStorage`, here it is `widget.state` via `widget.update`.
fn journal_browser_state(
    deps: &mut RouteDeps<'_>,
    request: &Request,
    id: &str,
    patch: Value,
) -> Result<(), Response> {
    let mut state = {
        let env = environment(deps, "/browser", "browser state commits to the journal")?;
        let canvas = folded_canvas(env)?;
        canvas
            .widgets
            .get(id)
            .and_then(|widget| widget.state.clone())
            .unwrap_or_else(|| json!({}))
    };
    if let (Some(dst), Some(src)) = (state.as_object_mut(), patch.as_object()) {
        for (key, value) in src {
            dst.insert(key.clone(), value.clone());
        }
    }
    submit_command(
        deps,
        request,
        "widget.update",
        &format!("widget:{id}"),
        json!({ "state": state.clone() }),
        "/browser",
        |deps, _| {
            let env = environment(deps, "/browser", "browser state commits to the journal")?;
            let canvas = folded_canvas(env)?;
            match canvas.widgets.get(id) {
                Some(existing) => {
                    let mut merged = widget_row(existing);
                    merged["state"] = state;
                    merged["version"] = json!(existing.version + 1.0);
                    Ok(merged)
                }
                None => Ok(Value::Null),
            }
        },
    )?;
    Ok(())
}

/// `POST /browser/open` — canvas surface only: the `chrome` (real browser)
/// and `code` (Code-workspace webview) surfaces have no native counterpart
/// and answer like the original's missing `broadcast`. The x/y math is the
/// original's camera placement verbatim.
fn browser_open(request: &Request, deps: &mut RouteDeps<'_>) -> Response {
    // `body.url === undefined` distinguishes "absent" from null, which
    // browserNavigationUrl then rejects — keep both branches.
    let target_url = match request.body.get("url") {
        None => None,
        Some(value) => match value
            .as_str()
            .and_then(crate::browser::browser_navigation_url)
        {
            Some(url) => Some(url),
            None => {
                return invalid("url must be a valid http(s) address; localhost is supported")
            }
        },
    };
    if target_url
        .as_deref()
        .map(crate::browser::is_protected_checkout_url)
        .unwrap_or(false)
    {
        return invalid("checkout navigation is disabled");
    }
    if let Some(surface) = request.body.get("surface") {
        let known = surface
            .as_str()
            .map(|s| matches!(s, "chrome" | "code" | "canvas"))
            .unwrap_or(false);
        if !known {
            return invalid("surface must be \"chrome\", \"code\" or \"canvas\"");
        }
    }
    // The shell defaulted to `chrome` — real Chrome over puppeteer — with
    // `code` for Code agents. The canvas widget is the only surface this
    // build can serve, so it is the default; explicit others get the 503 the
    // missing broadcast produced.
    let surface = request
        .body
        .get("surface")
        .and_then(Value::as_str)
        .unwrap_or("canvas");
    if surface != "canvas" {
        return Response::json(
            503,
            json!({
                "error": format!("browser surface \"{surface}\" is unavailable in the native build"),
                "code": "failed",
            }),
        );
    }
    let title = request
        .body
        .get("title")
        .and_then(Value::as_str)
        .map(|title| title.trim().chars().take(120).collect::<String>())
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Browser".to_owned());
    // `const { camera } = canvas.snapshot()` — the spawn point is a fixed
    // screen offset mapped through the zoom, stacked above the top widget.
    let (x, y, z) = {
        let env = match environment(
            deps,
            "POST /browser/open",
            "creating a widget commits to the journal",
        ) {
            Ok(env) => env,
            Err(response) => return response,
        };
        let canvas = match folded_canvas(env) {
            Ok(canvas) => canvas,
            Err(response) => return response,
        };
        let zoom = canvas.camera.zoom.max(0.1);
        (
            (-canvas.camera.x + 100.0) / zoom,
            (-canvas.camera.y + 105.0) / zoom,
            canvas
                .widgets
                .values()
                .map(|widget| widget.z)
                .fold(0.0, f64::max)
                + 1.0,
        )
    };
    let id = format!(
        "browser-{}-{}",
        deps.now,
        &uuid::Uuid::new_v4().simple().to_string()[..5]
    );
    // `widgetType("browser").defaultSize` — 720×480 like `POST /widgets`.
    let payload = json!({
        "id": id, "title": title, "kind": "browser",
        "x": x, "y": y, "w": 720.0, "h": 480.0, "z": z,
        "maximized": false,
    });
    if let Err(response) = submit_command(
        deps,
        request,
        "widget.create",
        "widget:new",
        payload,
        "POST /browser/open", widget_create_apply) {
        return response;
    }
    let Some(url) = target_url else {
        return Response::json(
            201,
            json!({ "ok": true, "data": { "id": id, "title": title, "surface": "canvas" } }),
        );
    };
    // `requestBrowserAgentAction(navigate)` — journal `state.url` first so a
    // not-yet-mounted webview loads the target on first paint, then drive
    // the real page load and await it like the original's 45s promise.
    if let Err(response) =
        journal_browser_state(deps, request, &id, json!({ "url": url }))
    {
        return response;
    }
    let action = crate::browser::BrowserAgentAction::Navigate { url: url.clone() };
    match crate::webview::agent_action(&id, &action) {
        Ok(_) => Response::json(
            201,
            json!({
                "ok": true,
                "data": { "id": id, "title": title, "surface": "canvas", "url": url },
            }),
        ),
        Err(error) => Response::json(
            502,
            json!({ "error": error, "code": "failed", "data": { "id": id } }),
        ),
    }
}

/// `String(body.x ?? '')` — JS coerces numbers and bools where a strict
/// `as_str` would not; the regex validation after it rejects everything else.
fn js_coerced_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// `/^\d{1,3}$/` plus the 1–100 range — `assertRefInRange`.
fn parse_ref(raw: &str) -> Option<u32> {
    if raw.is_empty() || raw.len() > 3 || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value = raw.parse::<u32>().ok()?;
    (1..=100).contains(&value).then_some(value)
}

/// The per-widget verbs — `POST /browser/{id}/{action}` and the GET
/// `snapshot` — validated exactly as `routeBrowser` validated them.
fn browser_widget_action(
    request: &Request,
    segments: &[String],
    deps: &mut RouteDeps<'_>,
) -> Response {
    let method = request.method.as_str();
    if segments.len() < 3 || !matches!(method, "POST" | "GET") {
        return not_found("no such browser route");
    }
    let raw_id = segments[1].as_str();
    if raw_id.is_empty()
        || raw_id.len() > 128
        || !raw_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    {
        return invalid("invalid browser widget id");
    }
    {
        let env = match environment(
            deps,
            "/browser/{id}",
            "browser actions need the running app",
        ) {
            Ok(env) => env,
            Err(response) => return response,
        };
        let canvas = match folded_canvas(env) {
            Ok(canvas) => canvas,
            Err(response) => return response,
        };
        let is_browser = canvas
            .widgets
            .get(raw_id)
            .map(|widget| widget.kind.as_deref() == Some("browser"))
            .unwrap_or(false);
        if !is_browser {
            return not_found(format!("browser widget {raw_id} not found"));
        }
    }

    use crate::browser::BrowserAgentAction as Action;
    let action = if segments[2] == "snapshot" && method == "GET" && segments.len() == 3 {
        Action::Snapshot
    } else if method == "POST" && segments.len() == 3 {
        match segments[2].as_str() {
            "navigate" => {
                let Some(url) = request
                    .text("url")
                    .and_then(crate::browser::browser_navigation_url)
                else {
                    return invalid(
                        "url must be a valid http(s) address; localhost is supported",
                    );
                };
                if crate::browser::is_protected_checkout_url(&url) {
                    return invalid("checkout navigation is disabled");
                }
                Action::Navigate { url }
            }
            "click" | "fill" | "select" => {
                let Some(ref_index) = parse_ref(&js_coerced_string(request.body.get("ref")))
                else {
                    return invalid("ref must be a number from 1 to 100");
                };
                if segments[2] == "click" {
                    Action::Click { ref_index }
                } else {
                    let Some(value) = request.text("value") else {
                        return invalid("value must be a string up to 4000 characters");
                    };
                    if value.chars().count() > 4000 {
                        return invalid("value must be a string up to 4000 characters");
                    }
                    if segments[2] == "select" {
                        Action::Select {
                            ref_index,
                            value: value.to_owned(),
                        }
                    } else {
                        Action::Fill {
                            ref_index,
                            value: value.to_owned(),
                        }
                    }
                }
            }
            "press" => {
                let key = js_coerced_string(request.body.get("key"));
                if !crate::browser::SUPPORTED_PRESS_KEYS.contains(&key.as_str()) {
                    return invalid("unsupported key");
                }
                let ref_index = match request.body.get("ref") {
                    None => None,
                    Some(_) => match parse_ref(&js_coerced_string(request.body.get("ref"))) {
                        Some(value) => Some(value),
                        None => return invalid("ref must be a number from 1 to 100"),
                    },
                };
                Action::Press { key, ref_index }
            }
            "scroll" => {
                // `Number(body.pixels ?? 600)` — strings parse, null defaults.
                let pixels = match request.body.get("pixels") {
                    None | Some(Value::Null) => 600.0,
                    Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
                    Some(Value::Bool(b)) => {
                        if *b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    Some(Value::String(s)) => {
                        s.trim().parse::<f64>().unwrap_or(f64::NAN)
                    }
                    Some(_) => f64::NAN,
                };
                if !pixels.is_finite() {
                    return invalid("pixels must be a number");
                }
                Action::Scroll {
                    pixels: pixels.clamp(-1200.0, 1200.0),
                }
            }
            _ => return not_found("unknown browser action"),
        }
    } else {
        return not_found("no such browser route");
    };

    // `navigate()`'s `setUrl` persisted the target — the journaled
    // `state.url` is the equivalent, and it doubles as the load directive
    // for a webview that has not been created yet. `setMedia(null)` rides
    // the same write.
    if let Action::Navigate { url } = &action {
        if let Err(response) = journal_browser_state(
            deps,
            request,
            raw_id,
            json!({ "url": url, "media": Value::Null }),
        ) {
            return response;
        }
    }

    match crate::webview::agent_action(raw_id, &action) {
        Ok(result) => Response::json(200, json!({ "ok": true, "data": result })),
        Err(error) => {
            Response::json(502, json!({ "error": error, "code": "failed" }))
        }
    }
}

// --- /media ------------------------------------------------------------------

/// `GET /media/{name}` — the control-socket port of the shell's `orc://media`
/// scheme and `media.dataUrl` IPC. The store is `{data_dir}/media`, where
/// `attachments::import_media` writes each import as `<sha256>.<ext>`; the
/// wire `Response` carries a JSON `body` only, so the bytes answer as a
/// `data:` URL through `Response::bytes` rather than as a raw body.
fn media_route(request: &Request, segments: &[String], deps: &mut RouteDeps<'_>) -> Response {
    let _ = request;
    let name = segments.get(1).map(String::as_str).unwrap_or("");
    // One plain file name — the segment arrives percent-decoded, so `..`,
    // separators and absolute spellings are already decoded; anything but a
    // single normal component resolves outside the media dir.
    let mut components = std::path::Path::new(name).components();
    let single_name = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if segments.len() != 2 || !single_name {
        return invalid("media needs a file name, not a path");
    }
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    // `mimeTypeForPath` over the full `MEDIA_MIME_BY_EXT` — every imported
    // kind is servable, not just images; an unknown spelling answers
    // `application/octet-stream`. `orc://media` refused to serve
    // `ACTIVE_EXTS` under their real type (markup/script would execute inside
    // the shell) and downgraded them to text/plain + attachment; over this
    // socket the downgrade is what survives.
    let content_type = if crate::attachments::is_active_extension(&extension) {
        "text/plain; charset=utf-8"
    } else {
        crate::attachments::mime_type_for_extension(&extension)
    };
    let env = match environment(deps, "GET /media", "media files live beside the journal") {
        Ok(env) => env,
        Err(response) => return response,
    };
    let file = env.data_dir().join("media").join(name);
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(_) => return not_found(format!("no such media: {name}")),
    };
    // The control client caps a response at 16 MB and base64 inflates by a
    // third — a bigger file answers with its identity and path so a local
    // consumer can read it directly.
    const INLINE_CAP: usize = 8 * 1024 * 1024;
    if bytes.len() > INLINE_CAP {
        return Response::json(
            200,
            json!({
                "name": name,
                "type": content_type,
                "size": bytes.len(),
                "path": file.to_string_lossy(),
                "dataUrl": Value::Null,
            }),
        );
    }
    let mut response = Response::bytes(200, &bytes, content_type);
    if let Value::Object(map) = &mut response.body {
        map.insert("name".into(), Value::from(name));
        map.insert(
            "path".into(),
            Value::from(file.to_string_lossy().into_owned()),
        );
    }
    response
}

// --- /snapshot ---------------------------------------------------------------

/// The command-type names the native build understands — `core.flow.types()`
/// in the TypeScript, the sorted `handlers` key set. File and transient
/// terminal verbs are registered there even though no HTTP route submits
/// them (the renderer's IPC submit did); the catalog advertised them anyway,
/// so the snapshot does the same.
const COMMAND_TYPES: &[&str] = &[
    "canvas.camera",
    "canvas.connections",
    "canvas.import",
    "canvas.strokes",
    "dispatch.account",
    "dispatch.settle",
    "dispatch.start",
    "file.create",
    "file.delete",
    "file.mkdir",
    "file.rename",
    "file.write",
    "gate.create",
    "gate.resolve",
    "git.branches",
    "git.checkout",
    "git.commit",
    "git.create-branch",
    "git.log",
    "git.refresh",
    "link.create",
    "link.delete",
    "note.create",
    "note.delete",
    "note.recolor",
    "note.update",
    "orc.ack",
    "orc.send",
    "orctask.create",
    "orctask.update",
    "plan.create",
    "plan.delete",
    "plan.toggle",
    "plan.update",
    "run.close",
    "run.create",
    "terminal.attach",
    "terminal.create",
    "terminal.dispose",
    "terminal.input",
    "terminal.resize",
    "terminal.spawn",
    "terminal.write",
    "timer.reset",
    "timer.toggle",
    "widget.create",
    "widget.remove",
    "widget.update",
];

/// `buildSnapshot` — presence + the world, one payload: terminals and
/// widgets, locks, the planner fold and a journal tail.
fn snapshot_route(request: &Request, deps: &mut RouteDeps<'_>) -> Response {
    let now = deps.now;
    let workspace_dir = deps.workspace_dir;
    let app_version = deps.app_version;
    let env = match environment(
        deps,
        "GET /snapshot",
        "a snapshot reads every store the app owns",
    ) {
        Ok(env) => env,
        Err(response) => return response,
    };
    let log = match env.journal() {
        Ok(log) => log,
        Err(message) => {
            return Response::from_error(&CommandError::new(ErrorCode::Failed, message))
        }
    };
    let canvas = crate::projection::fold(
        log.entries(),
        crate::projection::CanvasState::default(),
        crate::projection::Clock(deps_now_f64()),
    );
    let planner_items: Vec<crate::planner::PlanItem> = crate::planner::fold(
        log.entries(),
        crate::planner::PlannerState::default(),
        &crate::planner::today_local(),
    )
    .map(|items| {
        let mut items: Vec<_> = items.values().cloned().collect();
        items.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        items
    })
    .unwrap_or_default();
    let shells = env.terminals();
    let mut widgets = shells.clone();
    widgets.extend(
        canvas
            .widgets
            .values()
            .filter(|widget| widget.kind.as_deref() != Some("terminal"))
            .map(widget_row),
    );
    let last_seq = log.sequence();
    let since = request
        .param("since")
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| *value >= 0.0)
        .map(|value| value as u64)
        .unwrap_or_else(|| last_seq.saturating_sub(40));
    let entries: Vec<Value> = log
        .entries()
        .iter()
        .filter(|entry| entry.seq > since)
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    let locks: Vec<Value> = env.locks().list(now).iter().map(lock_json).collect();
    Response::json(
        200,
        json!({
            "ok": true,
            "app": "slate",
            "version": app_version,
            "pid": std::process::id(),
            "socketPath": env.socket_path(),
            "workspaceDir": workspace_dir,
            "terminals": shells,
            "widgets": widgets,
            "locks": locks,
            "planner": {
                "items": planner_items,
                "summary": planner_summary(&planner_items),
            },
            "journal": { "lastSeq": last_seq, "entries": entries },
            "commands": COMMAND_TYPES,
        }),
    )
}
