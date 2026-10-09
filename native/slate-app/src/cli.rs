//! `slate`: arguments in, one HTTP request out.
//!
//! Block 4. Mirrors the command surface of cli/slate.mjs. The parse is a pure
//! function so every command's request can be asserted without a server — which
//! is the only practical way to keep ~60 commands from drifting.
//!
//! Two conventions are load-bearing and are copied rather than tidied:
//!
//! * every value flag has aliases, and the *first* one present wins. `--spec`,
//!   `--brief` and `--body` all name a task's brief, because agents have been
//!   told all three at various times and breaking any of them breaks a running
//!   fleet.
//! * a value may arrive as a flag or as the first positional argument, in that
//!   order of preference, so `slate task-show otask-3` and
//!   `slate task-show --id otask-3` are the same command.
//!
//! A missing required value is an `invalid` error naming the flag, not a
//! panic — the message is what the agent reads.

use crate::command::{CommandError, CommandResult, ErrorCode};
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// What a parsed command asks the control server to do.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub method: String,
    pub path: String,
    pub query: IndexMap<String, String>,
    pub body: Value,
}

impl Plan {
    fn get(path: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            path: path.into(),
            query: IndexMap::new(),
            body: Value::Null,
        }
    }

    fn post(path: impl Into<String>, body: Value) -> Self {
        Self {
            method: "POST".into(),
            path: path.into(),
            query: IndexMap::new(),
            body,
        }
    }

    fn patch(path: impl Into<String>, body: Value) -> Self {
        Self {
            method: "PATCH".into(),
            path: path.into(),
            query: IndexMap::new(),
            body,
        }
    }

    fn delete(path: impl Into<String>) -> Self {
        Self {
            method: "DELETE".into(),
            path: path.into(),
            query: IndexMap::new(),
            body: Value::Null,
        }
    }

    fn query(mut self, key: &str, value: Option<String>) -> Self {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            self.query.insert(key.to_owned(), value);
        }
        self
    }
}

/// Flags and positionals, as parsed from the argument vector.
#[derive(Debug, Default, Clone)]
pub struct Args {
    flags: IndexMap<String, Value>,
    positional: Vec<String>,
}

impl Args {
    /// Accepts `--key value`, `--key=value` and bare `--flag` (which is `true`).
    /// A `--` ends flag parsing, so a body that starts with a dash can still be
    /// passed.
    pub fn parse(argv: &[String]) -> Self {
        let mut args = Args::default();
        let mut index = 0;
        let mut flags_over = false;

        while index < argv.len() {
            let token = &argv[index];
            if flags_over || !token.starts_with("--") {
                args.positional.push(token.clone());
                index += 1;
                continue;
            }
            if token == "--" {
                flags_over = true;
                index += 1;
                continue;
            }
            let stripped = &token[2..];
            if let Some((key, value)) = stripped.split_once('=') {
                // orc.mjs: `--no-x=v` lands as `x = (v is a negative
                // spelling)` — `--no-enter=false` evaluates to enter=true.
                if let Some(negated) = key.strip_prefix("no-") {
                    let value = value.to_ascii_lowercase();
                    args.flags.insert(
                        camel(negated),
                        Value::Bool(matches!(value.as_str(), "false" | "0" | "no")),
                    );
                } else {
                    args.flags.insert(camel(key), Value::from(value));
                }
                index += 1;
                continue;
            }
            // `--no-x` clears `x` — the same spelling slate.mjs honours for
            // `--no-enter`, `--no-inject` and friends.
            if let Some(negated) = stripped.strip_prefix("no-") {
                args.flags.insert(camel(negated), Value::Bool(false));
                index += 1;
                continue;
            }
            // orc.mjs consumes the next token as this flag's value for ANY
            // `--flag` — `slate reset --all junk` sets `all: "junk"` (and
            // reset then fails 'invalid') rather than leaving `junk` as a
            // positional. A following `--` token means bare boolean.
            let next = argv.get(index + 1);
            match next {
                Some(value) if !value.starts_with("--") => {
                    args.flags
                        .insert(camel(stripped), Value::from(value.clone()));
                    index += 2;
                }
                _ => {
                    args.flags.insert(camel(stripped), Value::Bool(true));
                    index += 1;
                }
            }
        }
        // orc.mjs normalizes after the whole pass: every flag whose value is
        // the string 'true'/'false' becomes a real boolean — `--flag=true`
        // and `--flag false` both work.
        for value in args.flags.values_mut() {
            if let Value::String(text) = value {
                match text.as_str() {
                    "true" => *value = Value::Bool(true),
                    "false" => *value = Value::Bool(false),
                    _ => {}
                }
            }
        }
        args
    }

