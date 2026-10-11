use serde_json::{json, Value};
use std::{
    env,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn prog_name() -> String {
    env::args()
        .next()
        .and_then(|p| {
            std::path::Path::new(&p)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "slate".to_string())
}

fn usage(prog: &str) -> String {
    format!(
        "{prog} — Slate agent CLI

THE OTHER AGENTS
  {prog} whoami | context | workers | tell <worker> \"msg\" | worker-read <id> | rename --to <id> --name <n>
  {prog} status | doctor | version | help <command>

RUNS, TASKS & MAIL
  {prog} run-create --objective \"...\" | run-list | run-show <id> | run-close <id> --yes
  {prog} task-create <spec> [--title \"...\"] [--deps a,b] | task-list [--ready] | task-show <id> | task-update <id>
  {prog} worker-start <task> [--agent <a>] [--terminal <id>] [--no-inject] | dispatch <task> --to <term>
  {prog} worker-show <dispatch> [--preamble] | dispatch-show [--task <id>] | worker-release|retain <dispatch>
  {prog} check [--wait] [--types ...] [--ack <msg>] | ask --question \"...\" | reply <ask-id> <body>
  {prog} send --type <t> [--to <who>] | done --outcome succeeded|failed | escalate --body \"...\" | heartbeat | ack <msg-id>
  {prog} gate-create --question \"...\" | gate-list | gate-resolve <id> <resolution> | allow <id> | deny <id>

THE APP & CANVAS
  {prog} plan list|create|toggle|done|update|delete [<id>]
  {prog} canvas list|widgets | place <kind> [--title --x --y] | image <path> | move <id> --x --y
         rename|title <id> --name <n> | focus <id> | close <id> --yes
  {prog} terminal list|open|send <id> <text>|read <id>|rename <id> <name>|close <id> --yes
  {prog} git status | git commit --message \"...\"
  {prog} journal tail [--lines n] [--since seq] | journal types
  {prog} reset --tasks|--messages|--all --yes     {prog} screenshot [worker] [--out <file>]
  {prog} api <METHOD> <path> [json-body]          raw request escape hatch
  {prog} browser open [url] | list | navigate | snapshot | click | fill | select | press | scroll

Destructive commands (reset, run-close, canvas close, terminal close,
plan delete) require --yes to confirm.

Add --json to any command for machine-readable output."
    )
}

/// `slate help <command>` — one line per command, the same surface as the
/// COMMAND_HELP table in slate.mjs.
fn command_help(name: &str) -> Option<&'static str> {
    let canon = match name {
        "ctx" => "context",
        "st" => "status",
        "ps" | "who" => "workers",
        "runs" => "run-list",
        "run" => "run-show",
        "tasks" => "task-list",
        "task" => "task-show",
        "logs" | "tail" => "worker-read",
        "inbox" => "check",
        "gates" => "gate-list",
        "mail" | "msg" => "send",
        "approve" | "permit" => "allow",
        "reject" | "refuse" => "deny",
        "shot" => "screenshot",
        "term" => "terminal",
        other => other,
    };
    Some(match canon {
        "whoami" => "slate whoami — identify your own agent, terminal & task.",
        "context" => {
            "slate context | slate ctx — project folder, Code Workspace and active task."
        }
        "workers" => {
            "slate workers | slate ps | slate who — roster: * marks you, name (id), busy/idle (+exited), [agent], task, cwd."
        }
        "version" => "slate version | slate --version — print the CLI version.",
        "status" => "slate status | slate st — open run, tasks, live workers, unread mail.",
        "run-create" => "slate run-create --objective \"...\" — open a run.",
        "run-list" => "slate run-list | slate runs — list all runs, newest first.",
        "run-show" => "slate run-show [<id>] | slate run [<id>] — inspect a run and its tasks.",
        "run-close" => "slate run-close [<id>] --yes — close a run (destructive: needs --yes).",
        "task-create" => {
            "slate task-create [<spec>] [--title \"...\"] [--deps '[\"otask-1\"]'] [--run <id>] — file a task."
        }
        "task-list" => "slate task-list | slate tasks [--ready] [--run <id>] [--status <s>] — list tasks.",
        "task-show" => "slate task-show [<id>] | slate task [<id>] — full task specification.",
        "task-update" => {
            "slate task-update [<id>] [--status <s>] [--title \"...\"] [--spec \"...\"] — update a task."
        }
        "worker-start" => {
            "slate worker-start [<taskId>] [--agent <a>] [--terminal <id>] [--command \"...\"] [--no-inject] — dispatch work."
        }
        "dispatch" => "slate dispatch [<taskId>] --to <terminalId> — dispatch into an existing terminal.",
        "worker-show" => "slate worker-show [<dispatchId>] [--preamble] — dispatch details.",
        "dispatch-show" | "dispatches" => {
            "slate dispatch-show [--task <taskId>] — list dispatches."
        }
        "worker-release" => {
            "slate worker-release [<dispatchId>] [--close] — release a worker, optionally closing its terminal."
        }
        "worker-retain" => "slate worker-retain [<dispatchId>] — keep a worker for subsequent tasks.",
        "agents" => {
            "slate agents — detected agent per terminal and its state (working/blocked/idle/unknown)."
        }
        "wait" => {
            "slate wait <terminal|name> [--for <state>] [--timeout <s>] — block until an agent reaches a state (default blocked)."
        }
        "worker-read" => {
            "slate worker-read [<dispatchId|terminalId|name>] [--limit N] | slate logs [<id>] [limit] — tail worker output."
        }
        "tell" => {
            "slate tell <worker> \"run the tests\" [--when <state>] [--image <file>] — deliver text (optionally only once the worker reaches a state); use `slate worker-read <worker>` for the answer."
        }
        "prune" => "slate prune — dispose exited terminals still holding slots.",
        "rename" => "slate rename [<worker>] [--name <name>] — rename a terminal.",
        "check" => {
            "slate check | slate inbox [--wait] [--types ...] [--ack <msgId>] [--all] — read coordinator mail."
        }
        "reply" => "slate reply [<askId>] [<bodyText>] — answer a worker question.",
        "ask" => {
            "slate ask --question \"...\" [--options \"a,b\"] [--type permission] — ask and block until a reply arrives."
        }
        "done" => {
            "slate done --outcome succeeded|failed [--task-id <t>] [--dispatch-id <d>] [--body \"...\"] [--files \"a.ts,b.ts\"] — report completion."
        }
        "send" => "slate send --type <t> [--to <who>] [--subject \"...\"] [--body \"...\"] — direct message or broadcast.",
        "escalate" => "slate escalate --body \"...\" — escalate an issue to the coordinator.",
        "heartbeat" => "slate heartbeat — keep worker activity alive.",
        "ack" => "slate ack <messageId> — acknowledge a message after reading it.",
        "allow" => "slate allow <id> [--note \"...\"] — approve a permission request.",
        "deny" => "slate deny <id> [--reason \"...\"] — deny a permission request.",
        "gate-create" => {
            "slate gate-create --question \"...\" [--task <t>] [--options '[\"a\",\"b\"]'] — open a decision gate."
        }
        "gate-list" => "slate gate-list | slate gates [--run <id>] [--open] — list decision gates.",
        "gate-resolve" => "slate gate-resolve [<gateId>] <resolution> — resolve a gate and unblock its task.",
        "reset" => "slate reset [--tasks] [--messages] [--all] --yes — destructive reset of orchestration state.",
        "doctor" => "slate doctor — diagnostics & connectivity check.",
        "terminal" => {
            "slate terminal list|open|send <id> <text>|read <id>|rename <id> <name>|close <id> --yes — direct terminal management."
        }
        "canvas" | "pane" | "panes" | "widget" | "widgets" => {
            "slate canvas list | place <kind> | image <path> | move | rename | close — pane widgets (alias: pane)."
        }
        "browser" => "slate browser open [url] | list | navigate <id> <url> | snapshot <id> | click <id> <ref> | fill <id> <ref> --value \"...\" | select <id> <ref> --value \"...\" | press <id> <key> [--ref N] | scroll <id> [--pixels N]. Detected checkout and purchase flows are blocked, along with password and payment fields.",
        "plan" => {
            "slate plan list | create | update | done | toggle | delete [<id>] — planner day tasks."
        }
        "git" => "slate git status | commit --message \"...\" — git audit integration.",
        "journal" => "slate journal tail [--lines n] [--since seq] | slate journal types — event audit log.",
        "screenshot" => "slate screenshot | slate shot [<worker>] [--out <file>] — capture the Slate window to a PNG.",
        "api" => "slate api <METHOD> <path> [json] — direct REST escape hatch.",
        _ => return None,
    })
}

pub fn run() -> Result<(), String> {
    let argv: Vec<String> = env::args().skip(1).collect();
    let parsed = slate_app::cli::Args::parse(&argv);
    // The command is the first *positional*, so `slate --json status` works —
    // leading flags are not the verb.
    let command = parsed
        .positionals()
        .first()
        .cloned()
        .unwrap_or_else(|| "help".to_owned());
    if parsed.flag("version") || matches!(command.as_str(), "version" | "--version" | "-V") {
        // slate.mjs emitted {version} through the same --json channel.
        if parsed.flag("json") {
            println!("{}", json!({ "version": env!("CARGO_PKG_VERSION") }));
        } else {
            println!("{} {}", prog_name(), env!("CARGO_PKG_VERSION"));
        }
        return Ok(());
    }
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        // `slate help <topic>` answers for that command alone, like slate.mjs.
        if let Some(topic) = parsed.positionals().get(1) {
            return match command_help(topic) {
                Some(detail) => {
                    println!("{detail}");
                    Ok(())
                }
                None => Err(format!(
                    "unknown_command: no help for \"{topic}\"{} — run `{} --help`",
                    slate_app::cli::suggest_command(topic)
                        .map(|hint| format!(" — did you mean \"{hint}\"?"))
                        .unwrap_or_default(),
                    prog_name()
                )),
            };
        }
        println!("{}", usage(&prog_name()));
        return Ok(());
    }
    // `slate <command> --help` prints that command's line rather than running
    // it — the same early exit as slate.mjs.
    if parsed.flag("help") {
        match command_help(&command) {
            Some(detail) => println!("{detail}"),
            None => println!("{}", usage(&prog_name())),
        }
        return Ok(());
    }
    if command == "whoami" {
        return run_whoami(&parsed);
    }
    if command == "browser" {
        return run_browser(&parsed);
    }
    // Local commands: the journal is a file and git is a subprocess — none of
    // these need the control socket, so they work with the app closed too.
    if command == "plan" {
        // The planner lives in the command journal, which is local file IO —
        // no control socket needed, and the canvas pane re-reads the journal
        // on its next refresh.
        return run_plan(&parsed);
    }
    if command == "journal" {
        return run_journal(&parsed);
    }
    if matches!(
        command.as_str(),
        "canvas" | "pane" | "panes" | "widget" | "widgets"
    ) {
        return run_canvas(&parsed);
    }
    if command == "git" {
        return run_git(&parsed);
    }
    let (url, token) = control()?;

    match command.as_str() {
        "workers" | "who" | "ps" => {
            let response = request(&url, &token, "GET", "/orchestration/workers", None)?;
            println!(
                "{}",
                if parsed.flag("json") {
                    serde_json::to_string_pretty(data_of(&response))
                        .unwrap_or_else(|_| response.to_string())
                } else {
                    format_workers(&response)
                }
            );
        }
        "tell" => {
            let body = tell_body(&parsed)?;
            let to = body["to"].as_str().unwrap();
            // `--when <state>` — don't interrupt a working agent: poll until
            // the target's detected state matches, then deliver. herdr's
            // "send only once it is genuinely blocked" discipline.
            // A `--when` delivery waits out a transient lock too: the gate
            // refuses `terminal:{id}` while another actor's write apply is
            // still in flight (~200ms), and a "deliver once ready" tell
            // should not flake on that window.
            let locked_deadline = parsed
                .pick(&["when", "for"])
                .map(|_| std::time::Instant::now() + std::time::Duration::from_secs(5));
            if let Some(when) = parsed.pick(&["when", "for"]) {
                if slate_app::agent_state::AgentState::parse(&when).is_none() {
                    return Err(format!(
                        "unknown state \"{when}\" — working|blocked|idle|unknown"
                    ));
                }
                let timeout = parsed_timeout_secs(&parsed).unwrap_or(300);
                let verdict = wait_for_agent_state(&url, &token, to, &when, timeout)?;
                eprintln!("{verdict} — delivering");
            }
            let response = loop {
                match request(
                    &url,
                    &token,
                    "POST",
                    "/orchestration/workers/tell",
                    Some(body.clone()),
                ) {
                    Err(error)
                        if error.contains("is locked by")
                            && locked_deadline.is_some_and(|d| std::time::Instant::now() < d) =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    result => break result?,
                }
            };
            let delivery_id = data_of(&response)
                .get("delivery")
                .and_then(|delivery| delivery.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("confirmed");
            if parsed.flag("json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(data_of(&response)).map_err(|e| e.to_string())?
                );
            } else {
                // The trailing hint is from slate.mjs: the answer to a tell
                // lives in that terminal's scrollback, not the inbox.
                match data_of(&response).get("mode").and_then(Value::as_str) {
                    Some(mode) => {
                        let count = data_of(&response)
                            .get("images")
                            .and_then(Value::as_array)
                            .map_or(1, |images| images.len().max(1));
                        println!("{count} image(s) attached to {to} via {mode}");
                    }
                    None => println!("delivered to {to} ({delivery_id})"),
                }
                println!("read the answer with: {} worker-read {to}", prog_name());
            }
        }
        "agents" => {
            // `herdr agent list` — the manifest detector's fleet roster.
            let response = request(&url, &token, "GET", "/orchestration/agents", None)?;
            if parsed.flag("json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(data_of(&response))
                        .unwrap_or_else(|_| response.to_string())
                );
            } else {
                println!("{}", format_agents(&response));
            }
        }
        "prune" => {
            // `slate prune` — dispose terminals whose child exited but whose
            // record still holds a slot against MAX_TERMINALS. The spawn path
            // reclaims one when it must; this sweeps them all on demand.
            let response = request(&url, &token, "GET", "/orchestration/workers", None)?;
            let dead: Vec<String> = data_of(&response)
                .get("workers")
                .and_then(Value::as_array)
                .map(|workers| {
                    workers
                        .iter()
                        .filter(|w| w.get("alive").and_then(Value::as_bool) == Some(false))
                        .filter_map(|w| w.get("id").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let mut pruned = 0usize;
            for id in &dead {
                if request(&url, &token, "DELETE", &format!("/terminal/{id}"), None).is_ok() {
                    pruned += 1;
                }
            }
            println!("pruned {pruned} terminal(s)");
        }
        "wait" => {
            // `slate wait <terminal|name> [--for working|blocked|idle]
            // [--timeout <s>]` — herdr's "wait until another agent is
            // genuinely blocked" gate for agent-on-agent orchestration.
            let target = parsed.positionals().get(1).cloned().ok_or_else(|| {
                "usage: slate wait <terminal|name> [--for <state>] [--timeout <s>]".to_owned()
            })?;
            let want = parsed
                .pick(&["for", "state"])
                .unwrap_or_else(|| "blocked".to_owned());
            if slate_app::agent_state::AgentState::parse(&want).is_none() {
                return Err(format!(
                    "unknown state \"{want}\" — working|blocked|idle|unknown"
                ));
            }
            let timeout = parsed
                .pick(&["timeout", "timeoutMs"])
                .map(|v| {
                    // --timeoutMs is milliseconds; --timeout seconds.
                    if parsed.pick(&["timeoutMs"]).is_some() {
                        (v.parse::<f64>().unwrap_or(300_000.0) / 1000.0) as u64
                    } else {
                        v.parse::<u64>().unwrap_or(300)
                    }
                })
                .unwrap_or(300);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                let response = request(&url, &token, "GET", "/orchestration/agents", None)?;
                let agents = data_of(&response)
                    .get("agents")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let row = agents.iter().find(|a| {
                    a.get("id").and_then(Value::as_str) == Some(target.as_str())
                        || a.get("name").and_then(Value::as_str) == Some(target.as_str())
                });
                let Some(row) = row else {
                    return Err(format!("no terminal called \"{target}\""));
                };
                let state = row
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                if parsed.flag("json") {
                    println!("{}", serde_json::to_string(row).unwrap_or_default());
                }
                if state == want {
                    let name = row.get("name").and_then(Value::as_str).unwrap_or(&target);
                    println!("{name} is {state}");
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    let name = row.get("name").and_then(Value::as_str).unwrap_or(&target);
                    return Err(format!("timed out waiting for {name} — still {state}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(700));
            }
        }
        "worker-read" | "logs" | "tail" => run_worker_read(&url, &token, &parsed)?,
        "context" | "ctx" => run_context(&url, &token, &parsed)?,
        "doctor" => run_doctor(&url, &token, &parsed)?,
        "status" | "st" => run_status(&url, &token, &parsed)?,
        "run-show" | "run" => run_run_show(&url, &token, &parsed)?,
        "check" | "inbox" => run_check(&url, &token, &parsed)?,
        "ask" => run_ask(&url, &token, &parsed)?,
        "screenshot" | "shot" => run_screenshot(&url, &token, &parsed)?,
        other => {
            let mut plan = slate_app::cli::plan(other, &parsed).map_err(|e| e.to_string())?;
            // `worker-start`/`dispatch` leave `terminalId` unset when the
            // caller did not name one — the dispatches route reserves a fresh
            // terminal itself, launches the agent command in it, sniffs for a
            // launch failure, and injects the preamble (the shell's
            // `dispatch.start` flow). Pre-spawning here would silently take
            // the existing-terminal path and skip the agent launch.
            // `reply`/`allow`/`deny` address the asker of the message being
            // answered; when --to is absent the runner resolves it from the
            // question, the lookupSender step in slate.mjs.
            if matches!(
                other,
                "reply" | "allow" | "approve" | "permit" | "deny" | "reject" | "refuse"
            ) && plan.body.get("to").is_none_or(|v| v.is_null())
            {
                if let Some(ask_id) = plan.body.get("replyTo").and_then(Value::as_str) {
                    if let Some(sender) = lookup_sender(&url, &token, ask_id) {
                        plan.body["to"] = json!(sender);
                    }
                }
            }
            let mut path = plan.path;
            if !plan.query.is_empty() {
                path.push('?');
                path.push_str(
                    &plan
                        .query
                        .iter()
                        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
                        .collect::<Vec<_>>()
                        .join("&"),
                );
            }
            let response = request(
                &url,
                &token,
                &plan.method,
                &path,
                (!plan.body.is_null()).then_some(plan.body.clone()),
            );
            // slate.mjs's two-step for the show verbs: a 404 on the single
            // read falls back to the list and finds the record there; any
            // other error rethrows.
            let response = match response {
                Err(error)
                    if (error.starts_with("not_found:") || error.starts_with("http_404"))
                        && matches!(other, "task-show" | "task" | "worker-show") =>
                {
                    let (list_path, key) = match other {
                        "task-show" | "task" => ("/orchestration/tasks", "task"),
                        _ => ("/orchestration/dispatches", "dispatch"),
                    };
                    let id = parsed
                        .pick(&["id", "task", "taskId", "dispatch", "dispatchId"])
                        .or_else(|| parsed.positionals().get(1).cloned())
                        .unwrap_or_default();
                    let list = request(&url, &token, "GET", list_path, None)?;
                    let plural = format!("{key}s");
                    let found = data_of(&list)
                        .get(&plural)
                        .and_then(Value::as_array)
                        .and_then(|items| {
                            items
                                .iter()
                                .find(|item| {
                                    item.get("id").and_then(Value::as_str) == Some(id.as_str())
                                })
                                .cloned()
                        })
                        .ok_or_else(|| format!("not_found: no {key} \"{id}\""))?;
                    let mut inner = serde_json::Map::new();
                    inner.insert(key.to_owned(), found);
                    json!({ "data": Value::Object(inner) })
                }
                other_result => other_result?,
            };
            print_response(other, &parsed, &response)?;
        }
    }
    Ok(())
}

/// The control endpoint + token, resolved the same way everywhere: the
/// terminal-stamped environment first, then the user-data dir the app
/// publishes to. Commands that only sometimes need the server (canvas focus)
/// call this lazily so the rest of their surface works with the app closed.
fn control() -> Result<(String, String), String> {
    // orc.mjs resolved SOCKET_PATH → URL → CONTROL_PORT. The port order
    // matters here because nothing listens on TCP — a stale
    // WORKSPACE_CONTROL_PORT must fall *behind* the socket the app is
    // actually serving on, not ahead of it.
    let url = env::var("SLATE_SOCKET_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| env::var("SLATE_URL").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            env::var("WORKSPACE_CONTROL_PORT")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|port| format!("http://127.0.0.1:{port}"))
        })
        .unwrap_or_else(|| slate_app::ipc::socket_path(slate_app::ipc::is_dev_environment()));
    let token = env::var("SLATE_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::fs::read_to_string(slate_app::ipc::control_token_path()).ok())
        .ok_or("no_token: No control token; start Slate first")?;
    let token = token.trim().to_owned();
    if token.contains(['\r', '\n']) {
        return Err("no_token: Invalid control token".into());
    }
    Ok((url, token))
}

/// `control()` plus the `GET /presence` probe the other verbs run: `Some`
/// means the running app answered, so a verb that also has a local journal
/// path can prefer the server route — the flow orc always took.
fn live_control() -> Option<(String, String)> {
    let (url, token) = control().ok()?;
    request(&url, &token, "GET", "/presence", None).ok()?;
    Some((url, token))
}

/// `POST /canvas/image`'s extension gate, mirrored client-side so the server
/// path and the offline journal path reject the same files with the route's
/// own message. The shared `classify_media` table keeps this in step with
/// http.rs and `attachments::import_image`.
fn check_image_path(path: &str) -> Result<(), String> {
    if slate_app::attachments::classify_media(std::path::Path::new(path))
        != slate_app::attachments::MediaKind::Image
    {
        return Err(format!("invalid: {path} is not an image"));
    }
    Ok(())
}

/// The server route and the media import accept absolute local paths only,
/// and a relative `imagePath` stops resolving the moment the app's cwd
/// differs from the caller's — absolutize before either path consumes it.
fn absolutize(path: &str) -> String {
    std::path::absolute(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_owned())
}

/// Writes are answered by the pure router as `{ok, version, seq, data}`;
/// slate.mjs unwraps `data` before printing, so every formatter below works on
/// the unwrapped payload and `--json` shows the same value the script saw.
fn data_of(response: &Value) -> &Value {
    match response.get("data") {
        Some(data) if response.get("ok") == Some(&Value::Bool(true)) => data,
        _ => response,
    }
}

/// A record read: `GET /orchestration/{kind}/{id}` answers `{kind: {...}}`
/// while the write that produced it answers the record as `data` directly.
fn record<'a>(data: &'a Value, key: &str) -> &'a Value {
    data.get(key).unwrap_or(data)
}

fn as_str<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn task_line(task: &Value) -> String {
    let deps = task
        .get("deps")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!(
        "  {}  [{}]  {}{}",
        as_str(task, "id"),
        as_str(task, "status"),
        as_str(task, "title"),
        if deps.is_empty() {
            String::new()
        } else {
            format!("  deps={deps}")
        }
    )
}

fn dispatch_line(dispatch: &Value) -> String {
    format!(
        "  {}  {}{}  task={}  term={}  {}",
        as_str(dispatch, "id"),
        as_str(dispatch, "state"),
        match as_str(dispatch, "outcome") {
            "" => String::new(),
            outcome => format!("/{outcome}"),
        },
        as_str(dispatch, "taskId"),
        as_str(dispatch, "terminalId"),
        as_str(dispatch, "agent"),
    )
}

fn message_line(message: &Value) -> String {
    let mut line = format!(
        "  {}  {}  from={}{}{}",
        as_str(message, "id"),
        as_str(message, "type"),
        as_str(message, "from"),
        match as_str(message, "taskId") {
            "" => String::new(),
            task => format!(" task={task}"),
        },
        match as_str(message, "outcome") {
            "" => String::new(),
            outcome => format!(" outcome={outcome}"),
        },
    );
    line.push_str(&format!("\n    {}", as_str(message, "subject")));
    let body = as_str(message, "body");
    if !body.is_empty() {
        line.push_str(&format!(
            "\n    {}",
            body.split('\n').collect::<Vec<_>>().join("\n    ")
        ));
    }
    line
}

fn gate_line(gate: &Value) -> String {
    let status = match gate.get("resolvedAt") {
        Some(at) if !at.is_null() => format!("resolved: {}", as_str(gate, "resolution")),
        _ => "open".to_owned(),
    };
    let options = gate
        .get("options")
        .and_then(Value::as_array)
        .map(|options| options.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    format!(
        "  {}  [{}]  {}{}",
        as_str(gate, "id"),
        status,
        as_str(gate, "question"),
        if options.is_empty() {
            String::new()
        } else {
            format!("  (options: {})", options.join(", "))
        }
    )
}

/// Human output where the raw envelope would be noise; `--json` always wins
/// and anything without a dedicated format falls back to the JSON it came in.
fn print_response(
    command: &str,
    parsed: &slate_app::cli::Args,
    response: &Value,
) -> Result<(), String> {
    let data = data_of(response);
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(data).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let sub = parsed
        .positionals()
        .get(1)
        .map(String::as_str)
        .unwrap_or("");
    let id = || data.get("id").and_then(Value::as_str).unwrap_or("?");
    let pretty = |value: &Value| serde_json::to_string_pretty(value).map_err(|e| e.to_string());
    match (command, sub) {
        ("terminal" | "term", "list" | "ls") => println!("{}", format_workers(data)),
        ("terminal" | "term", "read" | "output" | "cat") => {
            println!("{}", as_str(data, "output"))
        }
        ("terminal" | "term", "send" | "write") => println!("sent to {}", id()),
        ("terminal" | "term", "new" | "spawn" | "open") => println!("terminal {}", id()),
        ("terminal" | "term", "rename") => println!(
            "{} is now \"{}\"",
            id(),
            data.get("name").and_then(Value::as_str).unwrap_or("?")
        ),
        ("terminal" | "term", "close" | "kill") => println!("closed {}", id()),
        ("rename", _) => println!(
            "{} is now \"{}\"",
            id(),
            data.get("name").and_then(Value::as_str).unwrap_or("?")
        ),
        ("reset", _) => println!("reset"),

        // --- runs & tasks ---------------------------------------------------
        ("run-create", _) => println!("run {} — {}", id(), as_str(data, "objective")),
        ("run-close", _) => println!("run {} closed", id()),
        ("run-list" | "runs", _) => {
            let runs = data.get("runs").and_then(Value::as_array);
            match runs {
                Some(runs) if !runs.is_empty() => {
                    for run in runs {
                        println!(
                            "  {}{}{}",
                            as_str(run, "id"),
                            if run.get("closedAt").is_none_or(Value::is_null) {
                                " (active)"
                            } else {
                                " (closed)"
                            },
                            format_args!("  {}", as_str(run, "objective")),
                        );
                    }
                }
                _ => println!("no runs yet — slate run-create --objective \"...\""),
            }
        }
        ("task-create", _) => println!(
            "task {} [{}] {}",
            id(),
            as_str(data, "status"),
            as_str(data, "title")
        ),
        ("task-update", _) => println!("task {} → {}", id(), as_str(data, "status")),
        ("task-list" | "tasks", _) => {
            let tasks = data.get("tasks").and_then(Value::as_array);
            match tasks {
                Some(tasks) if !tasks.is_empty() => {
                    for task in tasks {
                        println!("{}", task_line(task));
                    }
                }
                _ => println!("  (no tasks)"),
            }
        }
        ("task-show" | "task", _) => {
            let task = record(data, "task");
            println!("Task: {} [{}]", as_str(task, "id"), as_str(task, "status"));
            println!("Title: {}", as_str(task, "title"));
            println!("Run: {}", as_str(task, "runId"));
            let deps = task
                .get("deps")
                .and_then(Value::as_array)
                .map(|deps| {
                    deps.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!(
                "Dependencies: {}",
                if deps.is_empty() { "none".into() } else { deps }
            );
            if !as_str(task, "outcome").is_empty() {
                println!("Outcome: {}", as_str(task, "outcome"));
            }
            println!("Created by: {}", as_str(task, "createdBy"));
            println!("Specification:");
            println!(
                "  {}",
                as_str(task, "spec")
                    .split('\n')
                    .collect::<Vec<_>>()
                    .join("\n  ")
            );
        }

        // --- dispatches -------------------------------------------------------
        ("worker-start" | "dispatch", _) => {
            // The route returns `dispatchId` (plus the nested `dispatch`);
            // `data.id` does not exist — orc printed the real id here.
            let dispatch_id = data
                .get("dispatchId")
                .and_then(Value::as_str)
                .or_else(|| {
                    data.get("dispatch")
                        .and_then(|dispatch| dispatch.get("id"))
                        .and_then(Value::as_str)
                })
                .unwrap_or("?");
            println!(
                "dispatch {} — task {} → terminal {} ({})",
                dispatch_id,
                as_str(data, "taskId"),
                as_str(data, "terminalId"),
                as_str(data, "agent"),
            );
            // The server reports whether the preamble actually landed —
            // a local `--no-inject` is one reason, but so is a launch the
            // server itself refused to brief.
            if data.get("injected").and_then(Value::as_bool) == Some(false) {
                println!("  preamble NOT injected — type it into the terminal yourself");
            }
        }
        ("worker-release" | "worker-retain", _) => {
            println!("dispatch {} → {}", id(), as_str(data, "state"))
        }
        ("worker-show", _) => {
            let dispatch = record(data, "dispatch");
            if parsed.flag("preamble") {
                println!("{}", as_str(dispatch, "preamble"));
            } else {
                println!("{}", dispatch_line(dispatch).trim_start());
            }
        }
        ("dispatch-show" | "dispatches", _) => {
            let dispatches = data.get("dispatches").and_then(Value::as_array);
            match dispatches {
                Some(dispatches) if !dispatches.is_empty() => {
                    for dispatch in dispatches {
                        println!("{}", dispatch_line(dispatch));
                    }
                }
                _ => println!("  (none)"),
            }
        }

        // --- mail -------------------------------------------------------------
        ("send" | "mail" | "msg", _) => println!("sent {}", id()),
        ("done", _) => {
            println!("worker_done sent ({})", id());
            let settled = data.get("settled");
            if let Some(settled) = settled {
                println!(
                    "  task {} → {}",
                    as_str(settled, "taskId"),
                    as_str(settled, "status")
                );
                let promoted = settled
                    .get("promoted")
                    .and_then(Value::as_array)
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                if !promoted.is_empty() {
                    println!("  now ready: {promoted}");
                }
            }
        }
        ("escalate", _) => println!("escalation sent ({})", id()),
        ("heartbeat", _) => println!("heartbeat sent ({})", id()),
        ("reply", _) => println!("replied ({})", id()),
        ("allow" | "approve" | "permit", _) => println!("permission granted ({})", id()),
        ("deny" | "reject" | "refuse", _) => println!("permission denied ({})", id()),
        ("ack", _) => println!("acknowledged {}", id()),

        // --- gates ------------------------------------------------------------
        ("gate-create", _) => println!("gate {} — {}", id(), as_str(data, "question")),
        ("gate-resolve", _) => {
            println!("gate {} resolved: {}", id(), as_str(data, "resolution"))
        }
        ("gate-list" | "gates", _) => {
            let gates = data.get("gates").and_then(Value::as_array);
            match gates {
                Some(gates) if !gates.is_empty() => {
                    for gate in gates {
                        println!("{}", gate_line(gate));
                    }
                }
                _ => println!("  (no gates)"),
            }
        }

        _ => println!("{}", pretty(data)?),
    }
    Ok(())
}

/// `slate plan …` — appends plan.* entries to the command journal, the same
/// stream the planner pane folds on every refresh. Ids can be given as a
/// unique prefix.
fn run_plan(parsed: &slate_app::cli::Args) -> Result<(), String> {
    use slate_app::{journal_log::JournalLog, planner, planner_document::PlannerDocument};
    let dir = slate_app::ipc::user_data_dir();
    let journal_path = dir.join("command-journal.ndjson");
    let doc_path = dir.join("workspace-planner.json");
    let today = planner::today_local();
    let load_doc = |_: &()| -> Result<PlannerDocument, String> {
        let log = JournalLog::open(&journal_path).map_err(|e| e.to_string())?;
        PlannerDocument::recover(&doc_path, &log, &today)
    };
    let sub = parsed
        .positionals()
        .get(1)
        .cloned()
        .unwrap_or_else(|| "list".into());
    let mut journal = JournalLog::open(&journal_path).map_err(|e| e.to_string())?;
    match sub.as_str() {
        "list" | "ls" => {
            let doc = load_doc(&())?;
            if parsed.flag("json") {
                let items: Vec<Value> = doc.items.values().map(|i| json!(i)).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "items": items })).unwrap()
                );
                return Ok(());
            }
            if doc.items.is_empty() {
                println!("  (no plans yet — `slate plan create \"title\"`)");
            }
            for item in doc.items.values() {
                println!(
                    "  {} {}  ({}){}",
                    if item.done { "[x]" } else { "[ ]" },
                    item.title,
                    item.id,
                    item.day
                        .as_deref()
                        .map(|d| format!("  day={d}"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        "create" | "add" | "new" => {
            let title = parsed
                .pick(&["title"])
                .or_else(|| {
                    let rest: Vec<String> = parsed.positionals().iter().skip(2).cloned().collect();
                    (!rest.is_empty()).then(|| rest.join(" "))
                })
                .ok_or("invalid: plan create needs a title")?;
            // Only keys with values go in the payload — the fold treats an
            // absent key as "leave alone" and `null` as "clear".
            let mut payload = json!({ "title": title });
            for key in ["note", "day", "time", "project"] {
                if let Some(value) = parsed.pick(&[key]) {
                    payload[key] = json!(value);
                }
            }
            let attachments = parsed.list(&["attachments", "attachment"]);
            if !attachments.is_empty() {
                payload["attachments"] = json!(attachments);
            }
            // `createItem` stamped `order: nextOrder(day)` — max order among
            // same-day items + 1 — so new tasks sort after their day-mates
            // instead of all landing on 0.
            let doc = load_doc(&())?;
            let day = planner::safe_day(payload.get("day"), &today);
            payload["order"] = json!(planner::next_order(doc.items.values(), day.as_deref()));
            let entry = journal
                .commit("cli", "plan.create", "plan:new", payload)
                .map_err(|e| e.to_string())?;
            println!("plan created (seq {})", entry.seq);
            Ok(())
        }
        "toggle" | "done" | "complete" | "update" | "delete" | "rm" => {
            // Destructive, like slate.mjs: `plan delete` needs --yes — and
            // the gate fires before the id lookup, so `--yes` is reported
            // even when the id is missing.
            if matches!(sub.as_str(), "delete" | "rm") && !parsed.flag("yes") {
                return Err(
                    "needs_confirm:  destructive: plan delete — add --yes to confirm".to_owned(),
                );
            }
            let id_or_prefix = parsed
                .pick(&["id"])
                .or_else(|| parsed.positionals().get(2).cloned())
                .ok_or("invalid: needs a plan id (or unique prefix)")?;
            let doc = load_doc(&())?;
            let matches: Vec<&String> = doc
                .items
                .keys()
                .filter(|id| id.as_str() == id_or_prefix || id.starts_with(&id_or_prefix))
                .collect();
            let id = match matches.len() {
                0 => return Err(format!("not_found: no plan item matches \"{id_or_prefix}\"")),
                1 => matches[0].clone(),
                _ => {
                    return Err(format!(
                        "invalid: \"{id_or_prefix}\" matches {} items — be more specific",
                        matches.len()
                    ))
                }
            };
            match sub.as_str() {
                // slate.mjs `done`/`complete` mark the item rather than flip
                // it; `toggle` honours an explicit --done and flips otherwise.
                "toggle" | "done" | "complete" => {
                    let done = if matches!(sub.as_str(), "done" | "complete") {
                        true
                    } else if parsed.flag_off("done") {
                        false
                    } else {
                        parsed
                            .pick(&["done"])
                            .map(|v| !matches!(v.as_str(), "false" | "0" | "no"))
                            .unwrap_or(!doc.items[&id].done)
                    };
                    journal.commit(
                        "cli",
                        "plan.toggle",
                        &format!("plan:{id}"),
                        json!({ "done": done }),
                    )?;
                    println!(
                        "{} {}",
                        if done { "[x]" } else { "[ ]" },
                        doc.items[&id].title
                    );
                }
                "update" => {
                    // patch semantics: only fields actually given are written —
                    // a `null` in the payload would clear the stored value.
                    let mut payload = json!({});
                    for key in ["title", "note", "day", "time", "project"] {
                        if let Some(value) = parsed.pick(&[key]) {
                            payload[key] = json!(value);
                        }
                    }
                    let attachments = parsed.list(&["attachments", "attachment"]);
                    if !attachments.is_empty() {
                        payload["attachments"] = json!(attachments);
                    }
                    if parsed.flag_off("attachments") {
                        payload["attachments"] = json!(Vec::<String>::new());
                    }
                    journal.commit("cli", "plan.update", &format!("plan:{id}"), payload)?;
                    println!("updated {id}");
                }
                _ => {
                    journal.commit("cli", "plan.delete", &format!("plan:{id}"), json!({}))?;
                    println!("deleted {id}");
                }
            }
            Ok(())
        }
        other => Err(format!(
            "unknown_command: unknown plan subcommand \"{other}\" — try list|create|toggle|update|delete"
        )),
    }
}

/// The command journal's on-disk path — the same file `slate plan` writes and
/// `canvas_view` replays.
fn journal_path() -> PathBuf {
    slate_app::ipc::user_data_dir().join("command-journal.ndjson")
}

/// `slate journal [types | N | tail [--lines n] [--since seq]]` — orc took
/// the seq as a bare positional (`orc journal 5`) and let the server cap
/// at 500. The journal is NDJSON on disk; reading it directly answers even
/// when the app is not running.
fn run_journal(parsed: &slate_app::cli::Args) -> Result<(), String> {
    use slate_app::journal_log::JournalLog;
    let path = journal_path();
    // A numeric positional is the `--since` seq, orc-style; words are
    // subcommands.
    let first = parsed.positionals().get(1).map(String::as_str);
    let positional_since =
        first.filter(|word| word.chars().all(|c| c.is_ascii_digit()) && !word.is_empty());
    let sub = first
        .filter(|_| positional_since.is_none())
        .unwrap_or("tail");
    let log = JournalLog::open(&path)?;
    match sub {
        "types" => {
            let mut counts = std::collections::BTreeMap::<&str, usize>::new();
            for entry in log.entries() {
                *counts.entry(entry.entry_type.as_str()).or_default() += 1;
            }
            if parsed.flag("json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "types": counts }))
                        .map_err(|e| e.to_string())?
                );
            } else if counts.is_empty() {
                println!("  (no events)");
            } else {
                for (entry_type, count) in counts {
                    println!("  {entry_type}  ×{count}");
                }
            }
            Ok(())
        }
        "tail" | "show" | "list" => {
            // `journal.since(seq)` is exclusive — entries after the seq,
            // not including it — and the original capped the answer at 500.
            let since = parsed
                .pick(&["since"])
                .or_else(|| positional_since.map(str::to_owned))
                .or_else(|| parsed.positionals().get(2).cloned())
                .and_then(|raw| raw.parse::<u64>().ok());
            let lines = parsed
                .pick(&["lines", "limit", "n"])
                .and_then(|raw| raw.parse::<usize>().ok())
                .unwrap_or(500);
            let entries: Vec<_> = log
                .entries()
                .iter()
                .filter(|entry| since.is_none_or(|seq| entry.seq > seq))
                .collect();
            if parsed.flag("json") {
                let events: Vec<Value> = entries
                    .iter()
                    .map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({ "lastSeq": log.sequence(), "entries": events })
                    )
                    .map_err(|e| e.to_string())?
                );
                return Ok(());
            }
            if entries.is_empty() {
                println!("  (no events)");
                return Ok(());
            }
            for entry in &entries[entries.len().saturating_sub(lines)..] {
                // orc's line shape — `  {time}  {type}  [{agent}]  {detail}` —
                // against the real schema: seq for time, actor for [agent],
                // the commit target for detail.
                println!(
                    "  #{}  {}  [{}]  {}",
                    entry.seq, entry.entry_type, entry.actor_id, entry.target
                );
            }
            Ok(())
        }
        other => Err(format!(
            "unknown_command: unknown journal subcommand \"{other}\" — try tail|types"
        )),
    }
}

/// A widget named on the command line: exact id, then exact title, then a
/// unique id prefix — the same ladder `slate plan` uses for plan ids.
fn resolve_widget<'a>(
    state: &'a slate_app::projection::CanvasState,
    given: &str,
) -> Result<&'a slate_app::projection::Widget, String> {
    if let Some(widget) = state.widgets.get(given) {
        return Ok(widget);
    }
    let by_title: Vec<_> = state
        .widgets
        .values()
        .filter(|widget| widget.title.eq_ignore_ascii_case(given))
        .collect();
    if by_title.len() == 1 {
        return Ok(by_title[0]);
    }
    let matches: Vec<_> = state
        .widgets
        .values()
        .filter(|widget| widget.id.starts_with(given))
        .collect();
    match matches.len() {
        0 => Err(format!("not_found: no widget matches \"{given}\"")),
        1 => Ok(matches[0]),
        n => Err(format!(
            "invalid: \"{given}\" matches {n} widgets — be more specific"
        )),
    }
}

fn num_flag(parsed: &slate_app::cli::Args, name: &str) -> Option<f64> {
    parsed
        .pick(&[name])
        .and_then(|raw| raw.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

/// `slate canvas|pane …` — widgets (the panes) live in the command journal,
/// so list/move/rename/close fold it or append to it locally, exactly the
/// events `canvas_view` writes. `focus` is the exception: it rides the socket
/// to the running app.
fn run_canvas(parsed: &slate_app::cli::Args) -> Result<(), String> {
    use slate_app::journal_log::JournalLog;
    use slate_app::projection::{self, CanvasState};
    let mut journal = JournalLog::open(journal_path())?;
    let state = projection::fold(
        journal.entries(),
        CanvasState::default(),
        projection::Clock(0.0),
    );
    let sub = parsed
        .positionals()
        .get(1)
        .map(String::as_str)
        .unwrap_or("list");
    match sub {
        "list" | "ls" | "widgets" => {
            let widgets: Vec<_> = projection::list_widgets(&state);
            if parsed.flag("json") {
                let list: Vec<Value> = widgets
                    .iter()
                    .map(|w| {
                        json!({
                            "id": w.id, "kind": w.kind, "title": w.title,
                            "x": w.x, "y": w.y, "w": w.w, "h": w.h, "z": w.z,
                            "maximized": w.maximized,
                            "imagePath": w.image_path, "imageName": w.image_name,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "widgets": list }))
                        .map_err(|e| e.to_string())?
                );
            } else if widgets.is_empty() {
                println!("  (no widgets)");
            } else {
                for w in widgets {
                    println!(
                        "  {}  [{}]  {}  at ({}, {}) {}x{}",
                        w.id,
                        w.kind.as_deref().unwrap_or("widget"),
                        w.title,
                        w.x,
                        w.y,
                        w.w,
                        w.h
                    );
                    if let Some(path) = &w.image_path {
                        println!("    source: {path}");
                    }
                }
            }
            Ok(())
        }
        "move" => {
            let given = parsed
                .pick(&["id", "widget"])
                .or_else(|| parsed.positionals().get(2).cloned())
                .ok_or("invalid: canvas move needs <id|name>")?;
            let id = resolve_widget(&state, &given)?.id.clone();
            let mut patch = json!({});
            for flag in ["x", "y", "w", "h"] {
                if let Some(value) = num_flag(parsed, flag) {
                    patch[flag] = json!(value);
                }
            }
            // orc PATCHed whatever flags arrived — an empty patch is a
            // harmless no-op `widget.update` on the server, not a CLI error.
            journal
                .commit("cli", "widget.update", &format!("widget:{id}"), patch)
                .map_err(|e| e.to_string())?;
            println!("moved {id}");
            Ok(())
        }
        "rename" => {
            let given = parsed
                .pick(&["id", "widget"])
                .or_else(|| parsed.positionals().get(2).cloned())
                .ok_or("invalid: canvas rename needs <id|name>")?;
            let name = parsed
                .pick(&["name", "title", "as"])
                .or_else(|| parsed.positionals().get(3).cloned())
                .filter(|name| !name.trim().is_empty())
                .ok_or("invalid: canvas rename needs <name> or --name <name>")?;
            let id = resolve_widget(&state, &given)?.id.clone();
            journal
                .commit(
                    "cli",
                    "widget.update",
                    &format!("widget:{id}"),
                    json!({ "title": name }),
                )
                .map_err(|e| e.to_string())?;
            println!("{id} is now \"{name}\"");
            Ok(())
        }
        "close" | "remove" | "rm" => {
            // orc's confirm gate fires before the id lookup, so the bare
            // command reports `--yes` rather than a missing id.
            let given = parsed
                .pick(&["id", "widget"])
                .or_else(|| parsed.positionals().get(2).cloned());
            if !parsed.flag("yes") {
                let label = given.as_deref().unwrap_or("");
                return Err(format!(
                    "needs_confirm:  destructive: canvas close {label} — add --yes to confirm"
                ));
            }
            let given = given.ok_or("invalid: canvas close needs <id|name>")?;
            let widget = resolve_widget(&state, &given)?;
            let (id, kind) = (widget.id.clone(), widget.kind.clone());
            journal
                .commit("cli", "widget.remove", &format!("widget:{id}"), json!({}))
                .map_err(|e| e.to_string())?;
            // A terminal widget's PTY outlives its widget — kill it too, best
            // effort, so `canvas close` is not a leaked-process machine.
            if kind.as_deref() == Some("terminal") {
                if let Ok((url, token)) = control() {
                    let _ = request(
                        &url,
                        &token,
                        "DELETE",
                        &format!("/terminal/{}", encode(&id)),
                        None,
                    );
                }
            }
            println!("closed {id}");
            Ok(())
        }
        "place" | "spawn" | "new" | "add" => {
            // The Electron shell posted /widgets; natively a widget is a
            // `widget.create` journal event — the live canvas refolds on
            // journal change, so committing here is the same spawn path.
            let kind = parsed
                .pick(&["kind"])
                .or_else(|| parsed.positionals().get(2).cloned())
                .ok_or("invalid: canvas place needs <kind> or --kind <kind>")?;
            let image_path = parsed
                .pick(&["path", "image", "file"])
                .or_else(|| parsed.positionals().get(3).cloned());
            // orc had no client-side kind whitelist — the kind rides to
            // /widgets and the server is the gate. `image` without --path is
            // likewise just POST /widgets {kind:"image"}.
            if kind == "image" && image_path.is_some() {
                // The route rejects non-images before importing; check the
                // same gate before either path so both refuse alike.
                check_image_path(image_path.as_deref().unwrap_or_default())?;
            }
            // The Electron shell posted /canvas/image for an image kind with
            // a file — that route runs the media-store import — and /widgets
            // for everything else. The `widget.create` journal commit below
            // stays as the offline path.
            if let Some((url, token)) = live_control() {
                // orc sent exactly `{path, title?, x?, y?}` to /canvas/image
                // and `{kind, title?, x?, y?}` to /widgets; unset fields are
                // absent so the route's own defaults apply.
                let mut body = json!({});
                if kind == "image" && image_path.is_some() {
                    body["path"] = json!(absolutize(image_path.as_deref().unwrap_or_default()));
                } else {
                    body["kind"] = json!(kind);
                }
                if let Some(title) = parsed.pick(&["title", "name", "as"]) {
                    body["title"] = json!(title);
                }
                for flag in ["x", "y"] {
                    if let Some(value) = num_flag(parsed, flag) {
                        body[flag] = json!(value);
                    }
                }
                let route = if kind == "image" && image_path.is_some() {
                    "/canvas/image"
                } else {
                    "/widgets"
                };
                let response = request(&url, &token, "POST", route, Some(body))?;
                let data = data_of(&response);
                if parsed.flag("json") {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(data).map_err(|e| e.to_string())?
                    );
                } else {
                    let placed = as_str(data, "id");
                    match (
                        data.get("x").and_then(Value::as_f64),
                        data.get("y").and_then(Value::as_f64),
                    ) {
                        (Some(x), Some(y)) => {
                            println!("placed {placed} ({kind}) at ({x}, {y})")
                        }
                        _ => println!("placed {placed} ({kind})"),
                    }
                }
                return Ok(());
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let id = format!("{kind}-{stamp}");
            let title = parsed.pick(&["title", "name", "as"]).unwrap_or_else(|| {
                let mut chars = kind.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
                    .unwrap_or_else(|| kind.clone())
            });
            let (w, h) = match kind.as_str() {
                "planner" => (420.0, 520.0),
                "files" => (580.0, 480.0),
                "orchestration" => (520.0, 560.0),
                "notes" | "links" => (360.0, 400.0),
                "timer" => (320.0, 300.0),
                "sys-monitor" => (420.0, 420.0),
                "kanban" => (560.0, 480.0),
                "calendar" => (420.0, 400.0),
                "image" => (480.0, 360.0),
                // The original registry's fallback for unregistered kinds.
                _ => (520.0, 360.0),
            };
            // No --x/--y lands the widget just inside the camera's view, so
            // a CLI place is visible without a camera move.
            let x = num_flag(parsed, "x").unwrap_or(state.camera.x + 120.0);
            let y = num_flag(parsed, "y").unwrap_or(state.camera.y + 80.0);
            let z = state.widgets.values().map(|w| w.z).fold(0.0_f64, f64::max) + 1.0;
            let mut payload = json!({
                "id": id, "title": title, "kind": kind,
                "x": x, "y": y, "w": w, "h": h, "z": z, "maximized": false,
            });
            if let Some(path) = image_path {
                // A relative path would stop resolving the moment the app's
                // cwd differs from the caller's — store it absolute.
                payload["imagePath"] = json!(absolutize(&path));
                payload["imageName"] = json!(std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone()));
            }
            journal
                .commit("cli", "widget.create", &format!("widget:{id}"), payload)
                .map_err(|e| e.to_string())?;
            println!("placed {id} ({kind}) at ({x}, {y})");
            Ok(())
        }
        "image" => {
            let given = parsed
                .pick(&["path", "image", "file"])
                .or_else(|| parsed.positionals().get(2).cloned())
                .ok_or("invalid: canvas image needs <path> or --path <file>")?;
            check_image_path(&given)?;
            let path = absolutize(&given);
            // With the app up, `POST /canvas/image` runs the media-store
            // import — a content-addressed copy into the media dir — exactly
            // the flow orc always took. The journal commit below is the
            // offline fallback.
            if let Some((url, token)) = live_control() {
                let mut body = json!({ "path": path });
                if let Some(title) = parsed.pick(&["title", "name"]) {
                    body["title"] = json!(title);
                }
                for flag in ["x", "y"] {
                    if let Some(value) = num_flag(parsed, flag) {
                        body[flag] = json!(value);
                    }
                }
                let response = request(&url, &token, "POST", "/canvas/image", Some(body))?;
                let data = data_of(&response);
                if parsed.flag("json") {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(data).map_err(|e| e.to_string())?
                    );
                } else {
                    println!("placed {} (image)", as_str(data, "id"));
                }
                return Ok(());
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let id = format!("image-{stamp}");
            let title = parsed.pick(&["title", "name"]).unwrap_or_else(|| {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Image".into())
            });
            let x = num_flag(parsed, "x").unwrap_or(state.camera.x + 120.0);
            let y = num_flag(parsed, "y").unwrap_or(state.camera.y + 80.0);
            let z = state.widgets.values().map(|w| w.z).fold(0.0_f64, f64::max) + 1.0;
            journal
                .commit(
                    "cli",
                    "widget.create",
                    &format!("widget:{id}"),
                    json!({
                        "id": id, "title": title, "kind": "image",
                        "x": x, "y": y, "w": 480.0, "h": 360.0, "z": z,
                        "maximized": false,
                        "imagePath": path, "imageName": title,
                    }),
                )
                .map_err(|e| e.to_string())?;
            println!("placed {id} (image) at ({x}, {y})");
            Ok(())
        }
        "focus" => {
            // Two routes: with an id it's our widget-focus extension on
            // /canvas/focus; without one it's orc's camera move verbatim —
            // POST /canvas/camera {x?, y?, zoom ?? 1} — where even a bare
            // `focus` posts {zoom:1} and snaps back to 100%.
            let mut body = json!({});
            let given = parsed
                .pick(&["id", "to", "widget"])
                .or_else(|| parsed.positionals().get(2).cloned());
            if let Some(given) = &given {
                body["id"] = json!(resolve_widget(&state, given)?.id);
            }
            for flag in ["x", "y", "zoom"] {
                if let Some(value) = num_flag(parsed, flag) {
                    body[flag] = json!(value);
                }
            }
            let (url, token) =
                control().map_err(|e| format!("canvas focus needs the running app — {e}"))?;
            let route = if given.is_some() {
                "/canvas/focus"
            } else {
                body["zoom"] = json!(num_flag(parsed, "zoom").unwrap_or(1.0));
                "/canvas/camera"
            };
            let focused = body
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("canvas")
                .to_owned();
            let response = request(&url, &token, "POST", route, Some(body))?;
            println!(
                "{}",
                if parsed.flag("json") {
                    serde_json::to_string_pretty(data_of(&response))
                        .map_err(|e| e.to_string())?
                } else {
                    format!("focused {focused}")
                }
            );
            Ok(())
        }
        other => Err(format!(
            "unknown_command: unknown canvas subcommand \"{other}\" — try list|move|rename|focus|close"
        )),
    }
}

/// `slate browser …` — orc's browser surface end to end: `list` reads
/// `GET /browser` (a journal fold when the app is closed); the action verbs
/// POST to `/browser/:id/{navigate,click,fill,select,press,scroll}` and `open`
/// mints a canvas browser widget, all on the live control server.
fn run_browser(parsed: &slate_app::cli::Args) -> Result<(), String> {
    let sub = parsed
        .positionals()
        .get(1)
        .map(String::as_str)
        .unwrap_or("list");
    match sub {
        "list" | "ls" | "status" => {}
        _ => return run_browser_action(parsed, sub),
    }
    let browsers = match live_control() {
        Some((url, token)) => {
            let response = request(&url, &token, "GET", "/browser", None)?;
            data_of(&response)
                .get("browsers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        }
        None => {
            // Offline: the same fold `GET /browser` runs — canvas widgets
            // whose kind is "browser", z-descending.
            use slate_app::journal_log::JournalLog;
            use slate_app::projection::{self, CanvasState};
            let journal = JournalLog::open(journal_path())?;
            let state = projection::fold(
                journal.entries(),
                CanvasState::default(),
                projection::Clock(0.0),
            );
            let mut widgets: Vec<Value> = state
                .widgets
                .values()
                .filter(|widget| widget.kind.as_deref() == Some("browser"))
                .map(|widget| {
                    json!({
                        "id": widget.id, "title": widget.title,
                        "surface": "canvas",
                        "x": widget.x, "y": widget.y, "z": widget.z,
                    })
                })
                .collect();
            widgets.sort_by(|a, b| {
                b["z"]
                    .as_f64()
                    .partial_cmp(&a["z"].as_f64())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            widgets
        }
    };
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "browsers": browsers }))
                .map_err(|e| e.to_string())?
        );
    } else if browsers.is_empty() {
        println!("  (no browser widgets)");
    } else {
        for browser in &browsers {
            println!(
                "  {}  {}  [{}] ({}, {})",
                as_str(browser, "id"),
                as_str(browser, "title"),
                as_str(browser, "surface"),
                browser.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                browser.get("y").and_then(Value::as_f64).unwrap_or(0.0),
            );
        }
    }
    Ok(())
}

/// `slate browser <action>` — everything but `list`: the request shape is
/// orc's, `post('/browser/:id/<action>', {...})` with absent options omitted
/// (JSON.stringify drops `undefined`, so no null fields go on the wire).
fn run_browser_action(parsed: &slate_app::cli::Args, sub: &str) -> Result<(), String> {
    let Some((url, token)) = live_control() else {
        return Err("offline: browser actions need the running app".into());
    };
    let pos = parsed.positionals();
    let id = |what: &str| -> Result<String, String> {
        pos.get(2)
            .cloned()
            .ok_or_else(|| format!("invalid: {what}"))
    };
    let pick = |names: &[&str], at: usize| pos.get(at).cloned().or_else(|| parsed.pick(names));
    let mut body = serde_json::Map::new();
    let (method, path) = match sub {
        "open" => {
            put(&mut body, "url", pick(&["url"], 2));
            put(&mut body, "surface", parsed.pick(&["surface"]));
            ("POST", "/browser/open".to_owned())
        }
        "navigate" | "goto" => {
            let id = id("browser navigate needs <id> <url>")?;
            let url = pick(&["url"], 3).ok_or("invalid: browser navigate needs <id> <url>")?;
            put(&mut body, "url", Some(url));
            ("POST", format!("/browser/{id}/navigate"))
        }
        "snapshot" => {
            let id = id("browser snapshot needs <id>")?;
            return browser_print(request(
                &url,
                &token,
                "GET",
                &format!("/browser/{id}/snapshot"),
                None,
            )?);
        }
        "click" => {
            let id = id("browser click needs <id> <ref>")?;
            put(
                &mut body,
                "ref",
                Some(pick(&["ref"], 3).ok_or("invalid: browser click needs <id> <ref>")?),
            );
            ("POST", format!("/browser/{id}/click"))
        }
        "fill" => {
            let id = id("browser fill needs <id> <ref>")?;
            put(
                &mut body,
                "ref",
                Some(pick(&["ref"], 3).ok_or("invalid: browser fill needs <id> <ref>")?),
            );
            put(
                &mut body,
                "value",
                Some(
                    pick(&["value", "text"], 4)
                        .ok_or("invalid: browser fill needs --value \"...\"")?,
                ),
            );
            ("POST", format!("/browser/{id}/fill"))
        }
        "select" => {
            let id = id("browser select needs <id> <ref>")?;
            put(
                &mut body,
                "ref",
                Some(pick(&["ref"], 3).ok_or("invalid: browser select needs <id> <ref>")?),
            );
            put(
                &mut body,
                "value",
                Some(pick(&["value"], 4).ok_or("invalid: browser select needs --value \"...\"")?),
            );
            ("POST", format!("/browser/{id}/select"))
        }
        "press" => {
            let id = id("browser press needs <id> <key>")?;
            put(
                &mut body,
                "key",
                Some(pick(&["key"], 3).ok_or("invalid: browser press needs <id> <key>")?),
            );
            put(&mut body, "ref", parsed.pick(&["ref"]));
            ("POST", format!("/browser/{id}/press"))
        }
        "scroll" => {
            let id = id("browser scroll needs <id>")?;
            if let Some(pixels) = pick(&["pixels"], 3) {
                body.insert(
                    "pixels".to_owned(),
                    pixels
                        .parse::<f64>()
                        .map(Value::from)
                        .unwrap_or(json!(pixels)),
                );
            }
            ("POST", format!("/browser/{id}/scroll"))
        }
        other => {
            return Err(format!(
                "invalid: browser: unknown action \"{other}\" \
                 (open|list|navigate|snapshot|click|fill|select|press|scroll)"
            ))
        }
    };
    browser_print(request(
        &url,
        &token,
        method,
        &path,
        Some(Value::Object(body)),
    )?)
}

fn put(body: &mut serde_json::Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        body.insert(key.to_owned(), json!(value));
    }
}

/// Actions emit their JSON payload like orc's `emit(result, browserLine)`,
/// which falls back to a JSON dump whenever the shape isn't a browser list.
fn browser_print(response: Value) -> Result<(), String> {
    let data = data_of(&response);
    println!(
        "{}",
        serde_json::to_string_pretty(data).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// `slate git …` — the original ran both verbs through the control server:
/// the workspace checkout, `git add -A` before commit, `{hash, message}`
/// back. Any subcommand that is not `commit` is a status read.
fn run_git(parsed: &slate_app::cli::Args) -> Result<(), String> {
    let (url, token) = control().map_err(|e| format!("git needs the running app — {e}"))?;
    let sub = parsed
        .positionals()
        .get(1)
        .map(String::as_str)
        .unwrap_or("status");
    match sub {
        "commit" => {
            let message = parsed
                .pick(&["message", "m", "msg"])
                .or_else(|| parsed.positionals().get(2).cloned())
                .filter(|message| !message.trim().is_empty())
                .ok_or("invalid: git commit needs --message \"...\"")?;
            let response = request(
                &url,
                &token,
                "POST",
                "/git/commit",
                Some(json!({ "message": message })),
            )?;
            let data = data_of(&response);
            if parsed.flag("json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(data).map_err(|e| e.to_string())?
                );
            } else {
                let hash = as_str(data, "hash");
                let short = hash.chars().take(7).collect::<String>();
                let hash = if short.is_empty() {
                    "committed"
                } else {
                    &short
                };
                let message = data.get("message").and_then(Value::as_str).unwrap_or("");
                if hash == "committed" {
                    println!("committed: {message}");
                } else {
                    println!("committed {hash}: {message}");
                }
            }
            Ok(())
        }
        _ => {
            let response = request(&url, &token, "GET", "/git/status", None)?;
            let data = data_of(&response);
            if parsed.flag("json") {
                println!(
                    "{}",
                    serde_json::to_string_pretty(data).map_err(|e| e.to_string())?
                );
            } else {
                let branch = as_str(data, "branch");
                if !branch.is_empty() {
                    println!("On branch {branch}");
                    let files: Vec<&str> = data
                        .get("files")
                        .and_then(Value::as_array)
                        .map(|files| files.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();
                    if files.is_empty() {
                        println!("  (clean)");
                    } else {
                        for file in files {
                            println!("  {file}");
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// `slate context` — the bundle a freshly-dispatched agent asks for first:
/// folder, code workspace, own terminal, active task, roster size. Presence
/// and code-workspace answers are optional so a server that does not serve
/// them still answers the rest.
fn run_context(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let agent = env::var("SLATE_AGENT_ID")
        .ok()
        .or_else(|| env::var("SLATE_TERMINAL_ID").ok())
        .unwrap_or_else(|| "cli".into());
    let terminal_env = env::var("SLATE_TERMINAL_ID").ok();
    let workers = request(url, token, "GET", "/orchestration/workers", None)?;
    let snapshot = request(url, token, "GET", "/orchestration", None)?;
    let presence = request(url, token, "GET", "/presence", None).ok();
    let code = request(url, token, "GET", "/workspace/code", None).ok();

    let worker_list = data_of(&workers)
        .get("workers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let self_worker = worker_list.iter().find(|w| {
        w.get("self") == Some(&Value::Bool(true))
            || w.get("id").and_then(Value::as_str) == terminal_env.as_deref()
            || w.get("name").and_then(Value::as_str) == Some(agent.as_str())
    });
    let terminal_id = self_worker
        .and_then(|w| w.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| terminal_env.clone())
        .unwrap_or_else(|| agent.clone());
    let terminal_name = self_worker
        .and_then(|w| w.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let active_dispatch = data_of(&snapshot)
        .get("dispatches")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter().find(|d| {
                d.get("state").and_then(Value::as_str) == Some("running")
                    && [Some(agent.as_str()), Some(terminal_id.as_str())]
                        .contains(&d.get("terminalId").and_then(Value::as_str))
            })
        });
    let task_id = active_dispatch
        .and_then(|d| d.get("taskId"))
        .and_then(Value::as_str);
    let task_title = task_id.and_then(|id| {
        data_of(&snapshot)
            .get("tasks")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter()
                    .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
            })
            .and_then(|t| t.get("title"))
            .and_then(Value::as_str)
    });
    let code_workspace = code.as_ref().and_then(|c| {
        let c = data_of(c);
        let active = c.get("activeId").and_then(Value::as_str);
        c.get("workspaces")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter()
                    .find(|w| w.get("id").and_then(Value::as_str) == active)
            })
            .cloned()
    });
    let folder = presence
        .as_ref()
        .and_then(|p| data_of(p).get("workspaceDir"))
        .and_then(Value::as_str);

    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "agentId": agent,
                "folder": folder,
                "codeWorkspace": code_workspace,
                "terminal": terminal_id,
                "terminalName": terminal_name,
                "taskId": task_id,
                "taskTitle": task_title,
                "workers": worker_list.len(),
            }))
            .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("Project folder: {}", folder.unwrap_or("(none)"));
    match &code_workspace {
        Some(ws) => println!(
            "Code Workspace: {} ({})",
            ws.get("name").and_then(Value::as_str).unwrap_or("?"),
            ws.get("id").and_then(Value::as_str).unwrap_or("?")
        ),
        None => println!("Code Workspace: (none)"),
    }
    match &terminal_name {
        Some(name) if name != &terminal_id => println!("Terminal: {name} ({terminal_id})"),
        _ => println!("Terminal: {terminal_id}"),
    }
    match task_id {
        Some(id) => println!(
            "Task: {id}{}",
            task_title.map(|t| format!(" — {t}")).unwrap_or_default()
        ),
        None => println!("Task: (idle)"),
    }
    println!("Workers: {}", worker_list.len());
    Ok(())
}

/// `slate doctor` — one shot at everything an agent asks when the CLI
/// misbehaves: is the socket up, does the version match, can the journal be
/// read, who else is here.
fn run_doctor(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    use slate_app::journal_log::JournalLog;
    let health = request(url, token, "GET", "/health", None).ok();
    let snapshot = request(url, token, "GET", "/orchestration", None)?;
    let workers = request(url, token, "GET", "/orchestration/workers", None)?;
    let presence = request(url, token, "GET", "/presence", None).ok();
    let path = journal_path();
    let journal = match JournalLog::open(&path) {
        Ok(log) => json!({
            "ok": true, "entries": log.entries().len(), "seq": log.sequence(),
        }),
        Err(error) => json!({ "ok": false, "error": error }),
    };
    let version = env!("CARGO_PKG_VERSION");
    let server_version = health
        .as_ref()
        .and_then(|h| h.get("version"))
        .and_then(Value::as_str);
    let drift = server_version.filter(|v| *v != version);
    // orc's AGENT_ID = env || 'cli' — outside a terminal the identity is cli.
    let agent = env::var("SLATE_AGENT_ID")
        .ok()
        .or_else(|| env::var("SLATE_TERMINAL_ID").ok())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "cli".to_owned());
    let workspace = presence
        .as_ref()
        .and_then(|p| p.get("workspaceDir"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            health
                .as_ref()
                .and_then(|h| h.get("workspaceDir"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let open_runs = data_of(&snapshot)
        .get("runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter(|r| r.get("closedAt").is_none_or(|v| v.is_null()))
                .count()
        })
        .unwrap_or(0);
    let worker_count = data_of(&workers)
        .get("workers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);

    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "app": url,
                "version": version,
                "serverVersion": server_version,
                "drift": drift,
                "agentId": agent.as_str(),
                "workspace": workspace,
                "workers": worker_count,
                "runs": open_runs,
                "journal": journal,
                "journalPath": path,
            }))
            .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("Slate: reachable at {url} (slate {version})");
    match (server_version, drift) {
        (_, Some(drifted)) => {
            println!("Server: {drifted} (drift — restart the app to match)")
        }
        (Some(v), _) => println!("Server: {v}"),
        (None, _) => println!("Server: ?"),
    }
    println!("Agent: {agent}");
    println!("Workspace: {}", workspace.as_deref().unwrap_or("(none)"));
    println!("Mode: native slate CLI & orchestration");
    println!("Workers: {worker_count} | open runs: {open_runs}");
    match journal.get("ok") == Some(&Value::Bool(true)) {
        true => println!(
            "Journal: {} entries (seq {}) — {}",
            journal["entries"],
            journal["seq"],
            path.display()
        ),
        false => println!(
            "Journal: unreadable — {}",
            journal["error"].as_str().unwrap_or("?")
        ),
    }
    Ok(())
}

/// `slate whoami` — identity is ambient (the terminal spawn stamped it into
/// the environment) so it must answer with the app closed, but when the
/// socket is reachable it reports the active task and dispatch too, matching
/// `orc whoami`.
fn run_whoami(parsed: &slate_app::cli::Args) -> Result<(), String> {
    // orc's `AGENT_ID || TERMINAL_ID || 'cli'` — either env alone answers,
    // and outside a terminal entirely the identity is `cli`.
    let agent = env::var("SLATE_AGENT_ID")
        .ok()
        .or_else(|| env::var("SLATE_TERMINAL_ID").ok())
        .unwrap_or_else(|| "cli".to_owned());
    let terminal = env::var("SLATE_TERMINAL_ID")
        .ok()
        .unwrap_or_else(|| agent.clone());
    // orc's whoami always hits the API — offline is an error, not a
    // degraded answer.
    let (url, token) = control()?;
    let enrich = || -> Result<Value, String> {
        let workers = request(&url, &token, "GET", "/orchestration/workers", None)?;
        let snapshot = request(&url, &token, "GET", "/orchestration", None)?;
        let workers_data = data_of(&workers).clone();
        let snapshot_data = data_of(&snapshot).clone();
        let worker_list = workers_data
            .get("workers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let worker_list = &worker_list;
        let self_worker = worker_list.iter().find(|w| {
            w.get("self") == Some(&Value::Bool(true))
                || w.get("id").and_then(Value::as_str) == Some(terminal.as_str())
        });
        let active_dispatch = snapshot_data
            .get("dispatches")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter().find(|d| {
                    d.get("state").and_then(Value::as_str) == Some("running")
                        && [Some(agent.as_str()), Some(terminal.as_str())]
                            .contains(&d.get("terminalId").and_then(Value::as_str))
                })
            });
        let task_id = active_dispatch
            .and_then(|d| d.get("taskId"))
            .and_then(Value::as_str);
        let task_title = task_id.and_then(|id| {
            snapshot_data
                .get("tasks")
                .and_then(Value::as_array)
                .and_then(|list| {
                    list.iter()
                        .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
                })
                .and_then(|t| t.get("title"))
                .and_then(Value::as_str)
        });
        Ok(json!({
            "agentId": agent.as_str(),
            "name": self_worker
                .and_then(|w| w.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(agent.as_str()),
            "terminalId": self_worker
                .and_then(|w| w.get("id"))
                .and_then(Value::as_str)
                .unwrap_or(terminal.as_str()),
            "role": self_worker
                .and_then(|w| w.get("agent"))
                .and_then(Value::as_str)
                .unwrap_or("worker"),
            "busy": self_worker
                .and_then(|w| w.get("busy"))
                .and_then(Value::as_bool)
                .unwrap_or(active_dispatch.is_some()),
            "taskId": task_id,
            "taskTitle": task_title,
            "dispatchId": active_dispatch.and_then(|d| d.get("id")).and_then(Value::as_str),
        }))
    };
    let me = enrich()?;
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&me).map_err(|e| e.to_string())?
        );
    } else {
        let name = as_str(&me, "name");
        let terminal_id = as_str(&me, "terminalId");
        println!(
            "You are: {name}{}",
            if name != terminal_id {
                format!(" ({terminal_id})")
            } else {
                String::new()
            }
        );
        println!("Agent ID: {}", as_str(&me, "agentId"));
        match as_str(&me, "taskId") {
            task if !task.is_empty() => println!(
                "Status: busy on task {task}{}",
                match as_str(&me, "taskTitle") {
                    "" => String::new(),
                    title => format!(" (\"{title}\")"),
                }
            ),
            _ => println!("Status: idle"),
        }
        let dispatch = as_str(&me, "dispatchId");
        if !dispatch.is_empty() {
            println!("Dispatch ID: {dispatch}");
        }
    }
    Ok(())
}

