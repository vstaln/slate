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
    /// Open a fresh shell terminal — the dispatch path's `terminals.reserve`.
    /// The canvas's orphan-adoption turns it into a widget on the next tick.
    fn spawn_terminal(&mut self, title: &str, cwd: &str) -> Result<String, String>;
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

/// `submit(...)` for a journal-backed command: append the entry and hand it
/// back so the caller can fold the new state into its reply.
fn journal_commit(
    deps: &mut RouteDeps<'_>,
    request: &Request,
    entry_type: &str,
    target: &str,
    payload: Value,
    what: &str,
) -> Result<crate::journal::JournalEntry, Response> {
    let env = environment(
        deps,
        what,
        "mutating journal-backed state needs the running app",
    )?;
    let mut log = env
        .journal()
        .map_err(|message| Response::from_error(&CommandError::new(ErrorCode::Failed, message)))?;
    log.commit(actor(request), entry_type, target, payload)
        .map_err(|message| Response::from_error(&CommandError::new(ErrorCode::Failed, message)))
}

/// Routes one request. `None` means no route matched — the caller answers 404,
/// as the TypeScript does after falling through every branch.
pub fn route(request: &Request, deps: &mut RouteDeps<'_>) -> Option<Response> {
    let segments = request.segments();
    let method = request.method.as_str();
    let head = segments.first().map(String::as_str);

    match head {
        None | Some("health") if method == "GET" => Some(Response::json(
            200,
            json!({
                "ok": true,
                "app": "slate",
                "server": "slate-control",
                "version": deps.app_version,
                "workspaceDir": deps.workspace_dir,
            }),
        )),
        Some("orchestration") => orchestration_route(request, &segments, deps),
        Some("snapshot") if method == "GET" => Some(snapshot_route(request, deps)),
        Some("journal") if method == "GET" => Some(journal_route(request, deps)),
        Some("locks") => Some(locks_route(request, &segments, deps)),
        Some("git") => Some(git_route(request, &segments, deps)),
        Some("planner") => Some(planner_route(request, &segments, deps)),
        Some("note" | "notes") => Some(notes_route(request, &segments, deps)),
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
            let objective = request.text("objective").unwrap_or("");
            let coordinator = request.agent_id.as_deref().unwrap_or("api");
            Some(match store.create_run(objective, coordinator, deps.now) {
                Ok(run) => Response::accepted(201, run.version, 0, run_json(&run)),
                Err(error) => Response::from_error(&error),
            })
        }
        ("POST", Some("runs"), Some(id), Some("close")) => {
            Some(match store.close_run(id, deps.now) {
                Ok(run) => Response::accepted(200, run.version, 0, run_json(&run)),
                Err(error) => Response::from_error(&error),
            })
        }

        ("POST", Some("tasks"), None, _) => {
            let run_id = match store.resolve_run_id(request.text("runId")) {
                Ok(id) => id,
                Err(error) => return Some(Response::from_error(&error)),
            };
            let created_by = request.agent_id.as_deref().unwrap_or("api");
            Some(
                match store.create_task(
                    &run_id,
                    request.text("title"),
                    request.text("spec").unwrap_or(""),
                    &request.strings("deps"),
                    &request.strings("images"),
                    created_by,
                    deps.now,
                ) {
                    Ok(task) => Response::accepted(201, task.version, 0, task_json(&task)),
                    Err(error) => Response::from_error(&error),
                },
            )
        }
        ("PATCH", Some("tasks"), Some(id), _) => Some(
            match store.update_task(
                id,
                request.text("status"),
                request.text("title"),
                request.text("spec"),
                deps.now,
            ) {
                Ok(task) => Response::accepted(200, task.version, 0, task_json(&task)),
                Err(error) => Response::from_error(&error),
            },
        ),

        ("POST", Some("dispatches"), None, _) => {
            // Faithful port of the shell's `dispatch.start` command: resolve or
            // open a terminal, record the dispatch, launch the agent command,
            // sniff for an immediate launch failure, inject the preamble, and
            // mail the coordinator a dispatch notice. Store access goes through
            // `deps.orchestration` inline and env through `environment(deps)` —
            // alternating statement borrows, never held across each other.
            let task_id = request.text("taskId").unwrap_or("").to_owned();
            if task_id.is_empty() {
                return Some(invalid("taskId is required"));
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
                return Some(invalid("agent must be 1-64 chars of [A-Za-z0-9_.-]"));
            }
            let task = match deps.orchestration.require_task(&task_id) {
                Ok(task) => task.clone(),
                Err(error) => return Some(Response::from_error(&error)),
            };
            let run = match deps.orchestration.require_run(&task.run_id) {
                Ok(run) => run.clone(),
                Err(error) => return Some(Response::from_error(&error)),
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
                                Err(error) => return Some(Response::from_error(&error)),
                            }
                        }
                        Err(response) => return Some(response),
                    };
                    if let Some(busy) = deps.orchestration.dispatch_for_terminal(&id) {
                        return Some(Response::from_error(&CommandError::new(
                            ErrorCode::Conflict,
                            format!("terminal \"{id}\" is already running dispatch {}", busy.id),
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
                        Ok(env) => match env.spawn_terminal(&title, &cwd) {
                            Ok(id) => {
                                opened = true;
                                id
                            }
                            Err(error) => {
                                return Some(Response::from_error(&CommandError::new(
                                    ErrorCode::Failed,
                                    error,
                                )))
                            }
                        },
                        Err(response) => return Some(response),
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
                    return Some(Response::from_error(&error));
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
                            return Some(response);
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
                        return Some(Response::from_error(&CommandError::new(
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
                        Ok(env) => env.tail_terminal(&terminal_id, 16_000).unwrap_or_default(),
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
                        return Some(Response::from_error(&CommandError::new(
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
                request.agent_id.as_deref().unwrap_or("api"),
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

            Some(Response::accepted(
                201,
                dispatch.version,
                0,
                json!({
                    "dispatch": dispatch_json(&dispatch),
                    "dispatchId": dispatch.id,
                    "taskId": task.id,
                    "terminalId": terminal_id,
                    "agent": agent,
                    "injected": injected,
                }),
            ))
        }
        ("POST", Some("dispatches"), Some(id), Some("settle")) => {
            let outcome = request.text("outcome").unwrap_or("");
            if !OUTCOMES.contains(&outcome) {
                return Some(invalid(format!(
                    "outcome must be one of {}",
                    OUTCOMES.join(", ")
                )));
            }
            let files = request
                .field("filesModified")
                .map(|_| request.strings("filesModified"));
            Some(
                match deps
                    .orchestration
                    .settle_dispatch(id, outcome, files.clone(), deps.now)
                {
                    Ok((dispatch, task, promoted)) => {
                        // API-driven settles mail the coordinator the same
                        // worker_done the CLI path does — a watcher polling mail
                        // must see either shape of completion.
                        let actor = request.agent_id.as_deref().unwrap_or("api");
                        let _ = deps.orchestration.send(
                            &dispatch.run_id,
                            "worker_done",
                            actor,
                            "@coordinator",
                            "",
                            "",
                            Some(&task.id),
                            Some(&dispatch.id),
                            None,
                            Some(outcome),
                            files.unwrap_or_default(),
                            Vec::new(),
                            Vec::new(),
                            deps.now,
                        );
                        Response::accepted(
                            200,
                            dispatch.version,
                            0,
                            json!({
                                "dispatchId": dispatch.id,
                                "taskId": task.id,
                                "status": task.status,
                                "promoted": promoted,
                            }),
                        )
                    }
                    Err(error) => Response::from_error(&error),
                },
            )
        }
        ("POST", Some("dispatches"), Some(id), Some("account")) => {
            let state = request.text("state").unwrap_or("released");
            Some(match store.set_dispatch_state(id, state) {
                Ok(dispatch) => {
                    // `released && closeTerminal` also kills the worker's
                    // shell — the original disposed the PTY and journaled
                    // `widget.remove` so the canvas tile goes with it.
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
                            if let Ok(mut journal) = env.journal() {
                                let known = crate::projection::fold(
                                    journal.entries(),
                                    crate::projection::CanvasState::default(),
                                    crate::projection::Clock(deps_now_f64()),
                                )
                                .widgets
                                .contains_key(&dispatch.terminal_id);
                                if known {
                                    let _ = journal.commit(
                                        actor(request),
                                        "widget.remove",
                                        &format!("widget:{}", dispatch.terminal_id),
                                        json!({}),
                                    );
                                }
                            }
                        }
                    }
                    Response::accepted(200, dispatch.version, 0, dispatch_json(&dispatch))
                }
                Err(error) => Response::from_error(&error),
            })
        }

        ("POST", Some("messages"), None, _) => {
            let message_type = request.text("type").unwrap_or("note");
            if !MESSAGE_TYPES.contains(&message_type) {
                return Some(invalid(format!(
                    "type must be one of {}",
                    MESSAGE_TYPES.join(", ")
                )));
            }
            let dispatch_id = request
                .text("dispatchId")
                .map(str::trim)
                .filter(|id| !id.is_empty());
            let dispatch = match dispatch_id {
                Some(id) => match store.require_dispatch(id) {
                    Ok(dispatch) => Some(dispatch.clone()),
                    Err(error) => return Some(Response::from_error(&error)),
                },
                None => None,
            };
            let run_id = match request.text("runId") {
                Some(id) if !id.trim().is_empty() => match store.resolve_run_id(Some(id)) {
                    Ok(id) => id,
                    Err(error) => return Some(Response::from_error(&error)),
                },
                _ => match dispatch.as_ref() {
                    Some(dispatch) => dispatch.run_id.clone(),
                    None => match store.resolve_run_id(None) {
                        Ok(id) => id,
                        Err(error) => return Some(Response::from_error(&error)),
                    },
                },
            };
            if let Some(dispatch) = dispatch.as_ref() {
                if dispatch.run_id != run_id {
                    return Some(invalid(format!(
                        "dispatch \"{}\" belongs to run \"{}\"",
                        dispatch.id, dispatch.run_id
                    )));
                }
                if request
                    .text("taskId")
                    .is_some_and(|task_id| task_id != dispatch.task_id.as_str())
                {
                    return Some(invalid(format!(
                        "dispatch \"{}\" belongs to task \"{}\"",
                        dispatch.id, dispatch.task_id
                    )));
                }
            }
            if let Some(reply_to) = request.text("replyTo") {
                if deps.orchestration.message_by_id(reply_to).is_none() {
                    return Some(not_found(format!("no message \"{reply_to}\" to reply to")));
                }
            }
            let settled = if message_type == "worker_done" {
                let Some(dispatch) = dispatch.as_ref() else {
                    return Some(invalid("worker_done needs --dispatch-id"));
                };
                let outcome = request.text("outcome").unwrap_or("");
                if !OUTCOMES.contains(&outcome) {
                    return Some(invalid(format!(
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
                    Err(error) => return Some(Response::from_error(&error)),
                }
            } else {
                None
            };
            let from = request.agent_id.as_deref().unwrap_or("api");
            // The shell's `resolveRecipient`: empty → @coordinator, the
            // handles pass through, `@<agent>` must name a running dispatch's
            // agent, and a bare worker reference resolves to its terminal id.
            // Without this `slate done` mail landed on `""` and never reached
            // the coordinator it was addressed to.
            let to = {
                let raw = request.text("to").unwrap_or("").trim().to_owned();
                let resolved = if raw.is_empty() {
                    Ok("@coordinator".to_owned())
                } else if matches!(raw.as_str(), "*" | "@all" | "@idle" | "@coordinator") {
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
                    let looked_up =
                        match environment(deps, "/orchestration/messages", "worker lookup") {
                            Ok(env) => env.resolve_terminal(&raw, request.agent_id.as_deref()).ok(),
                            Err(response) => return Some(response),
                        };
                    // A recipient that is not a terminal, a name, or a
                    // prefix still passes through as an actor id — the
                    // store accepts opaque ids (api, cli, …).
                    Ok(looked_up.unwrap_or(raw))
                };
                match resolved {
                    Ok(to) => to,
                    Err(error) => return Some(Response::from_error(&error)),
                }
            };
            let message = match deps.orchestration.send(
                &run_id,
                message_type,
                from,
                &to,
                request.text("subject").unwrap_or(""),
                request.text("body").unwrap_or(""),
                request.text("taskId"),
                dispatch_id,
                request.text("replyTo"),
                request.text("outcome"),
                request.strings("filesModified"),
                request.strings("images"),
                request.strings("options"),
                deps.now,
            ) {
                Ok(message) => message,
                Err(error) => return Some(Response::from_error(&error)),
            };
            let mut data = message_json(&message);
            if let (Some(settled), Value::Object(fields)) = (settled, &mut data) {
                fields.insert("settled".to_owned(), settled);
            }
            Some(Response::accepted(201, 1, 0, data))
        }
        ("POST", Some("messages"), Some(id), Some("ack")) => {
            let actor = request.agent_id.as_deref().unwrap_or("api");
            Some(match store.ack(id, actor) {
                Ok(message) => Response::accepted(200, 1, 0, message_json(&message)),
                Err(error) => Response::from_error(&error),
            })
        }

        ("POST", Some("gates"), None, _) => {
            let run_id = match store.resolve_run_id(request.text("runId")) {
                Ok(id) => id,
                Err(error) => return Some(Response::from_error(&error)),
            };
            let created_by = request.agent_id.as_deref().unwrap_or("api");
            Some(
                match store.create_gate(
                    &run_id,
                    request.text("question").unwrap_or(""),
                    &request.strings("options"),
                    created_by,
                    request.text("taskId"),
                    deps.now,
                ) {
                    Ok(gate) => Response::accepted(201, gate.version, 0, gate_json(&gate)),
                    Err(error) => Response::from_error(&error),
                },
            )
        }
        ("POST", Some("gates"), Some(id), Some("resolve")) => {
            let resolution = request.text("resolution").unwrap_or("");
            Some(match store.resolve_gate(id, resolution, deps.now) {
                Ok(gate) => Response::accepted(200, gate.version, 0, gate_json(&gate)),
                Err(error) => Response::from_error(&error),
            })
        }

        ("POST", Some("reset"), None, _) => {
            // Rebuilt through the store's own document so the private maps and
            // the id counter stay consistent — the store has no narrower
            // reset, and rewriting the JSON is what a fresh load sees.
            let tasks = request.field("tasks") == Some(&Value::Bool(true));
            let messages = request.field("messages") == Some(&Value::Bool(true));
            let all = request.field("all") == Some(&Value::Bool(true));
            if !tasks && !messages && !all {
                return Some(invalid("reset needs tasks, messages or all"));
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
            let message = request
                .text("message")
                .map(str::trim)
                .unwrap_or_default()
                .replace("\r\n", "\n")
                .replace('\r', "\n");
            if message.is_empty() {
                return invalid("a commit message is required");
            }
            let commit = git(deps, &["add", "-A"])
                .and_then(|_| git(deps, &["commit", "-m", &message]))
                .and_then(|_| git(deps, &["rev-parse", "HEAD"]));
            match commit {
                Ok(hash) => Response::json(
                    200,
                    json!({
                        "ok": true,
                        "data": {
                            "hash": hash.trim().chars().take(8).collect::<String>(),
                            "message": message,
                        }
                    }),
                ),
                Err(error) => Response::from_error(&error),
            }
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
            let reference = request.text("ref").unwrap_or("").trim().to_owned();
            if let Err(error) = assert_safe_ref(&reference) {
                return Response::from_error(&error);
            }
            let status = git_status(deps);
            if !status["repo"].as_bool().unwrap_or(false) {
                return Response::from_error(&CommandError::new(
                    ErrorCode::Failed,
                    "not a git repository",
                ));
            }
            if git_dirty(&status) {
                return Response::from_error(&CommandError::new(
                    ErrorCode::Conflict,
                    "uncommitted tracked changes — commit or discard them before switching",
                ));
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
                    Response::json(
                        200,
                        json!({"ok": true, "data": {"branch": branch, "hash": hash}}),
                    )
                }
                Err(error) => Response::from_error(&error),
            }
        }
        ("POST", Some("create-branch")) => {
            let name = request.text("name").unwrap_or("").trim().to_owned();
            if name.is_empty() {
                return invalid("a branch name is required");
            }
            if name.len() > 256 {
                return invalid("branch name is too long");
            }
            if git(deps, &["check-ref-format", "--branch", &name]).is_err() {
                return invalid("invalid branch name");
            }
            let start_point = request.text("startPoint").unwrap_or("").trim().to_owned();
            if !start_point.is_empty() {
                if let Err(error) = assert_safe_ref(&start_point) {
                    return Response::from_error(&error);
                }
            }
            let status = git_status(deps);
            if !status["repo"].as_bool().unwrap_or(false) {
                return Response::from_error(&CommandError::new(
                    ErrorCode::Failed,
                    "not a git repository",
                ));
            }
            if git_dirty(&status) {
                return Response::from_error(&CommandError::new(
                    ErrorCode::Conflict,
                    "uncommitted tracked changes — commit or discard them before switching",
                ));
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
                    Response::json(
                        200,
                        json!({"ok": true, "data": {"branch": name, "hash": hash}}),
                    )
                }
                Err(error) => Response::from_error(&error),
            }
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
/// TypeScript's `listWidgets` projection produced.
fn widget_row(widget: &crate::projection::Widget) -> Value {
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
        // `widget.create` — the payload is journaled verbatim after the
        // id/kind checks the TypeScript handler ran.
        ("POST", None) => {
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            let kind = payload
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_owned);
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
                return invalid("widget id must be 1–128 chars of [A-Za-z0-9._:-]");
            }
            {
                let env = match environment(
                    deps,
                    "POST /widgets",
                    "creating a widget commits to the journal",
                ) {
                    Ok(env) => env,
                    Err(response) => return response,
                };
                let canvas = match folded_canvas(env) {
                    Ok(canvas) => canvas,
                    Err(response) => return response,
                };
                if canvas.widgets.contains_key(&id) {
                    return Response::from_error(&CommandError::new(
                        ErrorCode::Conflict,
                        format!("widget {id} already exists"),
                    ));
                }
            }
            // Same defaulting as `widget.create`'s apply: a kind default size,
            // title from the kind label, z stacking above the top widget.
            if payload.get("id").is_none() {
                payload["id"] = json!(id);
            }
            if payload.get("title").and_then(Value::as_str).is_none() {
                let label = kind
                    .as_deref()
                    .map(|kind| {
                        let mut chars = kind.chars();
                        chars
                            .next()
                            .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                            .unwrap_or_else(|| kind.to_owned())
                    })
                    .unwrap_or_else(|| id.clone());
                payload["title"] = json!(label);
            }
            // `widgetType(kind)?.defaultSize ?? 520×360` — the registry's
            // five entries; every other kind falls through to the fallback.
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
            // The TypeScript target was `widget:new` — the reducer takes the
            // id from the payload then, which is what we just defaulted.
            match journal_commit(
                deps,
                request,
                "widget.create",
                "widget:new",
                payload,
                "POST /widgets",
            ) {
                Ok(entry) => {
                    let env = match environment(
                        deps,
                        "POST /widgets",
                        "reading the new widget refolds the journal",
                    ) {
                        Ok(env) => env,
                        Err(_) => return accepted(&entry, 201, json!({ "id": id })),
                    };
                    let canvas = folded_canvas(env).unwrap_or_default();
                    let widget = canvas
                        .widgets
                        .get(&id)
                        .map(widget_row)
                        .unwrap_or_else(|| json!({ "id": id }));
                    accepted(&entry, 201, widget)
                }
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
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            let clean_title = payload
                .get("title")
                .and_then(Value::as_str)
                .map(|title| title.trim().chars().take(200).collect::<String>())
                .filter(|title| !title.is_empty());
            match &clean_title {
                Some(title) => payload["title"] = json!(title),
                None => {
                    if let Some(map) = payload.as_object_mut() {
                        map.remove("title");
                    }
                }
            }
            let env = match environment(
                deps,
                "PATCH /widgets/{id}",
                "updating a widget commits to the journal",
            ) {
                Ok(env) => env,
                Err(response) => return response,
            };
            let canvas = match folded_canvas(env) {
                Ok(canvas) => canvas,
                Err(response) => return response,
            };
            let Some(existing) = canvas.widgets.get(raw) else {
                return not_found(format!("widget {raw} not found"));
            };
            let is_terminal = existing.kind.as_deref() == Some("terminal");
            let entry = match journal_commit(
                deps,
                request,
                "widget.update",
                &format!("widget:{raw}"),
                payload,
                "PATCH /widgets/{id}",
            ) {
                Ok(entry) => entry,
                Err(response) => return response,
            };
            // `terminals.setTitle` side effect: renaming a shell's widget
            // renames the shell too.
            if is_terminal {
                if let (Some(title), Some(env)) = (clean_title, deps.env.as_deref_mut()) {
                    env.set_terminal_title(raw, &title);
                }
            }
            let env = match environment(
                deps,
                "PATCH /widgets/{id}",
                "reading the updated widget refolds the journal",
            ) {
                Ok(env) => env,
                Err(_) => return accepted(&entry, 200, json!({ "id": raw })),
            };
            let canvas = folded_canvas(env).unwrap_or_default();
            let widget = canvas
                .widgets
                .get(raw)
                .map(widget_row)
                .unwrap_or_else(|| json!({ "id": raw }));
            accepted(&entry, 200, widget)
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
                match environment(
                    deps,
                    "DELETE /widgets/{id}",
                    "disposing a shell needs the PTY table",
                ) {
                    Ok(env) => {
                        if let Err(message) = env.dispose_terminal(raw) {
                            return Response::from_error(&CommandError::new(
                                ErrorCode::Failed,
                                message,
                            ));
                        }
                    }
                    Err(response) => return response,
                }
                // `failTerminalDispatches` — settle anything still running on
                // the dead shell, exactly as the explicit route does.
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
                // The TypeScript journaled `terminal.dispose` first; the
                // widget.remove that follows retires the tile the shell sat
                // in, when the journal knew one.
                match journal_commit(
                    deps,
                    request,
                    "terminal.dispose",
                    &format!("terminal:{raw}"),
                    json!({}),
                    "DELETE /widgets/{id}",
                ) {
                    Ok(entry) => {
                        let on_canvas = environment(
                            deps,
                            "DELETE /widgets/{id}",
                            "reading the canvas refolds the journal",
                        )
                        .ok()
                        .and_then(|env| folded_canvas(env).ok())
                        .is_some_and(|canvas| canvas.widgets.contains_key(raw));
                        if on_canvas {
                            let _ = journal_commit(
                                deps,
                                request,
                                "widget.remove",
                                &format!("widget:{raw}"),
                                json!({}),
                                "DELETE /widgets/{id}",
                            );
                        }
                        return accepted(&entry, 200, json!({ "id": raw }));
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
            if !on_canvas {
                return not_found(format!("widget {raw} not found"));
            }
            match journal_commit(
                deps,
                request,
                "widget.remove",
                &format!("widget:{raw}"),
                json!({}),
                "DELETE /widgets/{id}",
            ) {
                Ok(entry) => accepted(&entry, 200, json!({ "id": raw })),
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
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            if let Some(map) = payload.as_object_mut() {
                map.remove("agentId");
            }
            match journal_commit(
                deps,
                request,
                "canvas.camera",
                "canvas:main",
                payload,
                "POST /canvas/camera",
            ) {
                Ok(entry) => {
                    let camera = environment(
                        deps,
                        "POST /canvas/camera",
                        "reading the camera refolds the journal",
                    )
                    .ok()
                    .and_then(|env| folded_canvas(env).ok())
                    .map(|canvas| {
                        json!({
                            "x": canvas.camera.x,
                            "y": canvas.camera.y,
                            "zoom": canvas.camera.zoom,
                        })
                    })
                    .unwrap_or_default();
                    accepted(&entry, 200, json!({ "camera": camera }))
                }
                Err(response) => response,
            }
        }
        ("POST", Some("strokes")) => {
            let payload =
                json!({ "strokes": request.body.get("strokes").cloned().unwrap_or(Value::Null) });
            match journal_commit(
                deps,
                request,
                "canvas.strokes",
                "canvas:main",
                payload,
                "POST /canvas/strokes",
            ) {
                Ok(entry) => {
                    let count = folded_canvas(
                        match environment(
                            deps,
                            "POST /canvas/strokes",
                            "counting strokes refolds the journal",
                        ) {
                            Ok(env) => env,
                            Err(_) => return accepted(&entry, 200, json!({ "count": 0 })),
                        },
                    )
                    .map(|canvas| canvas.strokes.len())
                    .unwrap_or(0);
                    accepted(&entry, 200, json!({ "count": count }))
                }
                Err(response) => response,
            }
        }
        ("POST", Some("connections")) => {
            let payload = json!({ "connections": request.body.get("connections").cloned().unwrap_or(Value::Null) });
            match journal_commit(
                deps,
                request,
                "canvas.connections",
                "canvas:main",
                payload,
                "POST /canvas/connections",
            ) {
                Ok(entry) => {
                    let count = folded_canvas(
                        match environment(
                            deps,
                            "POST /canvas/connections",
                            "counting connections refolds the journal",
                        ) {
                            Ok(env) => env,
                            Err(_) => return accepted(&entry, 200, json!({ "count": 0 })),
                        },
                    )
                    .map(|canvas| canvas.connections.len())
                    .unwrap_or(0);
                    accepted(&entry, 200, json!({ "count": count }))
                }
                Err(response) => response,
            }
        }
        ("POST", Some("import")) => {
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            if let Some(map) = payload.as_object_mut() {
                map.remove("agentId");
            }
            match journal_commit(
                deps,
                request,
                "canvas.import",
                "canvas:main",
                payload,
                "POST /canvas/import",
            ) {
                Ok(entry) => accepted(&entry, 200, json!({ "applied": true })),
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
            let id = format!(
                "{widget_kind}-{}-{}",
                deps.now,
                &uuid::Uuid::new_v4().simple().to_string()[..5]
            );
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
            let mut payload = json!({
                "id": id, "title": title, "kind": widget_kind,
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
            match journal_commit(deps, request, "widget.create", "widget:new", payload, route) {
                Ok(entry) => accepted(
                    &entry,
                    201,
                    json!({
                        "id": id,
                        "title": title,
                        "path": imported.to_string_lossy(),
                        "name": name,
                        "kind": kind,
                        "widgetKind": widget_kind,
                    }),
                ),
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
    fn plan_items<'e>(
        env: &mut (dyn RouteEnv + 'e),
    ) -> Result<Vec<crate::planner::PlanItem>, Response> {
        let log = env.journal().map_err(|message| {
            Response::from_error(&CommandError::new(ErrorCode::Failed, message))
        })?;
        let items = crate::planner::fold(
            log.entries(),
            crate::planner::PlannerState::default(),
            &crate::planner::today_local(),
        )
        .map_err(|invalid| {
            Response::from_error(&CommandError::new(ErrorCode::Failed, invalid.0))
        })?;
        let mut items: Vec<_> = items.values().cloned().collect();
        items.sort_by(|a, b| {
            a.order
                .partial_cmp(&b.order)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(items)
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
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            if let Some(map) = payload.as_object_mut() {
                map.remove("agentId");
            }
            // `createItem` threw `invalid` before the commit ever landed.
            let title = payload
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if title.is_empty() {
                return invalid("title is required");
            }
            // `normalizeDay` — a malformed day aborts the request rather than
            // entering the journal.
            if let Some(day) = payload.get("day") {
                if !day.is_null() {
                    if let Err(invalid_day) =
                        crate::planner::normalize_day(Some(day), &crate::planner::today_local())
                    {
                        return invalid(invalid_day.0);
                    }
                }
            }
            // `createItem` stamped `order: nextOrder(day)` — max order among
            // same-day items + 1 — so new items sort after their day-mates.
            {
                let env = match environment(
                    deps,
                    "POST /planner",
                    "the planner folds the journal",
                ) {
                    Ok(env) => env,
                    Err(response) => return response,
                };
                if let Ok(items) = plan_items(env) {
                    let day = crate::planner::safe_day(
                        payload.get("day"),
                        &crate::planner::today_local(),
                    );
                    payload["order"] =
                        json!(crate::planner::next_order(items.iter(), day.as_deref()));
                }
            }
            // The TypeScript target was `plan:new` and the reducer minted the
            // id from the entry's stamp — keeping that means the committed
            // row is the id the fold will know it by.
            let requested_id = payload.get("id").and_then(Value::as_str).map(str::to_owned);
            match journal_commit(
                deps,
                request,
                "plan.create",
                "plan:new",
                payload,
                "POST /planner",
            ) {
                Ok(entry) => {
                    let env = match environment(
                        deps,
                        "POST /planner",
                        "reading the new item refolds the journal",
                    ) {
                        Ok(env) => env,
                        Err(_) => return accepted(&entry, 201, json!({})),
                    };
                    let id =
                        requested_id.unwrap_or_else(|| format!("plan-{}-{}", entry.at, entry.seq));
                    let item = plan_items(env)
                        .ok()
                        .and_then(|items| items.into_iter().find(|item| item.id == id))
                        .map(|item| serde_json::to_value(item).unwrap_or_default())
                        .unwrap_or_else(|| json!({ "id": id }));
                    accepted(&entry, 201, item)
                }
                Err(response) => response,
            }
        }
        ("POST", Some(raw), Some("toggle")) => {
            let env = match environment(
                deps,
                "POST /planner/{id}/toggle",
                "the planner folds the journal",
            ) {
                Ok(env) => env,
                Err(response) => return response,
            };
            if !match plan_items(env) {
                Ok(items) => items.iter().any(|item| item.id == *raw),
                Err(_) => false,
            } {
                return not_found("plan item not found");
            }
            let payload = match request.body.get("done") {
                Some(Value::Bool(done)) => json!({ "done": done }),
                _ => json!({}),
            };
            match journal_commit(
                deps,
                request,
                "plan.toggle",
                &format!("plan:{raw}"),
                payload,
                "POST /planner/{id}/toggle",
            ) {
                Ok(entry) => {
                    let env = match environment(
                        deps,
                        "POST /planner/{id}/toggle",
                        "reading the item refolds the journal",
                    ) {
                        Ok(env) => env,
                        Err(_) => return accepted(&entry, 200, json!({ "id": raw })),
                    };
                    let item = plan_items(env)
                        .ok()
                        .and_then(|items| items.into_iter().find(|item| item.id == *raw))
                        .map(|item| serde_json::to_value(item).unwrap_or_default())
                        .unwrap_or_else(|| json!({ "id": raw }));
                    accepted(&entry, 200, item)
                }
                Err(response) => response,
            }
        }
        ("PATCH", Some(raw), _) => {
            let env =
                match environment(deps, "PATCH /planner/{id}", "the planner folds the journal") {
                    Ok(env) => env,
                    Err(response) => return response,
                };
            if !match plan_items(env) {
                Ok(items) => items.iter().any(|item| item.id == *raw),
                Err(_) => false,
            } {
                return not_found("plan item not found");
            }
            let mut payload = request.body.clone();
            if !payload.is_object() {
                payload = json!({});
            }
            if let Some(map) = payload.as_object_mut() {
                map.remove("agentId");
            }
            // `plan.update`'s day is the throwing validator — reject before
            // the commit so a bad day never enters the journal.
            if let Some(day) = payload.get("day") {
                if !day.is_null() {
                    if let Err(invalid_day) =
                        crate::planner::normalize_day(Some(day), &crate::planner::today_local())
                    {
                        return invalid(invalid_day.0);
                    }
                }
            }
            match journal_commit(
                deps,
                request,
                "plan.update",
                &format!("plan:{raw}"),
                payload,
                "PATCH /planner/{id}",
            ) {
                Ok(entry) => {
                    let env = match environment(
                        deps,
                        "PATCH /planner/{id}",
                        "reading the item refolds the journal",
                    ) {
                        Ok(env) => env,
                        Err(_) => return accepted(&entry, 200, json!({ "id": raw })),
                    };
                    let item = plan_items(env)
                        .ok()
                        .and_then(|items| items.into_iter().find(|item| item.id == *raw))
                        .map(|item| serde_json::to_value(item).unwrap_or_default())
                        .unwrap_or_else(|| json!({ "id": raw }));
                    accepted(&entry, 200, item)
                }
                Err(response) => response,
            }
        }
        ("DELETE", Some(raw), _) => {
            match journal_commit(
                deps,
                request,
                "plan.delete",
                &format!("plan:{raw}"),
                json!({}),
                "DELETE /planner/{id}",
            ) {
                Ok(entry) => accepted(&entry, 200, json!({ "id": raw })),
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
    // store write was the apply. The journal entry is the audit; the file is
    // the projection the notes pane reads — both happen here, in that order.
    let mut payload = request.body.clone();
    if !payload.is_object() {
        payload = json!({});
    }
    if let Some(map) = payload.as_object_mut() {
        map.remove("agentId");
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
            let title = payload
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>();
            if title.is_empty() {
                return invalid("title is required");
            }
            let entry = match journal_commit(
                deps,
                request,
                "note.create",
                "note:new",
                payload.clone(),
                "POST /notes",
            ) {
                Ok(entry) => entry,
                Err(response) => return response,
            };
            let mut document = notes_document(&data_dir);
            let items = notes_items(&document);
            let order = items
                .iter()
                .filter_map(|item| item.get("order").and_then(Value::as_f64))
                .fold(0.0_f64, f64::max)
                + 1.0;
            let id = payload
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
            let category = normalize_category(payload.get("category"));
            let color = note_color_for(
                &document,
                category.as_deref(),
                normalize_color(payload.get("color")),
            );
            let item = json!({
                "id": id,
                "title": title,
                "body": payload.get("body").and_then(Value::as_str).unwrap_or("").chars().take(20_000).collect::<String>(),
                "tags": normalize_tags(payload.get("tags")),
                "category": category,
                "color": color,
                "createdBy": actor(request),
                "order": order,
                "createdAt": now,
                "updatedAt": now,
                "version": 1,
            });
            let mut items = items;
            items.push(item.clone());
            document["items"] = Value::Array(items);
            if let Err(error) = notes_write(&data_dir, &document) {
                return Response::from_error(&error);
            }
            accepted(&entry, 201, item)
        }
        ("POST", Some("recolor"), None) => {
            let category = normalize_category(payload.get("category"));
            let color = normalize_color(payload.get("color"));
            let (Some(category), Some(color)) = (category, color) else {
                return invalid("category and a #rrggbb color are required");
            };
            let entry = match journal_commit(
                deps,
                request,
                "note.recolor",
                "note:new",
                payload.clone(),
                "POST /notes/recolor",
            ) {
                Ok(entry) => entry,
                Err(response) => return response,
            };
            let mut document = notes_document(&data_dir);
            if !document["categoryColors"].is_object() {
                document["categoryColors"] = json!({});
            }
            if let Some(map) = document["categoryColors"].as_object_mut() {
                map.insert(category.clone(), json!(color.clone()));
            }
            let mut items = notes_items(&document);
            for item in &mut items {
                if item.get("category").and_then(Value::as_str) == Some(category.as_str()) {
                    item["color"] = json!(&color);
                    item["updatedAt"] = json!(now);
                    item["version"] =
                        json!(item.get("version").and_then(Value::as_f64).unwrap_or(1.0) + 1.0);
                }
            }
            document["items"] = Value::Array(items);
            match notes_write(&data_dir, &document) {
                Ok(()) => accepted(&entry, 200, json!({ "ok": true })),
                Err(error) => Response::from_error(&error),
            }
        }
        ("PATCH", Some(raw), _) => {
            let mut document = notes_document(&data_dir);
            let mut items = notes_items(&document);
            let Some(index) = items
                .iter()
                .position(|item| item.get("id").and_then(Value::as_str) == Some(raw))
            else {
                return not_found("note not found");
            };
            let entry = match journal_commit(
                deps,
                request,
                "note.update",
                &format!("note:{raw}"),
                payload.clone(),
                "PATCH /notes/{id}",
            ) {
                Ok(entry) => entry,
                Err(response) => return response,
            };
            {
                let body = &payload;
                let item = &mut items[index];
                if let Some(title) = body.get("title").and_then(Value::as_str) {
                    let title: String = title.trim().chars().take(200).collect();
                    if !title.is_empty() {
                        item["title"] = json!(title);
                    }
                }
                if let Some(text) = body.get("body").and_then(Value::as_str) {
                    item["body"] = json!(text.chars().take(20_000).collect::<String>());
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
                            // An explicit color joins the category's shared
                            // palette entry, as the TypeScript store did.
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
            if let Err(error) = notes_write(&data_dir, &document) {
                return Response::from_error(&error);
            }
            accepted(&entry, 200, updated)
        }
        ("DELETE", Some(raw), _) => {
            let exists = notes_items(&notes_document(&data_dir))
                .iter()
                .any(|item| item.get("id").and_then(Value::as_str) == Some(raw));
            if !exists {
                return not_found("note not found");
            }
            let entry = match journal_commit(
                deps,
                request,
                "note.delete",
                &format!("note:{raw}"),
                json!({}),
                "DELETE /notes/{id}",
            ) {
                Ok(entry) => entry,
                Err(response) => return response,
            };
            let mut document = notes_document(&data_dir);
            let mut items = notes_items(&document);
            items.retain(|item| item.get("id").and_then(Value::as_str) != Some(raw));
            document["items"] = Value::Array(items);
            match notes_write(&data_dir, &document) {
                Ok(()) => accepted(&entry, 200, json!({ "id": raw })),
                Err(error) => Response::from_error(&error),
            }
        }
        _ => not_found("no such notes route"),
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
    journal_commit(
        deps,
        request,
        "widget.update",
        &format!("widget:{id}"),
        json!({ "state": state }),
        "/browser",
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
    if let Err(response) = journal_commit(
        deps,
        request,
        "widget.create",
        "widget:new",
        payload,
        "POST /browser/open",
    ) {
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
/// in the TypeScript. There is no registry here; this is the fixed list of
/// journal events and store operations the server accepts.
const COMMAND_TYPES: &[&str] = &[
    "widget.create",
    "widget.update",
    "widget.remove",
    "canvas.camera",
    "canvas.strokes",
    "canvas.connections",
    "canvas.import",
    "terminal.create",
    "terminal.spawn",
    "terminal.write",
    "terminal.attach",
    "terminal.dispose",
    "plan.create",
    "plan.update",
    "plan.toggle",
    "plan.delete",
    "note.create",
    "note.update",
    "note.recolor",
    "note.delete",
    "git.refresh",
    "git.commit",
    "git.branches",
    "git.log",
    "git.checkout",
    "git.create-branch",
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