    /// The first alias that carries a string value. A bare `--flag` reads
    /// as absent — orc's `pick` returned `true`, and every consumer that
    /// wanted a value rejected it, so surfacing `None` keeps the same
    /// contract without the literal "true" leaking into ids.
    pub fn pick(&self, aliases: &[&str]) -> Option<String> {
        for alias in aliases {
            match self.flags.get(*alias) {
                Some(Value::String(text)) if !text.is_empty() => return Some(text.clone()),
                // `--id false` normalizes to `false`, and orc accepts it as
                // the string "false" — only bare `true` is valueless.
                Some(Value::Bool(false)) => return Some("false".to_owned()),
                _ => {}
            }
        }
        None
    }

    /// Whether any alias is present but valueless — orc's require1
    /// rejects `true` before trying the positional, so a bare required
    /// flag is an error, not a fallback.
    fn has_bare(&self, aliases: &[&str]) -> bool {
        aliases
            .iter()
            .any(|alias| matches!(self.flags.get(*alias), Some(Value::Bool(true))))
    }

    pub fn flag(&self, name: &str) -> bool {
        matches!(self.flags.get(name), Some(Value::Bool(true)))
    }

    /// A flag explicitly turned off: `--no-x`, `--x=false`, `--x=0`, `--x=no`.
    /// `flag()` can't see the difference between absent and negated, and the
    /// negated spelling is how slate.mjs switches `--enter`/`--inject` off.
    pub fn flag_off(&self, name: &str) -> bool {
        match self.flags.get(name) {
            Some(Value::Bool(false)) => true,
            Some(Value::String(v)) => matches!(v.as_str(), "false" | "0" | "no"),
            _ => false,
        }
    }

    pub fn positionals(&self) -> &[String] {
        &self.positional
    }

    /// A flag, or the positional at `index` if no alias carries a value.
    fn pick_or_positional(&self, aliases: &[&str], index: usize) -> Option<String> {
        self.pick(aliases)
            .or_else(|| self.positional.get(index).cloned())
    }

    fn require(&self, aliases: &[&str], index: usize, message: &str) -> CommandResult<String> {
        if self.has_bare(aliases) {
            return Err(CommandError::new(ErrorCode::Invalid, message));
        }
        self.pick_or_positional(aliases, index)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CommandError::new(ErrorCode::Invalid, message))
    }

    /// A comma-separated flag as a list — or a JSON array, which orc's
    /// `list()` `JSON.parse`s when the value starts with `[` (that is the
    /// spelling the help text and managed block teach for `--deps`).
    /// Empty entries are dropped, so a trailing comma is harmless.
    pub fn list(&self, aliases: &[&str]) -> Vec<String> {
        let Some(raw) = self.pick(aliases) else {
            return Vec::new();
        };
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(trimmed) {
                return items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect();
            }
        }
        trimmed
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

/// orc.mjs `camel` — every `--task-id` lands as `taskId` on the flags
/// table, which is the key the verb bodies actually read.
fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = false;
    for ch in key.chars() {
        if upper && ch.is_ascii_alphanumeric() {
            out.extend(ch.to_uppercase());
            upper = false;
        } else if ch == '-' {
            upper = true;
        } else {
            out.push(ch);
            upper = false;
        }
    }
    out
}

fn object(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut map = Map::new();
    for (key, value) in pairs {
        if let Some(value) = value {
            map.insert(key.to_owned(), value);
        }
    }
    Value::Object(map)
}

fn text(value: Option<String>) -> Option<Value> {
    value.filter(|v| !v.is_empty()).map(Value::from)
}

fn list_value(values: Vec<String>) -> Option<Value> {
    if values.is_empty() {
        None
    } else {
        Some(Value::from(values))
    }
}