/// `slate status` — the coordinator's first question: open run, task counts,
/// live dispatches, settled-but-unaccounted workers, unread mail, open gates.
fn run_status(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let snapshot = request(url, token, "GET", "/orchestration", None)?;
    let inbox = request(url, token, "GET", "/orchestration/inbox?limit=200", None)
        .unwrap_or_else(|_| json!({ "messages": [] }));
    let runs = data_of(&snapshot)
        .get("runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tasks = data_of(&snapshot)
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let dispatches = data_of(&snapshot)
        .get("dispatches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let gates = data_of(&snapshot)
        .get("gates")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let unread = data_of(&inbox)
        .get("messages")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let active = runs
        .iter()
        .find(|r| r.get("closedAt").is_none_or(Value::is_null));
    let count = |status: &str| {
        tasks
            .iter()
            .filter(|t| t.get("status").and_then(Value::as_str) == Some(status))
            .count()
    };
    let running = dispatches
        .iter()
        .filter(|d| d.get("state").and_then(Value::as_str) == Some("running"))
        .count();
    let unaccounted: Vec<&str> = dispatches
        .iter()
        .filter(|d| d.get("state").and_then(Value::as_str) == Some("settled"))
        .filter_map(|d| d.get("id").and_then(Value::as_str))
        .collect();
    let open_gates = gates
        .iter()
        .filter(|g| g.get("resolvedAt").is_none_or(Value::is_null))
        .count();

    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "run": active,
                "tasks": tasks.len(),
                "ready": count("ready"),
                "completed": count("completed"),
                "failed": count("failed"),
                "running": running,
                "unaccounted": unaccounted,
                "unread": unread,
                "openGates": open_gates,
            }))
            .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    match active {
        Some(run) => println!("run {} — {}", as_str(run, "id"), as_str(run, "objective")),
        None => println!("no open run"),
    }
    println!(
        "  tasks: {} total ({} ready, {} completed{})",
        tasks.len(),
        count("ready"),
        count("completed"),
        match count("failed") {
            0 => String::new(),
            failed => format!(", {failed} failed"),
        }
    );
    println!("  workers running: {running}");
    if !unaccounted.is_empty() {
        println!("  settled, awaiting decision: {}", unaccounted.join(", "));
    }
    println!("  unread mail: {unread}");
    if open_gates > 0 {
        println!("  open decision gates: {open_gates}");
    }
    Ok(())
}

