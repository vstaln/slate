//! The managed `<!-- BEGIN SLATE (managed) -->` block — the shell's
//! `orchestration/guide.ts::syncOrcGuide`, which kept AGENTS.md /
//! CLAUDE.md / GEMINI.md in the workspace pointed at the app's CLI so
//! dispatched agents know the control verbs without configuration.
//!
//! The block text itself is generated documentation; the managed
//! markers are what let a later run find and replace it in place.

use std::fs;
use std::path::Path;

const BEGIN: &str = "<!-- BEGIN SLATE (managed) -->";
const END: &str = "<!-- END SLATE (managed) -->";
/// Pre-rename markers in older workspaces get upgraded in place — the
/// same replacement `syncOrcGuide` did for its own tag.
const LEGACY_MARKERS: [(&str, &str); 2] = [
    (
        "<!-- BEGIN ORCSPACE (managed) -->",
        "<!-- END ORCSPACE (managed) -->",
    ),
    (
        "<!-- BEGIN GRAYSPACE (managed) -->",
        "<!-- END GRAYSPACE (managed) -->",
    ),
];

const GUIDE_FILES: [&str; 3] = ["AGENTS.md", "CLAUDE.md", "GEMINI.md"];

fn guide_body() -> String {
    [
        "<!-- BEGIN SLATE (managed) -->",
        "## Slate",
        "",
        "You are running inside Slate, a workspace shell the user is watching live.",
        "The `slate` command is already on your PATH and already authenticated — it talks",
        "to the running app directly. There is no MCP server to configure.",
        "",
        "**The other agents.** Every terminal pane is addressable by its visible",
        "name, and you can act on any of them:",
        "",
        "```sh",
        "slate whoami                                   # your own agent id, terminal & task",
        "slate workers                                  # who else is open; * marks you",
        "slate rename --to term-3 --name backend        # give one a name that means something",
        "slate tell backend \"run the tests and report\"  # type into its terminal",
        "slate worker-read backend                       # read its ordinary terminal answer",
        "```",
        "",
        "Names beat ids: rename a sibling once, then address it by name everywhere.",
        "`slate tell` does not create inbox mail. After a quick question sent with `tell`,",
        "read the answer with `slate worker-read <name>`. Use `slate check` only for messages",
        "sent through runs/tasks (`worker_done`, `ask`, `escalation`, and similar).",
        "",
        "**Good orchestration.** Before dispatching, turn the objective into a small, bounded",
        "task graph. Every task spec should state its goal, owned files or responsibility,",
        "acceptance criteria, verification command, and stop condition. Add dependencies",
        "only for real blockers; dispatch independent ready tasks in parallel, with no two",
        "workers owning the same files. The coordinator waits for reports, inspects the",
        "diff and test evidence, then releases or retains each dispatch — never treating",
        "a started process or a `tell` message as proof of completion.",
        "",
        "Use this compact task-spec shape when creating work:",
        "",
        "```text",
        "Goal: one concrete outcome",
        "Scope: files or responsibility owned by this worker",
        "Acceptance: observable conditions that must be true",
        "Verify: exact test/check to run",
        "Stop when: the acceptance criteria are met or a blocker is reported",
        "```",
        "",
        "Keep credentials, tokens, and private environment values out of task specs,",
        "mail, and reports. Use only agents currently available in the worker menu; do",
        "not invent a model or silently substitute an unavailable route.",
        "",
        "**Coordinating with other agents.** For work you intend to *wait on*, use runs,",
        "tasks and dispatches rather than `tell` — that is what gives you a completion",
        "report instead of a guess.",
        "",
        "```sh",
        "slate status                                   # what is running right now",
        "slate run-create --objective \"...\"             # open a run; check mail from this same terminal",
        "slate task-create --spec \"...\" [--deps '[\"otask-1\"]']",
        "slate task-list --ready                        # what can be dispatched now",
        "slate task-show <id>                           # view full specification and status",
        "slate worker-start --task <id> --agent opencode  # opens a terminal and briefs it",
        "slate check --wait --types worker_done,escalation,ask,permission   # block until a worker reports",
        "slate reply <askId> \"...\"                      # unblock a worker that asked",
        "slate ask --type permission --question \"...\"     # request safety approval and wait",
        "slate allow <permission-id> [--note \"...\"]      # approve a permission request",
        "slate deny <permission-id> [--reason \"...\"]     # reject a permission request",
        "slate gates                                    # check open decision gates",
        "slate worker-release <dispatchId>              # account for a finished worker",
        "```",
        "",
        "If *you* were dispatched, your preamble named your task and dispatch ids. Report",
        "exactly once when you finish, success or failure — a coordinator is blocked on it:",
        "",
        "```sh",
        "slate done --outcome succeeded --task-id <t> --dispatch-id <d> --body \"what changed\"",
        "slate ask --question \"...\"     # blocks until the coordinator answers",
        "slate escalate --body \"...\"    # you are stuck and need intervention",
        "```",
        "",
        "**Planner.** The day planner is the workspace task list.",
        "Use it to track work and report progress:",
        "",
        "```sh",
        "slate plan list                                # see all planner tasks",
        "slate plan create|update|toggle|delete [<id>]   # manage planner tasks",
        "```",
        "",
        "**The rest of the app** is the same CLI: `slate panes`, `slate plan`,",
        "`slate terminal`, `slate git`, `slate journal`. Add `--json` for parseable",
        "output. Prefer putting results in a pane or a task over loose files —",
        "the user is looking at the panes, not at your scrollback.",
        "<!-- END SLATE (managed) -->",
    ]
    .join("\n")
}

/// Sync one guide file: replace the first managed block found when stale
/// (current SLATE tag or a legacy ORCSPACE/GRAYSPACE one — upgraded in
/// place by the rename), or append the block when the file has none. A
/// missing file is created. Writes only on change.
fn sync_file(path: &Path, body: &str) {
    let existing = fs::read_to_string(path).unwrap_or_default();
    for (begin, end) in [(BEGIN, END)].into_iter().chain(LEGACY_MARKERS) {
        let (Some(b), Some(e)) = (existing.find(begin), existing.find(end)) else {
            continue;
        };
        if e < b {
            continue;
        }
        let after = e + end.len();
        if &existing[b..after] == body {
            return;
        }
        let _ = fs::write(
            path,
            format!("{}{}{}", &existing[..b], body, &existing[after..]),
        );
        return;
    }
    let trimmed = existing.trim_end();
    let sep = if trimmed.is_empty() { "" } else { "\n\n" };
    let _ = fs::write(path, format!("{trimmed}{sep}{body}\n"));
}

/// `syncOrcGuide(dir)` — stamp the managed block into the workspace's
/// three agent-guide files. Best-effort: guide files are advisory, and
/// a read-only workspace must not keep the app from starting.
pub fn sync_slate_guide(dir: &Path) {
    let body = guide_body();
    for name in GUIDE_FILES {
        sync_file(&dir.join(name), &body);
    }
}
