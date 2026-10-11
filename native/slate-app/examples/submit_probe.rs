//! Scratch verification driver — exercises `http::route` end-to-end against a
//! temp journal to prove the submit() gate chain: intent/commit/abort phases,
//! commandId+version stamps, baseVersion conflicts, lock conflicts, and
//! idempotent replay. Runs a stub RouteEnv so nothing touches the live app's
//! journal or terminals.

use std::sync::Mutex;

use serde_json::{json, Value};
use slate_app::command::{CommandError, ErrorCode};
use slate_app::http::{Request, RouteDeps, RouteEnv};
use slate_app::idempotency::IdempotencyCache;
use slate_app::journal_log::JournalLog;
use slate_app::locks::LockManager;
use slate_app::orchestration::OrchestrationStore;
use slate_app::queue::ActorRateLimiter;

struct StubEnv {
    journal_path: std::path::PathBuf,
    locks: Mutex<LockManager>,
    idempotency: Mutex<IdempotencyCache>,
    flow: Mutex<ActorRateLimiter>,
    /// The version the widget registry reports — the probe bumps it by hand.
    widget_version: Mutex<std::collections::HashMap<String, u64>>,
}

impl RouteEnv for StubEnv {
    fn journal(&mut self) -> Result<JournalLog, String> {
        JournalLog::open(&self.journal_path)
    }
    fn terminals(&mut self) -> Vec<Value> {
        Vec::new()
    }
    fn terminal_exists(&mut self, _id: &str) -> bool {
        false
    }
    fn dispose_terminal(&mut self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    fn set_terminal_title(&mut self, _id: &str, _title: &str) {}
    fn resolve_terminal(
        &mut self,
        _target: &str,
        _caller: Option<&str>,
    ) -> Result<String, CommandError> {
        Err(CommandError::new(
            ErrorCode::NotFound,
            "no terminals in the stub",
        ))
    }
    fn resolve_write_target(&mut self, _requested: &str) -> Option<String> {
        None
    }
    fn next_terminal_id(&mut self, prefix: &str) -> String {
        format!("{prefix}-1")
    }
    fn spawn_terminal(&mut self, _id: &str, _title: &str, _cwd: &str) -> Result<(), String> {
        Ok(())
    }
    fn terminal_running(&mut self, _id: &str) -> bool {
        true
    }
    fn write_terminal(&mut self, _id: &str, _text: &str, _enter: bool) -> Result<(), String> {
        Ok(())
    }
    fn tail_terminal(&mut self, _id: &str, _max: usize) -> Option<String> {
        None
    }
    fn locks(&self) -> std::sync::MutexGuard<'_, LockManager> {
        self.locks.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn data_dir(&self) -> std::path::PathBuf {
        std::env::temp_dir().join("slate-submit-probe")
    }
    fn socket_path(&self) -> Option<String> {
        None
    }
    fn target_version(&mut self, target: &str) -> u64 {
        self.widget_version
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(target)
            .copied()
            .unwrap_or(0)
    }
    fn idempotency(&mut self) -> std::sync::MutexGuard<'_, IdempotencyCache> {
        self.idempotency.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn rate_limit(&mut self, actor: &str, now: i64) -> bool {
        self.flow
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_consume(actor, 1.0, now)
    }
}

fn entries(env: &mut StubEnv) -> Vec<Value> {
    let log = JournalLog::open(&env.journal_path).expect("journal opens");
    log.entries()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect()
}

fn main() {
    let dir = std::env::temp_dir().join("slate-submit-probe");
    std::fs::create_dir_all(&dir).unwrap();
    let journal_path = dir.join("command-journal.ndjson");
    let _ = std::fs::remove_file(&journal_path);

    let mut store = OrchestrationStore::new();
    let mut env = StubEnv {
        journal_path: journal_path.clone(),
        locks: Mutex::new(LockManager::new(None)),
        idempotency: Mutex::new(IdempotencyCache::new(256, 5 * 60_000)),
        flow: Mutex::new(ActorRateLimiter::new(40.0, 25.0)),
        widget_version: Mutex::new(std::collections::HashMap::new()),
    };
    let mut failures = 0;
    let mut check = |name: &str, ok: bool, detail: Value| {
        if ok {
            println!("ok   {name}");
        } else {
            failures += 1;
            println!("FAIL {name}: {detail}");
        }
    };

    macro_rules! deps {
        () => {{
            RouteDeps {
                orchestration: &mut store,
                app_version: "0.0.1",
                workspace_dir: Some("/tmp"),
                now: 1_700_000_000_000,
                env: Some(&mut env),
            }
        }};
    }

    // 1. widget.create → intent + commit, commandId + version 1, data = row.
    let response = {
        let mut deps = deps!();
        let request = Request::get("POST", "/widgets")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "kind": "browser", "title": "demo", "x": 10, "y": 20,
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "widget.create accepted",
        response.status == 200 || response.status == 201,
        response.body.clone(),
    );
    let widget_id = response
        .body
        .pointer("/data/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    {
        let log = entries(&mut env);
        let tail: Vec<&Value> = log.iter().rev().take(2).rev().collect();
        let phases: Vec<&str> = tail
            .iter()
            .filter_map(|e| e.get("phase").and_then(Value::as_str))
            .collect();
        check(
            "create journaled intent+commit",
            phases == ["intent", "commit"],
            json!(tail),
        );
        let commit = tail.last().unwrap();
        check(
            "commit carries commandId+version 1",
            commit.get("commandId").is_some()
                && commit.get("version").and_then(Value::as_i64) == Some(1),
            (*commit).clone(),
        );
    }
    // The fold now knows the widget — reflect that into the stub registry.
    env.widget_version
        .lock()
        .unwrap()
        .insert(format!("widget:{widget_id}"), 1);

    // 2. widget.update with a wrong baseVersion → 409 conflict, no intent.
    let before = entries(&mut env).len();
    let response = {
        let mut deps = deps!();
        let request = (Request::get("PATCH", &format!("/widgets/{widget_id}")))
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "title": "stale", "baseVersion": 7,
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "stale baseVersion conflicts",
        response.status == 409
            && response.body.get("code").and_then(Value::as_str) == Some("conflict"),
        response.body.clone(),
    );
    check(
        "conflict leaves no journal entries",
        entries(&mut env).len() == before,
        json!(entries(&mut env).len()),
    );

    // 3. Lock held by another actor → 409 locked.
    env.locks()
        .acquire(
            slate_app::locks::AcquireInput {
                resource: &format!("widget:{widget_id}"),
                actor_id: "other",
                ttl_ms: None,
                reason: None,
                implicit: false,
            },
            1_700_000_000_000,
        )
        .unwrap();
    let response = {
        let mut deps = deps!();
        let request = (Request::get("PATCH", &format!("/widgets/{widget_id}")))
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "title": "blocked",
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "foreign lock conflicts",
        response.status == 409
            && response.body.get("code").and_then(Value::as_str) == Some("locked"),
        response.body.clone(),
    );
    env.locks()
        .release(&format!("widget:{widget_id}"), "other", 1_700_000_000_000)
        .unwrap();

    // 4. Implicit lock + successful patch → intent+commit, version bumps.
    let response = {
        let mut deps = deps!();
        let request = (Request::get("PATCH", &format!("/widgets/{widget_id}")))
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "title": "renamed", "baseVersion": 1,
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "matching baseVersion commits",
        response.status == 200,
        response.body.clone(),
    );
    env.widget_version
        .lock()
        .unwrap()
        .insert(format!("widget:{widget_id}"), 2);

    // 5. Idempotency replay — same key answers the cached result, no rerun.
    let key = "probe-key-1";
    let first = {
        let mut deps = deps!();
        let request = Request::get("POST", "/widgets")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "kind": "browser", "title": "idem", "idempotencyKey": key,
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    let second = {
        let mut deps = deps!();
        let request = Request::get("POST", "/widgets")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "kind": "browser", "title": "idem", "idempotencyKey": key,
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "idempotent replay returns same data",
        second.body.pointer("/data/id") == first.body.pointer("/data/id")
            && second.body.get("seq") == first.body.get("seq"),
        json!({ "first": first.body, "second": second.body }),
    );

    // 6. Abort — patch a widget that does not exist → intent+abort pair.
    let response = {
        let mut deps = deps!();
        let request = Request::get("PATCH", "/widgets/widget-missing")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "title": "ghost",
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "missing widget 404s",
        response.status == 404,
        response.body.clone(),
    );
    {
        let log = entries(&mut env);
        let tail: Vec<&Value> = log.iter().rev().take(2).rev().collect();
        let phases: Vec<&str> = tail
            .iter()
            .filter_map(|e| e.get("phase").and_then(Value::as_str))
            .collect();
        check(
            "failed work journals intent+abort",
            phases == ["intent", "abort"],
            json!(tail),
        );
    }

    // 7. Orchestration create through the flow — run.create commits at v1.
    let response = {
        let mut deps = deps!();
        let request = Request::get("POST", "/orchestration/runs")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "objective": "probe run",
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "run.create journaled",
        response.status == 201 && response.body.pointer("/data/id").is_some(),
        response.body.clone(),
    );
    let run_id = response
        .body
        .pointer("/data/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    // 8. orctask.create + version-gated update: wrong baseVersion → 409.
    let response = {
        let mut deps = deps!();
        let request = Request::get("POST", "/orchestration/tasks")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "runId": run_id, "spec": "probe task",
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    let task_id = response
        .body
        .pointer("/data/id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    check(
        "orctask.create accepted",
        response.status == 201,
        response.body.clone(),
    );
    let response = {
        let mut deps = deps!();
        let request = Request::get("PATCH", &format!("/orchestration/tasks/{task_id}"))
            .as_agent("probe")
            .with_body(json!({ "agentId": "probe", "status": "done", "baseVersion": 99 }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "orctask.update honors baseVersion",
        response.status == 409,
        response.body.clone(),
    );

    // 9. orc.send through the flow — mail lands in the store.
    let response = {
        let mut deps = deps!();
        let request = Request::get("POST", "/orchestration/messages")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "runId": run_id, "type": "note", "to": "@coordinator",
                "subject": "s", "body": "b",
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "orc.send journaled",
        response.status == 201,
        response.body.clone(),
    );

    // 10. POST /widgets/terminal → terminal.create intent+commit on
    // `terminal:new`; the PTY spawn (the apply's `terminals.reserve`) runs
    // inside the gated work.
    let before = entries(&mut env).len();
    let response = {
        let mut deps = deps!();
        let request = Request::get("POST", "/widgets/terminal")
            .as_agent("probe")
            .with_body(json!({
                "agentId": "probe", "title": "agent shell", "cwd": "/tmp",
            }));
        slate_app::http::route(&request, &mut deps).unwrap()
    };
    check(
        "terminal.create accepted",
        response.status == 200 && response.body["data"]["id"] == json!("agent-1"),
        response.body.clone(),
    );
    let spawned: Vec<String> = entries(&mut env)[before..]
        .iter()
        .map(|e| {
            format!(
                "{}:{}",
                e["type"].as_str().unwrap_or(""),
                e["phase"].as_str().unwrap_or("")
            )
        })
        .collect();
    check(
        "terminal.create journaled intent+commit",
        spawned == vec!["terminal.create:intent", "terminal.create:commit"],
        json!(spawned),
    );

    println!("\n{} failure(s)", failures);
    std::process::exit(if failures == 0 { 0 } else { 1 });
}