/// `slate run-show <id>` — one run plus its task list. A miss on the single-
/// run route falls back to scanning the list, like slate.mjs, so an id that
/// only the collection knows still answers.
fn run_run_show(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let id = parsed
        .pick(&["id", "run", "runId"])
        .or_else(|| parsed.positionals().get(1).cloned())
        .ok_or("invalid: run-show needs <run-id>")?;
    let single = request(
        url,
        token,
        "GET",
        &format!("/orchestration/runs/{}", encode(&id)),
        None,
    );
    let run = match single {
        // orc falls back to the list only when the single read 404s —
        // every other failure (offline, timeout, 500) propagates.
        Ok(response) => data_of(&response).get("run").cloned(),
        Err(error) if error.starts_with("not_found:") || error.starts_with("http_404") => None,
        Err(error) => return Err(error),
    };
    let run = match run {
        Some(run) => run,
        None => {
            let response = request(url, token, "GET", "/orchestration/runs", None)?;
            data_of(&response)
                .get("runs")
                .and_then(Value::as_array)
                .and_then(|runs| {
                    runs.iter()
                        .find(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
                        .cloned()
                })
                .ok_or_else(|| format!("not_found: no run \"{id}\""))?
        }
    };
    let tasks = request(
        url,
        token,
        "GET",
        &format!("/orchestration/tasks?runId={}", encode(&id)),
        None,
    )?;
    let task_list = data_of(&tasks)
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "run": run, "tasks": task_list }))
                .map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!(
        "Run: {}{}",
        as_str(&run, "id"),
        if run.get("closedAt").is_none_or(Value::is_null) {
            " (active)"
        } else {
            " (closed)"
        }
    );
    println!("Objective: {}", as_str(&run, "objective"));
    println!("Coordinator: {}", as_str(&run, "coordinator"));
    println!("Tasks ({}):", task_list.len());
    if task_list.is_empty() {
        println!("  (no tasks)");
    } else {
        for task in &task_list {
            println!("{}", task_line(task));
        }
    }
    Ok(())
}