/// Percent-encodes an id for a path segment.
fn encode(id: &str) -> String {
    id.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Maps one command line to the request it makes.
///
/// Aliases are resolved here rather than at the call sites so that
/// `slate runs` and `slate run-list` cannot drift apart.
pub fn plan(command: &str, args: &Args) -> CommandResult<Plan> {
    match command {
        // --- runs -----------------------------------------------------------
        "run-create" => {
            let objective = args.require(
                &["objective", "o"],
                1,
                "run-create needs --objective \"...\"",
            )?;
            Ok(Plan::post(
                "/orchestration/runs",
                object(vec![("objective", Some(Value::from(objective)))]),
            ))
        }
        "run-list" | "runs" => Ok(Plan::get("/orchestration/runs")),
        "run-show" | "run" => {
            let id = args.require(&["id", "run", "runId"], 1, "run-show needs <run-id>")?;
            Ok(Plan::get(format!("/orchestration/runs/{}", encode(&id))))
        }
        "run-close" => {
            // Destructive in slate.mjs too, but unlike canvas/terminal close
            // the require1 fires BEFORE the confirm gate — a missing id is
            // `invalid`, not `needs_confirm`.
            let id = args.require(&["id", "run", "runId"], 1, "run-close needs <run-id>")?;
            if !args.flag("yes") {
                return Err(CommandError::new(
                    ErrorCode::NeedsConfirm,
                    format!(" destructive: run-close {id} — add --yes to confirm"),
                ));
            }
            Ok(Plan::post(
                format!("/orchestration/runs/{}/close", encode(&id)),
                Value::Object(Map::new()),
            ))
        }

        // --- terminals ------------------------------------------------------
        "terminal" | "term" => {
            let Some(action) = args.positionals().get(1).map(String::as_str) else {
                // orc treats a bare `orc terminal` like any other unknown
                // action rather than implying `list` — code `invalid`.
                return Err(CommandError::new(
                    ErrorCode::Invalid,
                    "terminal: unknown action \"undefined\"",
                ));
            };
            match action {
                "list" | "ls" => Ok(Plan::get("/orchestration/workers")),
                "new" | "spawn" | "open" => Ok(Plan::post(
                    // orc spelled it /widgets/terminal — the control server
                    // aliases both spellings to the same create.
                    "/widgets/terminal",
                    object(vec![
                        // `title` only — `name` is a serde alias on the same
                        // field, so sending both is a duplicate-field 400.
                        ("title", text(args.pick(&["title", "name"]))),
                        ("cwd", text(args.pick(&["cwd", "dir", "directory"]))),
                        ("command", text(args.pick(&["command", "cmd"]))),
                        ("id", text(args.pick(&["id"]))),
                    ]),
                )),
                "send" | "write" => {
                    let id = args.require(
                        &["to", "id", "terminal"],
                        2,
                        "terminal send needs <id-or-name>",
                    )?;
                    let text = args
                        .pick(&["text", "command", "body", "message"])
                        .or_else(|| {
                            let rest: Vec<String> =
                                args.positionals().iter().skip(3).cloned().collect();
                            (!rest.is_empty()).then(|| rest.join(" "))
                        })
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| {
                            CommandError::new(
                                ErrorCode::Invalid,
                                "terminal send needs text to send",
                            )
                        })?;
                    // slate.mjs: pressEnter and confirmDelivery are on unless the
                    // negated spellings (--no-enter, --no-confirm) switch them off.
                    let press_enter = !args.flag_off("enter");
                    let confirm_delivery =
                        !args.flag_off("confirm") && !args.flag_off("confirmDelivery");
                    Ok(Plan::post(
                        format!("/terminal/{}/write", encode(&id)),
                        object(vec![
                            ("text", Some(Value::from(text))),
                            ("pressEnter", Some(Value::Bool(press_enter))),
                            ("confirmDelivery", Some(Value::Bool(confirm_delivery))),
                        ]),
                    ))
                }
                "read" | "output" | "cat" => {
                    let id = args.require(
                        &["to", "id", "terminal"],
                        2,
                        "terminal read needs <id-or-name>",
                    )?;
                    // orc sends `full` only when asked; the bare read is the
                    // server's incremental window with `clear`/`offset`/`limit`.
                    Ok(Plan::get(format!("/terminal/{}/output", encode(&id)))
                        .query("full", args.flag("full").then(|| "1".to_owned()))
                        .query("clear", args.flag("clear").then(|| "1".to_owned()))
                        .query("offset", args.pick(&["offset"]))
                        .query("limit", args.pick(&["limit"])))
                }
                "rename" => {
                    let id = args.require(
                        &["to", "id", "terminal"],
                        2,
                        "terminal rename needs <id-or-name>",
                    )?;
                    let name = args.require(
                        &["name", "as", "title"],
                        3,
                        "terminal rename needs <name> or --name <name>",
                    )?;
                    Ok(Plan::post(
                        format!("/terminal/{}/rename", encode(&id)),
                        object(vec![("name", Some(Value::from(name)))]),
                    ))
                }
                "close" | "kill" => {
                    // Destructive, same gate as `canvas close`: kills the PTY —
                    // and the gate fires before the id lookup, like orc.
                    if !args.flag("yes") {
                        let label = args
                            .pick(&["to", "id", "terminal"])
                            .or_else(|| args.positionals().get(2).cloned())
                            .unwrap_or_default();
                        return Err(CommandError::new(
                            ErrorCode::NeedsConfirm,
                            format!(" destructive: terminal close {label} — add --yes to confirm"),
                        ));
                    }
                    let id = args.require(
                        &["to", "id", "terminal"],
                        2,
                        "terminal close needs <id-or-name>",
                    )?;
                    // orc deletes the WIDGET, which disposes the PTY along
                    // with it — `DELETE /terminal/:id` would leave the canvas
                    // widget orphaned.
                    let mut plan = Plan::delete(format!("/widgets/{}", encode(&id)));
                    plan.body = serde_json::json!({
                        "agentId": std::env::var("SLATE_AGENT_ID")
                            .ok()
                            .or_else(|| std::env::var("SLATE_TERMINAL_ID").ok())
                            .filter(|id| !id.is_empty())
                            .unwrap_or_else(|| "cli".to_owned())
                    });
                    Ok(plan)
                }
                other => Err(CommandError::new(
                    ErrorCode::UnknownCommand,
                    format!(
                    "unknown terminal subcommand \"{other}\" — try list|open|send|read|rename|close"
                ),
                )),
            }
        }
        "rename" => {
            // The worker endpoint — `/orchestration/workers/rename` — is the
            // one that also commits `widget.update`, keeping the canvas title
            // in step with the worker name. `/terminal/{id}/rename` only
            // touches the terminal table.
            let to = args.require(
                &["to", "worker", "id", "terminal"],
                1,
                "rename needs --to <terminal>",
            )?;
            let name = args.require(&["name", "as", "title"], 2, "rename needs --name <name>")?;
            Ok(Plan::post(
                "/orchestration/workers/rename",
                object(vec![
                    ("to", Some(Value::from(to))),
                    ("name", Some(Value::from(name))),
                ]),
            ))
        }

        // --- tasks ----------------------------------------------------------
        "task-create" => {
            let spec = args.require(&["spec", "brief", "body"], 1, "task-create needs <spec>")?;
            Ok(Plan::post(
                "/orchestration/tasks",
                object(vec![
                    ("spec", Some(Value::from(spec))),
                    ("title", text(args.pick(&["taskTitle", "title"]))),
                    ("deps", list_value(args.list(&["deps", "dep"]))),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }
        "task-list" | "tasks" => Ok(Plan::get("/orchestration/tasks")
            .query("runId", args.pick(&["run", "runId"]))
            .query("status", args.pick(&["status"]))
            .query("ready", args.flag("ready").then(|| "1".to_owned()))),
        "task-show" | "task" => {
            let id = args.require(&["id", "task", "taskId"], 1, "task-show needs <task-id>")?;
            Ok(Plan::get(format!("/orchestration/tasks/{}", encode(&id))))
        }
        "task-update" => {
            let id = args.require(&["id", "task", "taskId"], 1, "task-update needs <task-id>")?;
            Ok(Plan::patch(
                format!("/orchestration/tasks/{}", encode(&id)),
                object(vec![
                    ("status", text(args.pick(&["status"]))),
                    ("title", text(args.pick(&["taskTitle", "title"]))),
                    ("spec", text(args.pick(&["spec", "brief"]))),
                ]),
            ))
        }

        // --- dispatches -----------------------------------------------------
        "worker-start" | "dispatch" => {
            // slate.mjs splits the second positional: `dispatch` takes it as
            // the target terminal, `worker-start` as the agent name.
            let task = args.require(
                &["task", "taskId", "id"],
                1,
                &format!("{command} needs <task-id>"),
            )?;
            let terminal = args.pick(&["to", "terminal", "terminalId"]).or_else(|| {
                (command == "dispatch")
                    .then(|| args.positionals().get(2).cloned())
                    .flatten()
            });
            let agent = args.pick(&["agent"]).or_else(|| {
                (command == "worker-start")
                    .then(|| args.positionals().get(2).cloned())
                    .flatten()
            });
            // --no-inject keeps the dispatch record but leaves the preamble
            // for the caller to type — a flag the server honours.
            let inject = args.flag_off("inject").then(|| Value::Bool(false));
            Ok(Plan::post(
                "/orchestration/dispatches",
                object(vec![
                    ("taskId", Some(Value::from(task))),
                    ("terminalId", text(terminal)),
                    ("agent", text(agent)),
                    ("command", text(args.pick(&["command", "cmd"]))),
                    ("inject", inject),
                ]),
            ))
        }
        "worker-release" | "worker-retain" => {
            let id = args.require(
                &["dispatch", "dispatchId", "id"],
                1,
                &format!("{command} needs <dispatch-id>"),
            )?;
            let state = if command == "worker-retain" {
                "retained"
            } else {
                "released"
            };
            Ok(Plan::post(
                format!("/orchestration/dispatches/{}/account", encode(&id)),
                object(vec![
                    ("state", Some(Value::from(state))),
                    // orc sends the key unconditionally — `flags.close ===
                    // true` — so an absent --close lands as explicit false.
                    ("closeTerminal", Some(Value::Bool(args.flag("close")))),
                ]),
            ))
        }
        "worker-show" => {
            let id = args.require(
                &["dispatch", "dispatchId", "id"],
                1,
                "worker-show needs <dispatch-id>",
            )?;
            Ok(Plan::get(format!(
                "/orchestration/dispatches/{}",
                encode(&id)
            )))
        }
        // slate.mjs `dispatch-show` lists dispatches (optionally for one
        // task); `worker-show <id>` inspects a single one.
        "dispatch-show" | "dispatches" => Ok(Plan::get("/orchestration/dispatches")
            .query("runId", args.pick(&["run", "runId"]))
            .query(
                "taskId",
                args.pick(&["task", "taskId"])
                    .or_else(|| args.positionals().get(1).cloned()),
            )),

        // --- reporting ------------------------------------------------------
        "done" => {
            let outcome = args.require(&["outcome"], 1, "done needs --outcome succeeded|failed")?;
            if outcome != "succeeded" && outcome != "failed" {
                return Err(CommandError::new(
                    ErrorCode::Invalid,
                    "done --outcome must be \"succeeded\" or \"failed\"",
                ));
            }
            Ok(Plan::post(
                "/orchestration/messages",
                object(vec![
                    ("type", Some(Value::from("worker_done"))),
                    ("outcome", Some(Value::from(outcome))),
                    ("to", text(args.pick(&["to"]))),
                    ("subject", text(args.pick(&["subject"]))),
                    ("taskId", text(args.pick(&["task", "taskId"]))),
                    ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                    ("body", text(args.pick(&["body", "message"]))),
                    (
                        "filesModified",
                        list_value(args.list(&["files", "filesModified"])),
                    ),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }
        "escalate" => Ok(Plan::post(
            "/orchestration/messages",
            object(vec![
                ("type", Some(Value::from("escalation"))),
                ("to", text(args.pick(&["to"]))),
                ("subject", text(args.pick(&["subject"]))),
                ("body", text(args.pick(&["body", "message"]))),
                ("taskId", text(args.pick(&["task", "taskId"]))),
                ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                ("runId", text(args.pick(&["run", "runId"]))),
            ]),
        )),
        "heartbeat" => Ok(Plan::post(
            "/orchestration/messages",
            object(vec![
                ("type", Some(Value::from("heartbeat"))),
                ("to", text(args.pick(&["to"]))),
                ("subject", text(args.pick(&["subject"]))),
                ("body", text(args.pick(&["body", "message"]))),
                ("taskId", text(args.pick(&["task", "taskId"]))),
                ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                ("runId", text(args.pick(&["run", "runId"]))),
            ]),
        )),
        "ask" => {
            let question = args.require(
                &["question", "body", "q"],
                1,
                "ask needs --question \"...\"",
            )?;
            // `--type permission` reuses this path; the default subject differs
            // so a permission request reads as one in an inbox listing.
            let message_type = args.pick(&["type"]).unwrap_or_else(|| "ask".to_owned());
            let default_subject = if message_type == "permission" {
                "permission_request"
            } else {
                "question"
            };
            Ok(Plan::post(
                "/orchestration/messages",
                object(vec![
                    ("type", Some(Value::from(message_type))),
                    ("body", Some(Value::from(question))),
                    (
                        "subject",
                        Some(Value::from(
                            args.pick(&["subject"])
                                .unwrap_or_else(|| default_subject.to_owned()),
                        )),
                    ),
                    ("options", list_value(args.list(&["options"]))),
                    ("to", text(args.pick(&["to"]))),
                    ("taskId", text(args.pick(&["task", "taskId"]))),
                    ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }
        "reply" => {
            // `to` is filled by the runner from the question's sender when the
            // caller does not name one — the same `lookupSender` slate.mjs
            // performs.
            let ask_id = args.require(
                &["id", "replyTo", "message", "askId"],
                1,
                "reply needs <ask-id>",
            )?;
            // `to` is the recipient flag, not an ask-id alias — keeping it
            // out of the require list stops `--to bob` resolving as the
            // message being answered. orc also allowed an empty-body reply.
            let body = args
                .pick(&["body", "text", "message"])
                .or_else(|| args.positionals().get(2).cloned())
                .unwrap_or_default();
            Ok(Plan::post(
                "/orchestration/messages",
                object(vec![
                    ("type", Some(Value::from("reply"))),
                    ("replyTo", Some(Value::from(ask_id))),
                    ("to", text(args.pick(&["to"]))),
                    (
                        "subject",
                        Some(Value::from(
                            args.pick(&["subject"])
                                .unwrap_or_else(|| "reply".to_owned()),
                        )),
                    ),
                    // orc's empty reply omits the key rather than sending "".
                    ("body", text(Some(body))),
                    ("taskId", text(args.pick(&["task", "taskId"]))),
                    ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }
        // slate.mjs requires a message type and lets `to` stay empty for a
        // broadcast; it is `send --type note --to worker "body"`.
        "send" | "mail" | "msg" => {
            let message_type = args.require(&["type"], 1, "send needs --type <type>")?;
            Ok(Plan::post(
                "/orchestration/messages",
                object(vec![
                    ("type", Some(Value::from(message_type))),
                    ("to", text(args.pick(&["to"]))),
                    ("subject", text(args.pick(&["subject"]))),
                    ("body", text(args.pick(&["body", "message"]))),
                    ("taskId", text(args.pick(&["task", "taskId"]))),
                    ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }
        "ack" => {
            let id = args.require(&["id", "message", "messageId"], 1, "ack needs <message-id>")?;
            Ok(Plan::post(
                format!("/orchestration/messages/{}/ack", encode(&id)),
                Value::Object(Map::new()),
            ))
        }

        // --- inbox ----------------------------------------------------------
        "check" | "inbox" => Ok(Plan::get("/orchestration/inbox")
            .query("runId", args.pick(&["run", "runId"]))
            .query("types", args.pick(&["types", "type"]))
            .query("wait", args.flag("wait").then(|| "1".to_owned()))
            .query("all", args.flag("all").then(|| "1".to_owned()))
            .query("limit", args.pick(&["limit"]))),

        // --- gates ----------------------------------------------------------
        "gate-create" => {
            let question = args.require(
                &["question", "q"],
                1,
                "gate-create needs --question \"...\"",
            )?;
            Ok(Plan::post(
                "/orchestration/gates",
                object(vec![
                    ("question", Some(Value::from(question))),
                    ("taskId", text(args.pick(&["task", "taskId"]))),
                    ("options", list_value(args.list(&["options"]))),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }
        "gate-list" | "gates" => Ok(Plan::get("/orchestration/gates")
            .query("runId", args.pick(&["run", "runId"]))
            .query("open", args.flag("open").then(|| "1".to_owned()))),
        "gate-resolve" => {
            let id = args.require(&["id", "gate", "gateId"], 1, "gate-resolve needs <gate-id>")?;
            let resolution = args.require(
                &["resolution", "answer"],
                2,
                "gate-resolve needs --resolution \"...\"",
            )?;
            Ok(Plan::post(
                format!("/orchestration/gates/{}/resolve", encode(&id)),
                object(vec![("resolution", Some(Value::from(resolution)))]),
            ))
        }

        // --- permissions ----------------------------------------------------
        // Three spellings each, because agents have been told all of them.
        // Both are a `reply` message addressed to the asker (the runner looks
        // the sender up when --to is absent) with a `permission_*` subject —
        // the subject is what the blocked worker matches on, so the exact
        // spellings from slate.mjs matter.
        "allow" | "approve" | "permit" | "deny" | "reject" | "refuse" => {
            let id = args.require(
                &["id", "permission"],
                1,
                &format!("{command} needs <permission-id>"),
            )?;
            let granted = matches!(command, "allow" | "approve" | "permit");
            let verdict = if granted { "allow" } else { "deny" };
            let body = match args.pick(&["note", "reason", "body"]) {
                Some(detail) if !detail.is_empty() => format!("{verdict}: {detail}"),
                _ => verdict.to_owned(),
            };
            Ok(Plan::post(
                "/orchestration/messages",
                object(vec![
                    ("type", Some(Value::from("reply"))),
                    ("replyTo", Some(Value::from(id))),
                    ("to", text(args.pick(&["to"]))),
                    (
                        "subject",
                        Some(Value::from(if granted {
                            "permission_granted"
                        } else {
                            "permission_denied"
                        })),
                    ),
                    ("body", Some(Value::from(body))),
                    // orc's `sendMessage` attaches the linkage fields on
                    // every send-verb; a permission answer without them
                    // can't be traced back to the task it unblocked.
                    ("taskId", text(args.pick(&["task", "taskId"]))),
                    ("dispatchId", text(args.pick(&["dispatch", "dispatchId"]))),
                    ("runId", text(args.pick(&["run", "runId"]))),
                ]),
            ))
        }

        // --- app & escape hatches --------------------------------------------
        "reset" => {
            // The destructive-confirm contract from slate.mjs: nothing happens
            // without --yes plus at least one scope flag.
            if !args.flag("yes") {
                return Err(CommandError::new(
                    ErrorCode::NeedsConfirm,
                    " destructive: reset — add --yes to confirm",
                ));
            }
            let (tasks, messages, all) =
                (args.flag("tasks"), args.flag("messages"), args.flag("all"));
            if !tasks && !messages && !all {
                return Err(CommandError::new(
                    ErrorCode::Invalid,
                    "reset needs --tasks, --messages or --all",
                ));
            }
            Ok(Plan::post(
                "/orchestration/reset",
                // orc posts all three keys — `flags.x === true` — so an
                // unset scope is an explicit false, not an absent key.
                object(vec![
                    ("tasks", Some(Value::Bool(tasks))),
                    ("messages", Some(Value::Bool(messages))),
                    ("all", Some(Value::Bool(all))),
                ]),
            ))
        }
        "api" => {
            // The escape hatch: `slate api POST /terminal '{"id":"x"}'` reaches
            // any route — including ones this CLI has no verb for yet. A first
            // argument that is already a path means GET.
            let (method, path_index) = match args.positionals().get(1).map(String::as_str) {
                Some(first) if first.starts_with('/') => ("GET".to_owned(), 1),
                Some(method) => (method.to_uppercase(), 2),
                None => ("GET".to_owned(), 2),
            };
            let path = args
                .pick(&["path"])
                .or_else(|| args.positionals().get(path_index).cloned())
                .ok_or_else(|| {
                    CommandError::new(
                        ErrorCode::Invalid,
                        "api needs a path, e.g. slate api GET /health",
                    )
                })?;
            let path = if path.starts_with('/') {
                path
            } else {
                format!("/{path}")
            };
            let body = match args
                .pick(&["body", "data"])
                .or_else(|| args.positionals().get(path_index + 1).cloned())
            {
                Some(raw) => serde_json::from_str::<Value>(&raw).map_err(|_| {
                    CommandError::new(ErrorCode::Invalid, "api: body must be valid JSON")
                })?,
                None => Value::Null,
            };
            // orc stamps `{agentId: AGENT_ID, ...body}` on every non-GET.
            let body = if method == "GET" || body.is_null() {
                body
            } else {
                let mut merged = serde_json::Map::new();
                merged.insert(
                    "agentId".to_owned(),
                    Value::from(
                        std::env::var("SLATE_AGENT_ID")
                            .ok()
                            .or_else(|| std::env::var("SLATE_TERMINAL_ID").ok())
                            .filter(|id| !id.is_empty())
                            .unwrap_or_else(|| "cli".to_owned()),
                    ),
                );
                if let Some(map) = body.as_object() {
                    for (key, value) in map {
                        merged.insert(key.clone(), value.clone());
                    }
                }
                Value::Object(merged)
            };
            Ok(Plan {
                method,
                path,
                query: IndexMap::new(),
                body,
            })
        }
        "screenshot" | "shot" => Ok(Plan::post(
            "/screenshot",
            object(vec![(
                "worker",
                text(
                    args.pick(&["worker", "to", "terminal", "widget"])
                        .or_else(|| args.positionals().get(1).cloned()),
                ),
            )]),
        )),

        // --- status ---------------------------------------------------------
        "status" | "st" => {
            Ok(Plan::get("/orchestration").query("runId", args.pick(&["run", "runId"])))
        }
        "version" => Ok(Plan::get("/health")),

        _ => {
            let hint = suggest_command(command);
            Err(CommandError::new(
                ErrorCode::UnknownCommand,
                format!(
                    "unknown command \"{command}\"{} — see `--help`",
                    hint.map(|h| format!(" — did you mean \"{h}\"?"))
                        .unwrap_or_default()
                ),
            ))
        }
    }
}

/// Every command `plan` can be called with, for the did-you-mean hint.
/// Mirrors `KNOWN_COMMANDS` in slate.mjs (aliases included).
const KNOWN_COMMANDS: &[&str] = &[
    "whoami",
    "context",
    "ctx",
    "workers",
    "who",
    "ps",
    "rename",
    "tell",
    "status",
    "st",
    "run-create",
    "run-list",
    "runs",
    "run-show",
    "run",
    "run-close",
    "task-create",
    "task-list",
    "tasks",
    "task-show",
    "task",
    "task-update",
    "worker-start",
    "dispatch",
    "worker-show",
    "worker-read",
    "logs",
    "tail",
    "worker-release",
    "worker-retain",
    "dispatch-show",
    "dispatches",
    "send",
    "mail",
    "msg",
    "done",
    "escalate",
    "heartbeat",
    "ask",
    "reply",
    "ack",
    "allow",
    "approve",
    "permit",
    "deny",
    "reject",
    "refuse",
    "check",
    "inbox",
    "gate-create",
    "gate-list",
    "gates",
    "gate-resolve",
    "plan",
    "canvas",
    "terminal",
    "term",
    "git",
    "journal",
    "reset",
    "doctor",
    "api",
    "version",
    "help",
    "browser",
    "screenshot",
    "shot",
];

/// True when `word` is a real command or alias — `slate <path>` uses this
/// so a directory named `canvas` in cwd does not hijack the verb.
pub fn is_known_command(word: &str) -> bool {
    KNOWN_COMMANDS.contains(&word)
}

/// Levenshtein distance, capped at a best match — the same "did you mean"
/// slate.mjs prints for a mistyped command.
pub fn suggest_command(unknown: &str) -> Option<&'static str> {
    let mut best = None;
    let mut best_dist = 4;
    for &known in KNOWN_COMMANDS {
        let d = edit_distance(unknown, known);
        if d < best_dist {
            best_dist = d;
            best = Some(known);
        }
    }
    best
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}
