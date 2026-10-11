//! The control-server route surface.
//!
//! Response shapes are the contract `slate --json` parses, so these assert on the
//! exact fields and status codes rather than on "something succeeded". A
//! renamed field or a changed status is a breaking change for every agent.

use serde_json::{json, Value};
use slate_app::http::{route, Request, Response, RouteDeps, RouteEnv};
use slate_app::orchestration::OrchestrationStore;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

const NOW: i64 = 1_000_000;

struct TestEnv {
    root: PathBuf,
    terminals: HashSet<String>,
    locks: Mutex<slate_app::locks::LockManager>,
    idem: Mutex<slate_app::idempotency::IdempotencyCache>,
}

impl TestEnv {
    fn new() -> Self {
        Self {
            root: std::env::temp_dir().join(format!("slate-http-{}", uuid::Uuid::new_v4())),
            terminals: ["alice", "worker", "term-1", "term-2"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            locks: Mutex::new(slate_app::locks::LockManager::new(None)),
            idem: Mutex::new(slate_app::idempotency::IdempotencyCache::new(64, 60_000)),
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl RouteEnv for TestEnv {
    fn journal(&mut self) -> Result<slate_app::journal_log::JournalLog, String> {
        slate_app::journal_log::JournalLog::open(self.root.join("journal.ndjson"))
    }

    fn terminals(&mut self) -> Vec<Value> {
        self.terminals
            .iter()
            .map(|id| json!({"id": id, "name": id, "alive": true}))
            .collect()
    }

    fn terminal_exists(&mut self, id: &str) -> bool {
        self.terminals.contains(id)
    }

    fn dispose_terminal(&mut self, id: &str) -> Result<(), String> {
        self.terminals.remove(id);
        Ok(())
    }

    fn set_terminal_title(&mut self, _id: &str, _title: &str) {}

    fn resolve_terminal(
        &mut self,
        target: &str,
        _caller: Option<&str>,
    ) -> Result<String, slate_app::command::CommandError> {
        if self.terminals.contains(target) {
            Ok(target.to_owned())
        } else {
            Err(slate_app::command::CommandError::new(
                slate_app::command::ErrorCode::NotFound,
                "unknown fixture terminal",
            ))
        }
    }

    fn resolve_write_target(&mut self, requested: &str) -> Option<String> {
        if self.terminals.contains(requested) {
            Some(requested.to_owned())
        } else {
            self.terminals.iter().next().cloned()
        }
    }

    fn next_terminal_id(&mut self, prefix: &str) -> String {
        let mut n = self.terminals.len() + 1;
        while self.terminals.contains(&format!("{prefix}-{n}")) {
            n += 1;
        }
        format!("{prefix}-{n}")
    }

    fn spawn_terminal(&mut self, id: &str, _title: &str, _cwd: &str) -> Result<(), String> {
        self.terminals.insert(id.to_owned());
        Ok(())
    }

    fn terminal_running(&mut self, id: &str) -> bool {
        self.terminals.contains(id)
    }

    fn write_terminal(&mut self, id: &str, _text: &str, _press_enter: bool) -> Result<(), String> {
        if self.terminals.contains(id) {
            Ok(())
        } else {
            Err("unknown fixture terminal".into())
        }
    }

    fn tail_terminal(&mut self, id: &str, _max: usize) -> Option<String> {
        self.terminals.contains(id).then(String::new)
    }

    fn locks(&self) -> MutexGuard<'_, slate_app::locks::LockManager> {
        self.locks.lock().unwrap()
    }

    fn data_dir(&self) -> PathBuf {
        self.root.clone()
    }

    fn socket_path(&self) -> Option<String> {
        None
    }

    fn target_version(&mut self, _target: &str) -> u64 {
        0
    }

    fn idempotency(&mut self) -> MutexGuard<'_, slate_app::idempotency::IdempotencyCache> {
        self.idem.lock().unwrap()
    }

    fn rate_limit(&mut self, _actor: &str, _now: i64) -> bool {
        true
    }
}

struct Server {
    store: OrchestrationStore,
    env: TestEnv,
}

impl Server {
    fn new() -> Self {
        Self {
            store: OrchestrationStore::new(),
            env: TestEnv::new(),
        }
    }

    fn call(&mut self, request: Request) -> Response {
        let mut deps = RouteDeps {
            orchestration: &mut self.store,
            app_version: "2.0.1",
            workspace_dir: Some("C:/work"),
            now: NOW,
            env: Some(&mut self.env),
        };
        route(&request, &mut deps).unwrap_or(Response {
            status: 404,
            body: json!({ "error": "not found" }),
        })
    }

    /// Seeds a run and returns its id.
    fn run(&mut self) -> String {
        let response = self.call(
            Request::get("POST", "/orchestration/runs")
                .with_body(json!({ "objective": "ship it" }))
                .as_agent("alice"),
        );
        assert_eq!(response.status, 201);
        response.body["data"]["id"].as_str().unwrap().to_owned()
    }

    fn task(&mut self, spec: &str) -> String {
        let response = self.call(
            Request::get("POST", "/orchestration/tasks")
                .with_body(json!({ "spec": spec }))
                .as_agent("alice"),
        );
        assert_eq!(response.status, 201, "{:?}", response.body);
        response.body["data"]["id"].as_str().unwrap().to_owned()
    }
}

#[test]
fn health_reports_the_app_and_workspace() {
    let mut server = Server::new();
    let response = server.call(Request::get("GET", "/health"));
    assert_eq!(response.status, 200);
    assert_eq!(response.body["ok"], json!(true));
    assert_eq!(response.body["app"], json!("slate"));
    assert_eq!(response.body["server"], json!("slate-control"));
    assert_eq!(response.body["version"], json!("2.0.1"));
    assert_eq!(response.body["workspaceDir"], json!("C:/work"));
}

#[test]
fn the_root_path_is_health_too() {
    let mut server = Server::new();
    assert_eq!(server.call(Request::get("GET", "/")).status, 200);
}

#[test]
fn an_unrouted_path_is_a_404() {
    let mut server = Server::new();
    assert_eq!(
        server.call(Request::get("GET", "/nothing/here")).status,
        404
    );
}

// --- runs -------------------------------------------------------------------

#[test]
fn creating_a_run_answers_201_with_the_accepted_envelope() {
    let mut server = Server::new();
    let response = server.call(
        Request::get("POST", "/orchestration/runs")
            .with_body(json!({ "objective": "ship it" }))
            .as_agent("alice"),
    );
    assert_eq!(response.status, 201);
    assert_eq!(response.body["ok"], json!(true));
    assert!(response.body["version"].is_number());
    assert_eq!(response.body["data"]["objective"], json!("ship it"));
    assert_eq!(
        response.body["data"]["coordinator"],
        json!("alice"),
        "the caller becomes the coordinator"
    );
}

#[test]
fn a_run_without_an_objective_is_refused_as_invalid() {
    let mut server = Server::new();
    let response = server.call(
        Request::get("POST", "/orchestration/runs")
            .as_agent("alice")
            .with_body(json!({ "objective": "  " })),
    );
    assert_eq!(response.status, 400);
    assert_eq!(response.body["code"], json!("invalid"));
    assert!(response.body["error"]
        .as_str()
        .unwrap()
        .contains("objective"));
}

#[test]
fn listing_runs_reports_the_active_one() {
    let mut server = Server::new();
    let id = server.run();
    let response = server.call(Request::get("GET", "/orchestration/runs"));
    assert_eq!(response.status, 200);
    assert_eq!(response.body["runs"].as_array().unwrap().len(), 1);
    assert_eq!(response.body["active"]["id"], json!(id));
}

#[test]
fn a_missing_run_is_a_404_carrying_the_code() {
    let mut server = Server::new();
    let response = server.call(Request::get("GET", "/orchestration/runs/run-999"));
    assert_eq!(response.status, 404);
    assert_eq!(response.body["code"], json!("not_found"));
}

#[test]
fn closing_a_run_clears_it_from_active() {
    let mut server = Server::new();
    let id = server.run();
    let response = server.call(
        Request::get("POST", &format!("/orchestration/runs/{id}/close"))
            .as_agent("alice")
            .with_body(json!({})),
    );
    assert_eq!(response.status, 200);
    assert!(response.body["data"]["closedAt"].is_number());

    let listed = server.call(Request::get("GET", "/orchestration/runs"));
    assert_eq!(listed.body["active"], Value::Null);
}

// --- tasks ------------------------------------------------------------------

#[test]
fn a_task_files_under_the_newest_open_run_when_none_is_named() {
    let mut server = Server::new();
    let run = server.run();
    let response = server.call(
        Request::get("POST", "/orchestration/tasks")
            .with_body(json!({ "spec": "do the work" }))
            .as_agent("alice"),
    );
    assert_eq!(response.status, 201);
    assert_eq!(response.body["data"]["runId"], json!(run));
    assert_eq!(response.body["data"]["status"], json!("ready"));
    assert_eq!(response.body["data"]["createdBy"], json!("alice"));
}

/// Filing a task with no run at all must say so rather than invent one.
#[test]
fn a_task_with_no_open_run_is_refused() {
    let mut server = Server::new();
    let response = server.call(
        Request::get("POST", "/orchestration/tasks")
            .as_agent("alice")
            .with_body(json!({ "spec": "work" })),
    );
    assert_eq!(response.status, 404);
    assert!(response.body["error"].as_str().unwrap().contains("run"));
}

#[test]
fn an_unknown_task_status_filter_is_refused_before_listing() {
    let mut server = Server::new();
    server.run();
    let response =
        server.call(Request::get("GET", "/orchestration/tasks").with_query("status", "teleported"));
    assert_eq!(response.status, 400);
    assert_eq!(response.body["code"], json!("invalid"));
}

#[test]
fn the_ready_filter_is_a_query_flag() {
    let mut server = Server::new();
    server.run();
    let first = server.task("first");
    server.call(
        Request::get("POST", "/orchestration/tasks")
            .with_body(json!({ "spec": "second", "deps": [first] }))
            .as_agent("alice"),
    );

    let all = server.call(Request::get("GET", "/orchestration/tasks"));
    assert_eq!(all.body["tasks"].as_array().unwrap().len(), 2);

    let ready = server.call(Request::get("GET", "/orchestration/tasks").with_query("ready", "1"));
    assert_eq!(
        ready.body["tasks"].as_array().unwrap().len(),
        1,
        "the dependent task is not ready"
    );
}

#[test]
fn patching_a_task_updates_it() {
    let mut server = Server::new();
    server.run();
    let id = server.task("work");
    let response = server.call(
        Request::get("PATCH", &format!("/orchestration/tasks/{id}"))
            .as_agent("alice")
            .with_body(json!({ "title": "Renamed" })),
    );
    assert_eq!(response.status, 200);
    assert_eq!(response.body["data"]["title"], json!("Renamed"));
}

// --- dispatches -------------------------------------------------------------

#[test]
fn dispatch_without_a_runtime_environment_is_rejected_without_mutation() {
    let mut server = Server::new();
    server.run();
    let task = server.task("work");
    let mut deps = RouteDeps {
        orchestration: &mut server.store,
        app_version: "2.0.1",
        workspace_dir: Some("C:/work"),
        now: NOW,
        env: None,
    };
    let response = route(
        &Request::get("POST", "/orchestration/dispatches")
            .as_agent("alice")
            .with_body(json!({"taskId": task, "terminalId": "term-1", "agent": "claude"})),
        &mut deps,
    )
    .unwrap();
    assert_eq!(response.status, 500);
    assert_eq!(response.body["needsEngineHandler"], true);
    assert!(server.store.list_dispatches(None, None, None).is_empty());
}

#[test]
fn a_dispatch_conflict_answers_409_with_the_code() {
    let mut server = Server::new();
    server.run();
    let task = server.task("work");
    let body = json!({ "taskId": task, "terminalId": "term-1", "agent": "claude" });

    assert_eq!(
        server
            .call(
                Request::get("POST", "/orchestration/dispatches")
                    .as_agent("alice")
                    .with_body(body.clone())
            )
            .status,
        201
    );

    let second = server.call(
        Request::get("POST", "/orchestration/dispatches")
            .as_agent("alice")
            .with_body(json!({ "taskId": task, "terminalId": "term-2", "agent": "codex" })),
    );
    assert_eq!(second.status, 409, "two workers on one task");
    assert_eq!(second.body["code"], json!("conflict"));
    assert_eq!(
        second.body["terminalId"],
        json!("term-1"),
        "error details are merged in at the top level"
    );
}

#[test]
fn settling_reports_the_task_status_and_what_it_promoted() {
    let mut server = Server::new();
    server.run();
    let first = server.task("first");
    let second_response = server.call(
        Request::get("POST", "/orchestration/tasks")
            .with_body(json!({ "spec": "second", "deps": [first.clone()] }))
            .as_agent("alice"),
    );
    let second = second_response.body["data"]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let dispatch = server.call(
        Request::get("POST", "/orchestration/dispatches")
            .as_agent("alice")
            .with_body(json!({ "taskId": first, "terminalId": "term-1", "agent": "claude" })),
    );
    let dispatch_id = dispatch.body["data"]["dispatchId"]
        .as_str()
        .unwrap()
        .to_owned();

    let settled = server.call(
        Request::get(
            "POST",
            &format!("/orchestration/dispatches/{dispatch_id}/settle"),
        )
        .as_agent("alice")
        .with_body(json!({ "outcome": "succeeded" })),
    );
    assert_eq!(settled.status, 200);
    assert_eq!(settled.body["data"]["status"], json!("completed"));
    assert_eq!(settled.body["data"]["promoted"], json!([second]));
}

#[test]
fn an_unknown_outcome_is_refused_before_the_store_is_touched() {
    let mut server = Server::new();
    server.run();
    let task = server.task("work");
    let dispatch = server.call(
        Request::get("POST", "/orchestration/dispatches")
            .as_agent("alice")
            .with_body(json!({ "taskId": task, "terminalId": "term-1", "agent": "claude" })),
    );
    let id = dispatch.body["data"]["dispatchId"]
        .as_str()
        .unwrap()
        .to_owned();

    let response = server.call(
        Request::get("POST", &format!("/orchestration/dispatches/{id}/settle"))
            .as_agent("alice")
            .with_body(json!({ "outcome": "mostly" })),
    );
    assert_eq!(response.status, 400);
    assert_eq!(response.body["code"], json!("invalid"));

    // The dispatch is untouched and can still be settled properly.
    let good = server.call(
        Request::get("POST", &format!("/orchestration/dispatches/{id}/settle"))
            .as_agent("alice")
            .with_body(json!({ "outcome": "succeeded" })),
    );
    assert_eq!(good.status, 200);
}

#[test]
fn the_dispatch_listing_names_what_is_unaccounted() {
    let mut server = Server::new();
    server.run();
    let task = server.task("work");
    let dispatch = server.call(
        Request::get("POST", "/orchestration/dispatches")
            .as_agent("alice")
            .with_body(json!({ "taskId": task, "terminalId": "term-1", "agent": "claude" })),
    );
    let id = dispatch.body["data"]["dispatchId"]
        .as_str()
        .unwrap()
        .to_owned();
    server.call(
        Request::get("POST", &format!("/orchestration/dispatches/{id}/settle"))
            .as_agent("alice")
            .with_body(json!({ "outcome": "succeeded" })),
    );

    let listed = server.call(Request::get("GET", "/orchestration/dispatches"));
    assert_eq!(listed.body["unaccounted"], json!([id.clone()]));

    server.call(
        Request::get("POST", &format!("/orchestration/dispatches/{id}/account"))
            .as_agent("alice")
            .with_body(json!({ "state": "released" })),
    );
    let after = server.call(Request::get("GET", "/orchestration/dispatches"));
    assert_eq!(after.body["unaccounted"], json!([]));
}

// --- messages and gates -----------------------------------------------------

#[test]
fn the_inbox_needs_a_caller_and_hides_acked_mail() {
    let mut server = Server::new();
    let run = server.run();

    let anonymous = server.call(Request::get("GET", "/orchestration/inbox"));
    assert_eq!(anonymous.status, 401, "an inbox belongs to someone");
    assert_eq!(anonymous.body["code"], json!("unknown_actor"));

    let sent = server.call(
        Request::get("POST", "/orchestration/messages")
            .with_body(json!({ "runId": run, "type": "ask", "to": "alice", "subject": "q", "body": "which?" }))
            .as_agent("worker"),
    );
    assert_eq!(sent.status, 201);
    let message_id = sent.body["data"]["id"].as_str().unwrap().to_owned();

    let inbox = server.call(Request::get("GET", "/orchestration/inbox").as_agent("alice"));
    assert_eq!(inbox.body["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox.body["waited"], json!(false));

    server.call(
        Request::get("POST", &format!("/orchestration/messages/{message_id}/ack"))
            .with_body(json!({}))
            .as_agent("alice"),
    );
    let after = server.call(Request::get("GET", "/orchestration/inbox").as_agent("alice"));
    assert!(after.body["messages"].as_array().unwrap().is_empty());
}

#[test]
fn an_unknown_inbox_type_filter_is_refused() {
    let mut server = Server::new();
    server.run();
    let response = server.call(
        Request::get("GET", "/orchestration/inbox")
            .with_query("types", "ask,telepathy")
            .as_agent("alice"),
    );
    assert_eq!(response.status, 400);
    assert_eq!(response.body["code"], json!("invalid"));
}

#[test]
fn replies_report_null_until_one_is_sent() {
    let mut server = Server::new();
    let run = server.run();
    let ask = server.call(
        Request::get("POST", "/orchestration/messages")
            .with_body(
                json!({ "runId": run, "type": "ask", "to": "alice", "subject": "q", "body": "?" }),
            )
            .as_agent("worker"),
    );
    let ask_id = ask.body["data"]["id"].as_str().unwrap().to_owned();

    let pending = server.call(Request::get(
        "GET",
        &format!("/orchestration/replies/{ask_id}"),
    ));
    assert_eq!(pending.status, 200);
    assert_eq!(pending.body["reply"], Value::Null);

    let missing = server.call(Request::get("GET", "/orchestration/replies/msg-999"));
    assert_eq!(missing.status, 404);
}

#[test]
fn a_gate_can_be_opened_listed_and_resolved_once() {
    let mut server = Server::new();
    let run = server.run();
    let created = server.call(
        Request::get("POST", "/orchestration/gates")
            .with_body(json!({ "runId": run, "question": "ship it?", "options": ["yes", "no"] }))
            .as_agent("alice"),
    );
    assert_eq!(created.status, 201);
    let id = created.body["data"]["id"].as_str().unwrap().to_owned();

    let open = server.call(Request::get("GET", "/orchestration/gates").with_query("open", "1"));
    assert_eq!(open.body["gates"].as_array().unwrap().len(), 1);

    let resolved = server.call(
        Request::get("POST", &format!("/orchestration/gates/{id}/resolve"))
            .as_agent("alice")
            .with_body(json!({ "resolution": "yes" })),
    );
    assert_eq!(resolved.status, 200);
    assert_eq!(resolved.body["data"]["resolution"], json!("yes"));

    let again = server.call(
        Request::get("POST", &format!("/orchestration/gates/{id}/resolve"))
            .as_agent("alice")
            .with_body(json!({ "resolution": "no" })),
    );
    assert_eq!(
        again.status, 409,
        "a resolved gate cannot be reopened by resolving it again"
    );
}

#[test]
fn the_snapshot_carries_every_collection() {
    let mut server = Server::new();
    server.run();
    server.task("work");
    let response = server.call(Request::get("GET", "/orchestration"));
    assert_eq!(response.status, 200);
    for key in ["runs", "tasks", "dispatches", "messages", "gates"] {
        assert!(response.body[key].is_array(), "snapshot is missing {key}");
    }
    assert_eq!(response.body["tasks"].as_array().unwrap().len(), 1);
}

/// Ids are generated and contain no reserved characters, but a caller may still
/// encode one — the router must decode before looking it up.
#[test]
fn a_percent_encoded_id_is_decoded() {
    let mut server = Server::new();
    let id = server.run();
    let encoded = id.replace('-', "%2D");
    let response = server.call(Request::get(
        "GET",
        &format!("/orchestration/runs/{encoded}"),
    ));
    assert_eq!(response.status, 200);
    assert_eq!(response.body["run"]["id"], json!(id));
}

// --- end to end: CLI plan into the router -----------------------------------

/// The two halves of block 4 must actually fit. A plan the CLI produces is fed
/// straight into the router, so a path or field the two disagree about shows up
/// here rather than in a running fleet.
mod cli_to_router {
    use super::*;
    use slate_app::cli::{plan, Args};

    fn run_cli(server: &mut Server, line: &str, agent: &str) -> Response {
        let tokens: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        let parsed = Args::parse(&tokens);
        let planned = plan(&tokens[0], &parsed).unwrap_or_else(|e| panic!("{line}: {}", e.message));

        let mut request = Request::get(&planned.method, &planned.path).as_agent(agent);
        for (key, value) in &planned.query {
            request = request.with_query(key, value);
        }
        if !planned.body.is_null() {
            request = request.with_body(planned.body.clone());
        }
        server.call(request)
    }

    #[test]
    fn a_whole_coordination_cycle_runs_through_both_halves() {
        let mut server = Server::new();

        let run = run_cli(&mut server, "run-create --objective ship-it", "alice");
        assert_eq!(run.status, 201, "{:?}", run.body);
        let run_id = run.body["data"]["id"].as_str().unwrap().to_owned();

        let first = run_cli(&mut server, "task-create --spec build", "alice");
        assert_eq!(first.status, 201, "{:?}", first.body);
        let first_id = first.body["data"]["id"].as_str().unwrap().to_owned();

        let second = run_cli(
            &mut server,
            &format!("task-create --spec test --deps {first_id}"),
            "alice",
        );
        assert_eq!(second.status, 201);
        let second_id = second.body["data"]["id"].as_str().unwrap().to_owned();

        // Only the independent task is dispatchable.
        let ready = run_cli(&mut server, "task-list --ready", "alice");
        let ids: Vec<&str> = ready.body["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec![first_id.as_str()]);

        let dispatched = run_cli(
            &mut server,
            &format!("worker-start --task {first_id} --terminal term-1 --agent claude"),
            "alice",
        );
        assert_eq!(dispatched.status, 201, "{:?}", dispatched.body);
        let dispatch_id = dispatched.body["data"]["dispatchId"]
            .as_str()
            .unwrap()
            .to_owned();

        // A second worker on the same task is refused with the code slate reports.
        let clash = run_cli(
            &mut server,
            &format!("worker-start --task {first_id} --terminal term-2 --agent codex"),
            "alice",
        );
        assert_eq!(clash.status, 409);
        assert_eq!(clash.body["code"], json!("conflict"));

        // The worker reports, which completes the task and unblocks the next.
        let settled = server.call(
            Request::get(
                "POST",
                &format!("/orchestration/dispatches/{dispatch_id}/settle"),
            )
            .as_agent("alice")
            .with_body(json!({ "outcome": "succeeded" })),
        );
        assert_eq!(settled.status, 200);
        assert_eq!(settled.body["data"]["promoted"], json!([second_id]));

        // The coordinator accounts for the finished worker.
        let released = run_cli(
            &mut server,
            &format!("worker-release {dispatch_id}"),
            "alice",
        );
        assert_eq!(released.status, 200);
        assert_eq!(released.body["data"]["state"], json!("released"));

        let status = run_cli(&mut server, &format!("status --run {run_id}"), "alice");
        assert_eq!(status.status, 200);
        assert_eq!(status.body["tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ask_and_reply_travel_through_the_inbox() {
        let mut server = Server::new();
        run_cli(&mut server, "run-create --objective ship", "alice");

        let asked = run_cli(&mut server, "ask --question which-way?", "worker");
        assert_eq!(asked.status, 201, "{:?}", asked.body);
        let ask_id = asked.body["data"]["id"].as_str().unwrap().to_owned();

        // The ask went to nobody in particular, so it is not in alice's inbox;
        // a reply to it is still findable by id.
        let pending = server.call(Request::get(
            "GET",
            &format!("/orchestration/replies/{ask_id}"),
        ));
        assert_eq!(pending.body["reply"], Value::Null);

        let replied = run_cli(&mut server, &format!("reply {ask_id} left"), "alice");
        assert_eq!(replied.status, 201, "{:?}", replied.body);

        let answered = server.call(Request::get(
            "GET",
            &format!("/orchestration/replies/{ask_id}"),
        ));
        assert_eq!(answered.body["reply"]["body"], json!("left"));
        assert_eq!(answered.body["reply"]["replyTo"], json!(ask_id));
    }

    #[test]
    fn a_permission_verb_answers_the_request_it_names() {
        let mut server = Server::new();
        run_cli(&mut server, "run-create --objective ship", "alice");

        let asked = run_cli(
            &mut server,
            "ask --question may-i-delete? --type permission",
            "worker",
        );
        let id = asked.body["data"]["id"].as_str().unwrap().to_owned();
        assert_eq!(asked.body["data"]["subject"], json!("permission_request"));

        let denied = run_cli(
            &mut server,
            &format!("deny {id} --reason too-risky"),
            "alice",
        );
        assert_eq!(denied.status, 201);
        assert_eq!(denied.body["data"]["body"], json!("deny: too-risky"));
        assert_eq!(denied.body["data"]["subject"], json!("permission_denied"));
        assert_eq!(denied.body["data"]["replyTo"], json!(id));
    }
}