/// `slate worker-read`/`logs`/`tail` — a dispatch id resolves to its terminal
/// first (slate.mjs), then the tail is sliced client-side. Falls back to this
/// terminal when nothing is named.
fn run_worker_read(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let target = parsed
        .pick(&["to", "id", "dispatch", "dispatchId", "terminal"])
        .or_else(|| parsed.positionals().get(1).cloned())
        .or_else(|| env::var("SLATE_TERMINAL_ID").ok())
        .ok_or("invalid: worker-read needs a terminal id")?;
    let mut terminal_id = target.clone();
    if target.starts_with("disp-") {
        let single = request(
            url,
            token,
            "GET",
            &format!("/orchestration/dispatches/{}", encode(&target)),
            None,
        );
        let resolved = match single {
            Ok(single) => data_of(&single)
                .get("dispatch")
                .and_then(|d| d.get("terminalId"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            // orc rethrows anything that isn't a 404, then lists.
            Err(error) if error.starts_with("not_found:") || error.starts_with("http_404") => {
                let list = request(url, token, "GET", "/orchestration/dispatches", None)?;
                data_of(&list)
                    .get("dispatches")
                    .and_then(Value::as_array)
                    .and_then(|dispatches| {
                        dispatches
                            .iter()
                            .find(|d| d.get("id").and_then(Value::as_str) == Some(target.as_str()))
                            .and_then(|d| d.get("terminalId"))
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
            }
            Err(error) => return Err(error),
        };
        match resolved {
            Some(id) => terminal_id = id,
            // An id-shaped target that resolves to nothing is still tried as
            // a terminal id, as slate.mjs does.
            None if !is_id_like(&target) => {
                return Err(format!("not_found: no dispatch \"{target}\""))
            }
            None => {}
        }
    }
    let limit = parsed
        .pick(&["limit"])
        .or_else(|| parsed.positionals().get(2).cloned())
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .filter(|n| n.is_finite())
        .map(|n| n.trunc().max(0.0) as usize)
        .unwrap_or(50);
    let output = request(
        url,
        token,
        "GET",
        &format!("/terminal/{}/output?full=1", encode(&terminal_id)),
        None,
    )?;
    let text = as_str(data_of(&output), "output").to_owned();
    let tail = if limit == 0 {
        String::new()
    } else {
        let lines: Vec<&str> = text.split('\n').collect();
        lines[lines.len().saturating_sub(limit)..].join("\n")
    };
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "target": target, "terminalId": terminal_id, "output": tail,
            }))
            .map_err(|e| e.to_string())?
        );
    } else {
        println!("{tail}");
    }
    Ok(())
}

/// `/terminal/` ids, run ids and names are all plain slug text — used to tell
/// a resolvable dispatch miss from a name that was never a dispatch at all.
fn is_id_like(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The sender of a `msg-*`, for `reply`/`allow`/`deny` without an explicit
/// `--to`. Single-message route first, then the snapshot's message list — the
/// same two-step fallback as slate.mjs's lookupSender.
fn lookup_sender(url: &str, token: &str, ask_id: &str) -> Option<String> {
    if let Ok(single) = request(
        url,
        token,
        "GET",
        &format!("/orchestration/messages/{}", encode(ask_id)),
        None,
    ) {
        if let Some(from) = data_of(&single)
            .get("message")
            .and_then(|m| m.get("from"))
            .and_then(Value::as_str)
        {
            return Some(from.to_owned());
        }
    }
    request(url, token, "GET", "/orchestration", None)
        .ok()?
        .get("messages")?
        .as_array()?
        .iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(ask_id))
        .and_then(|m| m.get("from"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The heartbeat slate.mjs prints while a long-poll is open: one JSON line on
/// stderr every 15s so a watching human knows the CLI is alive.
struct Beat {
    label: String,
    started: std::time::Instant,
    last: std::time::Instant,
}

impl Beat {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_owned(),
            started: std::time::Instant::now(),
            last: std::time::Instant::now() - std::time::Duration::from_secs(15),
        }
    }
    fn tick(&mut self) {
        if self.last.elapsed() >= std::time::Duration::from_secs(15) {
            self.last = std::time::Instant::now();
            eprintln!(
                "{}",
                json!({ "waiting": self.label, "seconds": self.started.elapsed().as_secs() })
            );
        }
    }
}

fn timeout_ms_flag(parsed: &slate_app::cli::Args, default: u64) -> u64 {
    parsed
        .pick(&["timeoutMs", "timeout-ms", "timeout"])
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default)
        .max(1_000)
}

/// `slate check`/`inbox` — coordinator mail. `--wait` long-polls by re-asking
/// (the pure router answers immediately rather than holding the socket), and
/// `--ack <id>` acknowledges one message in the same breath.
fn run_check(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let timeout_ms = timeout_ms_flag(parsed, 900_000);
    let wait = parsed.flag("wait");
    let mut query = String::from("/orchestration/inbox");
    let mut sep = '?';
    for (key, value) in [
        ("runId", parsed.pick(&["run", "runId"])),
        ("types", parsed.pick(&["types", "type"])),
        ("limit", parsed.pick(&["limit"])),
    ] {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            query.push_str(&format!("{sep}{key}={}", encode(&value)));
            sep = '&';
        }
    }
    if parsed.flag("all") {
        query.push_str(&format!("{sep}all=1"));
    }
    // `wait`/`timeoutMs` hand the hold to the server (the TypeScript answer
    // only came back when a message landed or the deadline passed). The
    // local loop stays as the fallback for a server that ignores them.
    if wait {
        query.push_str(&format!("{sep}wait=1&timeoutMs={timeout_ms}"));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let read_timeout = if wait {
        std::time::Duration::from_millis(timeout_ms) + std::time::Duration::from_secs(10)
    } else {
        std::time::Duration::from_secs(30)
    };
    let mut beat = Beat::new("inbox");
    let inbox = loop {
        let inbox = request_at(url, token, "GET", &query, None, read_timeout)?;
        let empty = data_of(&inbox)
            .get("messages")
            .and_then(Value::as_array)
            .is_none_or(|messages| messages.is_empty());
        if !wait || !empty || std::time::Instant::now() >= deadline {
            break inbox;
        }
        beat.tick();
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    // `--ack <id>` marks one message read right after listing it.
    if let Some(ack) = parsed.pick(&["ack"]).filter(|id| !id.is_empty()) {
        let _ = request(
            url,
            token,
            "POST",
            &format!("/orchestration/messages/{}/ack", encode(&ack)),
            None,
        );
    }
    let messages = data_of(&inbox)
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if parsed.flag("json") {
        // orc emits the inbox payload verbatim under --json.
        println!(
            "{}",
            serde_json::to_string_pretty(data_of(&inbox)).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    if !messages.is_empty() {
        for message in &messages {
            println!("{}", message_line(message));
        }
    } else if wait {
        println!("  (nothing arrived before the timeout)");
    } else {
        println!(
            "  (inbox empty — replies to `{0} tell` stay in that terminal; use `{0} worker-read <name>`)",
            prog_name()
        );
    }
    Ok(())
}

/// `slate ask` — posts the question, then blocks for the reply. The server
/// holds `wait=true` requests on `/orchestration/replies/<id>` until the
/// reply lands or `timeoutMs` passes; the client loop is the fallback for a
/// server that ignores them.
fn run_ask(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let plan = slate_app::cli::plan("ask", parsed).map_err(|e| e.to_string())?;
    let asked = request(
        url,
        token,
        "POST",
        "/orchestration/messages",
        Some(plan.body),
    )?;
    let asked = data_of(&asked).clone();
    let ask_id = as_str(&asked, "id").to_owned();
    let timeout_ms = timeout_ms_flag(parsed, 600_000);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let read_timeout =
        std::time::Duration::from_millis(timeout_ms) + std::time::Duration::from_secs(10);
    let mut beat = Beat::new(&format!("reply to {ask_id}"));
    let reply = loop {
        let found = request_at(
            url,
            token,
            "GET",
            &format!(
                "/orchestration/replies/{}?wait=1&timeoutMs={timeout_ms}",
                encode(&ask_id)
            ),
            None,
            read_timeout,
        )?;
        if let Some(reply) = found.get("reply").filter(|r| !r.is_null()) {
            break reply.clone();
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "timeout: no reply within {}s — the question is still pending as {ask_id}; \
                 resume with: {} check --wait --types reply",
                timeout_ms / 1_000,
                prog_name()
            ));
        }
        beat.tick();
        std::thread::sleep(std::time::Duration::from_secs(1));
    };
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&reply).map_err(|e| e.to_string())?
        );
    } else {
        println!(
            "reply from {}:\n{}",
            as_str(&reply, "from"),
            as_str(&reply, "body")
        );
    }
    Ok(())
}

/// `slate screenshot [worker]` — the app writes the PNG and reports the path;
/// `--out` copies it somewhere the caller chose, as slate.mjs did.
fn run_screenshot(url: &str, token: &str, parsed: &slate_app::cli::Args) -> Result<(), String> {
    let worker = parsed
        .pick(&["worker", "to", "terminal", "widget"])
        .or_else(|| parsed.positionals().get(1).cloned());
    let shot = request(
        url,
        token,
        "POST",
        "/screenshot",
        Some(object_body(vec![(
            "worker",
            worker.filter(|w| !w.is_empty()).map(Value::from),
        )])),
    )?;
    let mut shot = data_of(&shot).clone();
    if let Some(out) = parsed
        .pick(&["out", "output", "save"])
        .filter(|out| !out.trim().is_empty())
    {
        let source = as_str(&shot, "path").to_owned();
        if !source.is_empty() {
            let destination = PathBuf::from(out.trim());
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::copy(&source, &destination)
                .map_err(|e| format!("screenshot: copy to {}: {e}", destination.display()))?;
            shot["savedAs"] = Value::from(destination.to_string_lossy().into_owned());
        }
    }
    if parsed.flag("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&shot).map_err(|e| e.to_string())?
        );
    } else {
        let path = match as_str(&shot, "savedAs") {
            "" => as_str(&shot, "path"),
            saved => saved,
        };
        println!("screenshot: {path}");
    }
    Ok(())
}

/// `object` for this file: pairs with `None` values are dropped, matching the
/// cli.rs helper of the same name.
fn object_body(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        if let Some(value) = value {
            map.insert(key.to_owned(), value);
        }
    }
    Value::Object(map)
}

/// `--timeout <s>` / `--timeoutMs <ms>` parse shared by `wait` and
/// `tell --when`; seconds by default, milliseconds under the long name.
fn parsed_timeout_secs(parsed: &slate_app::cli::Args) -> Option<u64> {
    parsed.pick(&["timeout", "timeoutMs"]).map(|v| {
        if parsed.pick(&["timeoutMs"]).is_some() {
            (v.parse::<f64>().unwrap_or(300_000.0) / 1000.0) as u64
        } else {
            v.parse::<u64>().unwrap_or(300)
        }
    })
}

/// `slate wait`'s poll loop, shared with `tell --when`: GET
/// /orchestration/agents until `target` reports `want`, the deadline passes,
/// or the target vanishes. Returns the "<name> is <state>" verdict line.
fn wait_for_agent_state(
    url: &str,
    token: &str,
    target: &str,
    want: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let response = request(url, token, "GET", "/orchestration/agents", None)?;
        let agents = data_of(&response)
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let row = agents.iter().find(|a| {
            a.get("id").and_then(Value::as_str) == Some(target)
                || a.get("name").and_then(Value::as_str) == Some(target)
        });
        let Some(row) = row else {
            return Err(format!("no terminal called \"{target}\""));
        };
        let state = row
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(target)
            .to_owned();
        if state == want {
            return Ok(format!("{name} is {state}"));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("timed out waiting for {name} — still {state}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
    }
}

fn tell_body(parsed: &slate_app::cli::Args) -> Result<Value, String> {
    let mut positional = parsed.positionals().iter().skip(1);
    let to = parsed
        .pick(&["to", "worker"])
        .or_else(|| positional.next().cloned())
        .ok_or("invalid: tell needs <worker>")?;
    let text = parsed
        .pick(&["text", "message", "body"])
        .unwrap_or_else(|| positional.cloned().collect::<Vec<_>>().join(" "));
    let images = if let Some(image) = parsed.pick(&["image"]) {
        vec![image]
    } else if let Some(images) = parsed.pick(&["images"]) {
        serde_json::from_str::<Vec<String>>(&images).unwrap_or_else(|_| {
            images
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
    } else {
        Vec::new()
    };
    if text.trim().is_empty() && images.is_empty() {
        return Err("invalid: tell needs non-empty text".into());
    }
    // orc's body is `{to, text, images?}` — an empty list omits the key.
    Ok(object_body(vec![
        ("to", Some(Value::from(to))),
        ("text", Some(Value::from(text))),
        ("images", (!images.is_empty()).then(|| Value::from(images))),
    ]))
}

/// The roster line slate.mjs prints: `* ` marks your own terminal, then
/// name (id when it differs), busy/idle (+exited), agent, task and cwd —
/// whichever of those the worker record carries.
/// `slate agents` — one `state agent name (id)` row per terminal, herdr's
/// sidebar ordering: blocked first, then working, then idle.
fn format_agents(response: &Value) -> String {
    let mut rows: Vec<&Value> = data_of(response)
        .get("agents")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let rank = |row: &Value| match row.get("state").and_then(Value::as_str) {
        Some("blocked") => 0,
        Some("working") => 1,
        Some("idle") => 2,
        _ => 3,
    };
    rows.sort_by_key(|r| rank(r));
    if rows.is_empty() {
        return "no terminals".to_owned();
    }
    rows.iter()
        .map(|row| {
            let state = row
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let agent = row.get("agent").and_then(Value::as_str).unwrap_or("-");
            let name = row.get("name").and_then(Value::as_str).unwrap_or("-");
            let id = row.get("id").and_then(Value::as_str).unwrap_or("-");
            let alive = row.get("alive").and_then(Value::as_bool).unwrap_or(true);
            let dead = if alive { "" } else { " (+exited)" };
            format!("{state:<8} {agent:<12} {name} ({id}){dead}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_workers(response: &Value) -> String {
    let self_id = env::var("SLATE_TERMINAL_ID").ok();
    let age = |ms: f64| -> String {
        if !ms.is_finite() || ms < 0.0 {
            return "?".to_owned();
        }
        let s = (ms / 1000.0) as u64;
        if s < 60 {
            return format!("{s}s");
        }
        let m = s / 60;
        if m < 60 {
            return format!("{m}m");
        }
        let h = m / 60;
        if h < 48 {
            return format!("{h}h");
        }
        format!("{}d", h / 24)
    };
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    data_of(response)
        .get("workers")
        .and_then(Value::as_array)
        .map(|workers| {
            if workers.is_empty() {
                "  (no terminals open)".to_owned()
            } else {
                workers
                    .iter()
                    .map(|worker| {
                        let id = worker.get("id").and_then(Value::as_str).unwrap_or("?");
                        let name = worker.get("name").and_then(Value::as_str).unwrap_or(id);
                        let alive = worker.get("alive").and_then(Value::as_bool).unwrap_or(true);
                        let busy = worker.get("busy").and_then(Value::as_bool).unwrap_or(false);
                        // The server's `self` flag is the stamp: it is set
                        // from the request's agentId, which is the terminal's
                        // stamped identity even when TERMINAL_ID differs.
                        let is_self = worker
                            .get("self")
                            .and_then(Value::as_bool)
                            .unwrap_or(self_id.as_deref() == Some(id));
                        let mut line = format!("  {} {name}", if is_self { "*" } else { " " });
                        if name != id {
                            line.push_str(&format!(" ({id})"));
                        }
                        // The manifest detector's state wins over raw
                        // liveness when the terminal carries one — herdr's
                        // roster reads `working`/`blocked`, not `busy`.
                        let detected = worker.get("state").and_then(Value::as_str).unwrap_or("");
                        if detected.is_empty() || detected == "unknown" {
                            line.push_str(if busy { "  busy" } else { "  idle" });
                        } else {
                            line.push_str(&format!("  {detected}"));
                        }
                        if !alive {
                            line.push_str(" (exited)");
                        }
                        let tool = worker
                            .get("agent")
                            .or_else(|| worker.get("running"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !tool.is_empty() && tool != "shell" {
                            line.push_str(&format!("  [{tool}]"));
                        }
                        if let Some(task) = worker.get("taskId").and_then(Value::as_str) {
                            line.push_str(&format!("  task={task}"));
                        }
                        if let Some(title) = worker.get("taskTitle").and_then(Value::as_str) {
                            line.push_str(&format!(
                                "  \"{}\"",
                                title.chars().take(80).collect::<String>()
                            ));
                        }
                        let cwd = worker.get("cwd").and_then(Value::as_str).unwrap_or("");
                        if !cwd.is_empty() {
                            line.push_str(&format!("  {cwd}"));
                        }
                        if let Some(last_active) =
                            worker.get("lastActiveAt").and_then(Value::as_f64)
                        {
                            line.push_str(&format!("  active={} ago", age(now_ms - last_active)));
                        }
                        line
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        })
        .unwrap_or_else(|| response.to_string())
}

/// The actor the server should record: the terminal-stamped identity first,
/// else the generic `cli` — the same fallback chain as slate.mjs's AGENT_ID.
fn agent_id() -> String {
    env::var("SLATE_AGENT_ID")
        .ok()
        .or_else(|| env::var("SLATE_TERMINAL_ID").ok())
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "cli".to_owned())
}

fn request(
    base_url: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    request_at(
        base_url,
        token,
        method,
        path,
        body,
        std::time::Duration::from_secs(30),
    )
}

/// `request` with a caller-set read timeout — `--wait` calls hold the socket
/// server-side and need the client's patience to exceed it.
fn request_at(
    base_url: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    read_timeout: std::time::Duration,
) -> Result<Value, String> {
    // slate.mjs carries the caller's identity on every call: `?agentId=` on
    // reads, `agentId` in the body on writes. The inbox route 400s without it
    // and every `from`/`createdBy` field is filled from it.
    let agent = agent_id();
    let mut path = path.to_owned();
    let mut body = body;
    if method == "GET" {
        let separator = if path.contains('?') { '&' } else { '?' };
        path.push_str(&format!("{separator}agentId={}", encode(&agent)));
    } else {
        let mut map = match body.take() {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        map.entry("agentId".to_owned())
            .or_insert_with(|| Value::from(agent));
        body = Some(Value::Object(map));
    }
    let mut stream: Box<dyn Transport> = if base_url.starts_with("http://") {
        let (host, port) = parse_http_url(base_url)?;
        let stream = TcpStream::connect((host.as_str(), port))
            .map_err(|e| format!("offline: connect control server: {e}"))?;
        stream
            .set_read_timeout(Some(read_timeout))
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| e.to_string())?;
        Box::new(stream)
    } else {
        local_transport(base_url, read_timeout)?
    };
    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nx-slate-token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("connection_lost: send request: {error}"))?;
    let mut response = Vec::new();
    stream
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut response)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                format!("timeout: read response: {error}")
            }
            _ => format!("connection_lost: read response: {error}"),
        })?;
    if response.len() > 16 * 1024 * 1024 {
        return Err("Control response exceeds 16 MB".into());
    }
    parse_response(&response)
}

trait Transport: Read + Write {}
impl<T: Read + Write> Transport for T {}

#[cfg(windows)]
fn local_transport(
    path: &str,
    _read_timeout: std::time::Duration,
) -> Result<Box<dyn Transport>, String> {
    if !path.starts_with(r"\\.\pipe\") {
        return Err("Control endpoint must be a local named pipe".into());
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(|file| Box::new(file) as Box<dyn Transport>)
        .map_err(|e| format!("offline: connect control pipe: {e}"))
}

#[cfg(unix)]
fn local_transport(
    path: &str,
    read_timeout: std::time::Duration,
) -> Result<Box<dyn Transport>, String> {
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|e| format!("offline: connect control socket: {e}"))?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    Ok(Box::new(stream))
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn parse_http_url(url: &str) -> Result<(String, u16), String> {
    let authority = url
        .strip_prefix("http://")
        .ok_or_else(|| "invalid: SLATE_URL must use http://".to_owned())?
        .split('/')
        .next()
        .unwrap_or_default();
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| "invalid: SLATE_URL must include a port".to_owned())?;
    let port = port
        .parse::<u16>()
        .map_err(|error| format!("invalid control server port: {error}"))?;
    Ok((host.to_owned(), port))
}

fn parse_response(response: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(response);
    let (headers, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "invalid control server response".to_owned())?;
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("500");
    let json: Value = serde_json::from_str(body).unwrap_or_else(|_| json!({ "body": body }));
    if status.starts_with('2') {
        Ok(json)
    } else {
        // Keep the server's error code on the wire — the exit-code table in
        // main maps `invalid→2, offline→3, timeout→4, not_found→5` off the
        // `code: ` prefix, and a bare 404 is orc's `http_404`.
        let message = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("control server request failed")
            .to_owned();
        match json.get("code").and_then(Value::as_str) {
            Some(code) => Err(format!("{code}: {message}")),
            None if status == "404" => Err(format!("not_found: {message}")),
            None => Err(message),
        }
    }
}

/// A second `slate --gui` hands off to the running instance — the shell's
/// `requestSingleInstanceLock` + `second-instance` → `focusMainWindow`.
/// `dir` rides along when the invocation named a workspace path (the
/// shell's `handleSecondInstanceArgs` → `setWorkspaceDir`). `Ok` means the
/// running app acknowledged the raise; `Err` carries the refusal (a
/// headless instance has no window to raise).
pub fn raise_existing_instance(dir: Option<&str>) -> Result<(), String> {
    let (url, token) = control()?;
    request(&url, &token, "POST", "/raise", Some(json!({ "dir": dir }))).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{parse_http_url, tell_body};

    #[test]
    fn tell_preserves_positionals_and_flag_targets() {
        for args in [
            vec!["tell", "backend", "hello", "world"],
            vec!["tell", "--to", "backend", "hello world"],
            vec!["tell", "--json", "--to=backend", "hello", "world"],
            vec!["tell", "backend", "--text", "hello world"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            let body = tell_body(&slate_app::cli::Args::parse(&args)).unwrap();
            assert_eq!(body["to"], "backend");
            assert_eq!(body["text"], "hello world");
        }
    }

    #[test]
    fn tell_accepts_image_only_and_literal_flags_after_double_dash() {
        let parse = |args: &[&str]| {
            tell_body(&slate_app::cli::Args::parse(
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            ))
        };
        assert_eq!(
            parse(&["tell", "backend", "--", "--help", "please"]).unwrap()["text"],
            "--help please"
        );
        assert_eq!(
            parse(&["tell", "--to", "backend", "--image", "a b.png"]).unwrap()["images"][0],
            "a b.png"
        );
        assert!(parse(&["tell", "backend"]).is_err());
    }

    #[test]
    fn parses_loopback_url() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:1234"),
            Ok(("127.0.0.1".to_owned(), 1234))
        );
    }
}
