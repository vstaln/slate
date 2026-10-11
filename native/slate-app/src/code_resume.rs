//! Conversations the CLI agents keep on disk, so a Code session can be picked
//! up where it left off instead of starting cold after a restart — the port
//! of orcspace's `src/main/agentSessions.ts` (the scanner) and
//! `src/renderer/src/lib/agentConversations.ts` (the list/dismissal logic),
//! collapsed to one sync module. The view calls [`list`] on a background
//! executor.
//!
//! Every agent stores its own history in its own shape; nothing here writes
//! to those stores, and a store that is missing, unreadable or in a shape we
//! do not recognise yields nothing rather than an error. Dismissals persist
//! in `user_data_dir()/code-resume-dismissed.json` — the original's
//! `orcspace:code-resume-dismissed:<dir>` localStorage keys.

use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The agent's own conversation id — what its resume flag takes — plus the
/// fields a row draws.
#[derive(Clone, Debug)]
pub struct AgentConversation {
    pub id: String,
    /// Matches `CodeAgent.id` in the view, so the row can show its icon.
    pub agent_id: &'static str,
    /// First human message of the conversation, or "" when none was found.
    pub title: String,
    /// Last activity, ms since epoch.
    pub updated_at: u64,
    /// Exact command that resumes it.
    pub command: String,
}

/// `<agent>:<id>` — one key per conversation for select/dismiss sets.
pub fn conversation_key(agent_id: &str, id: &str) -> String {
    format!("{agent_id}:{id}")
}

/// As many as the Code view can hold, so a restore that reopens a full board
/// still has a conversation to offer every terminal.
const DEFAULT_LIMIT: usize = 32;
/// Enough of a transcript to find its first human message.
const HEAD_BYTES: u64 = 192 * 1024;
/// Second look for a transcript whose opening is all preamble.
const DEEP_TITLE_BYTES: u64 = 512 * 1024;
/// Codex opens a rollout with its full instructions: the folder is known
/// early, the first typed prompt can sit well past that.
const CODEX_META_BYTES: u64 = 48 * 1024;
const CODEX_TITLE_BYTES: u64 = 384 * 1024;
/// Antigravity's interleaved prompt history is read from the tail.
const HISTORY_TAIL_BYTES: u64 = 2 * 1024 * 1024;
/// Codex keeps one folder-agnostic pile of rollouts; the budget stops a
/// machine with thousands of unrelated sessions from being walked in full.
const CODEX_FILE_BUDGET: usize = 150;
const GROK_SUMMARY_BYTES: u64 = 64 * 1024;
/// How many session summaries the group search may open when no directory
/// name matches this folder.
const GROK_GROUP_PROBE_BUDGET: i32 = 40;
const TITLE_MAX: usize = 140;

/// Conversation ids land in a shell command, so keep them boring:
/// `[A-Za-z0-9][A-Za-z0-9._-]{7,127}`.
fn safe_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    (8..=128).contains(&id.len())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `<…`, `#… `, `caveat:` — prompts a CLI injects on the user's behalf. Real
/// `user` messages in the transcript, but they say nothing about what the
/// conversation was about.
fn machine_title(text: &str) -> bool {
    text.starts_with('<')
        || text.starts_with('#')
        || text.len() >= 7 && text[..7].eq_ignore_ascii_case("caveat:")
}

/// A typed slash command — worth showing when it is all a conversation has.
fn weak_title(text: &str) -> bool {
    text.starts_with('/')
}

struct TitleCandidate {
    text: String,
    weak: bool,
}

fn candidate_title(raw: &str) -> Option<TitleCandidate> {
    let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.trim();
    if text.is_empty() || machine_title(text) {
        return None;
    }
    let text = if text.chars().count() > TITLE_MAX {
        let mut t: String = text.chars().take(TITLE_MAX - 1).collect();
        t.push('…');
        t
    } else {
        text.to_owned()
    };
    Some(TitleCandidate {
        weak: weak_title(&text),
        text,
    })
}

/// Keeps the first strong title seen, and the first weak one as a fallback.
#[derive(Default)]
struct TitlePicker {
    strong: String,
    weak: String,
}

impl TitlePicker {
    /// Returns true when a strong title landed — the caller can stop reading.
    fn offer(&mut self, candidate: Option<TitleCandidate>) -> bool {
        let Some(candidate) = candidate else { return false };
        if candidate.weak {
            if self.weak.is_empty() {
                self.weak = candidate.text;
            }
            return false;
        }
        if self.strong.is_empty() {
            self.strong = candidate.text;
        }
        true
    }

    fn value(&self) -> String {
        if self.strong.is_empty() {
            self.weak.clone()
        } else {
            self.strong.clone()
        }
    }
}

/// Seconds since 1970 (10 digits) — anything below this is not a timestamp
/// in ms; a history that records seconds would date everything to 1970.
fn epoch_ms(value: &Value) -> u64 {
    let Some(v) = value.as_f64() else { return 0 };
    if !v.is_finite() || v <= 0.0 {
        return 0;
    }
    (if v < 100_000_000_000.0 { v * 1000.0 } else { v }).round() as u64
}

/// `2026-09-14 14:29:24.9264321+00:00` → ms since epoch. Antigravity/Grok
/// write SQL-shaped stamps: a space instead of the `T`, and more fractional
/// digits than needed. Days-from-civil does the calendar without a date
/// dependency.
fn parse_sql_time(value: &Value) -> u64 {
    if value.as_f64().is_some() {
        return epoch_ms(value);
    }
    let Some(raw) = value.as_str() else { return 0 };
    let text = raw.trim().replace(' ', "T");
    let mut parts = text.splitn(2, 'T');
    let (Some(date), Some(time)) = (parts.next(), parts.next()) else {
        return 0;
    };
    let d: Vec<i64> = date.split('-').filter_map(|p| p.parse().ok()).collect();
    if d.len() != 3 {
        return 0;
    }
    // Strip the zone suffix (+hh:mm / -hh:mm / Z) before the clock split.
    let time = time.split(['+', 'Z']).next().unwrap_or("");
    let time = match time.rfind('-') {
        // A "-" this late is a negative zone offset, not the time itself.
        Some(at) if at > 5 => &time[..at],
        _ => time,
    };
    let t: Vec<f64> = time.split(':').filter_map(|p| p.parse().ok()).collect();
    if t.len() < 2 {
        return 0;
    }
    // Howard Hinnant's days_from_civil.
    let (y, m, day) = (d[0], d[1], d[2]);
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = t[0] * 3600.0 + t[1] * 60.0 + t.get(2).copied().unwrap_or(0.0);
    let ms = days * 86_400_000 + (secs * 1000.0) as i64;
    if ms > 0 { ms as u64 } else { 0 }
}

/// Linux/Android paths are case-sensitive — the original's case-folding only
/// ran on win32/darwin. Trailing slashes and `.`/`..` segments normalize out.
fn same_path(a: &str, b: &str) -> bool {
    fn normalize(value: &str) -> String {
        let mut out: Vec<&str> = Vec::new();
        for part in value.split(['/', '\\']) {
            match part {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                _ => out.push(part),
            }
        }
        out.join("/")
    }
    !a.is_empty() && !b.is_empty() && normalize(a) == normalize(b)
}

/// Claude Code names a project folder after the path, every other character
/// replaced: `/home/u/my app` → `-home-u-my-app`.
fn claude_project_slug(dir: &str) -> String {
    let value = dir.trim().trim_end_matches(['/', '\\']);
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The same rule with letters and digits of any script kept, for a Claude
/// build that does not flatten a non-ASCII path.
fn claude_project_slug_unicode(dir: &str) -> String {
    let value = dir.trim().trim_end_matches(['/', '\\']);
    value
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect()
}

/// Head or tail `bytes` of `file`, cut on line boundaries — a partial line
/// at the cut end is dropped rather than parsed wrong.
fn read_chunk(file: &Path, bytes: u64, from_end: bool) -> String {
    let Ok(mut handle) = std::fs::File::open(file) else {
        return String::new();
    };
    let size = handle.metadata().map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return String::new();
    }
    let length = size.min(bytes);
    let mut buffer = vec![0u8; length as usize];
    if from_end
        && handle.seek(SeekFrom::End(-(length as i64))).is_err() {
            return String::new();
        }
    let mut filled = 0usize;
    while filled < buffer.len() {
        match handle.read(&mut buffer[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    let mut text = String::from_utf8_lossy(&buffer[..filled]).into_owned();
    if filled as u64 >= size {
        return text;
    }
    if from_end {
        return match text.find('\n') {
            Some(at) => text.split_off(at + 1),
            None => String::new(),
        };
    }
    match text.rfind('\n') {
        Some(at) => {
            text.truncate(at + 1);
            text
        }
        None => String::new(),
    }
}

/// `{`-lines parsed lazily — a parser that finds what it needs in the first
/// few lines never pays for the rest of a half-megabyte transcript.
fn json_lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.lines().filter_map(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            return None;
        }
        serde_json::from_str::<Value>(trimmed)
            .ok()
            .filter(|v| v.is_object())
    })
}

/// `{ message: { content } }` is a string in older transcripts and a part
/// list in newer ones.
fn message_candidate(content: Option<&Value>) -> Option<TitleCandidate> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return candidate_title(text);
    }
    let parts = content.as_array()?;
    let mut picker = TitlePicker::default();
    for part in parts {
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "text" && kind != "input_text" {
            continue;
        }
        if picker.offer(part.get("text").and_then(Value::as_str).and_then(candidate_title)) {
            break;
        }
    }
    let value = picker.value();
    if value.is_empty() {
        None
    } else {
        Some(TitleCandidate {
            weak: weak_title(&value),
            text: value,
        })
    }
}

/// The first strong user message in a Claude transcript; slash commands
/// only stand in when nothing else was ever typed.
fn parse_claude_title(head: &str) -> String {
    let mut picker = TitlePicker::default();
    for entry in json_lines(head) {
        if entry.get("type").and_then(Value::as_str) != Some("user")
            || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let content = entry.get("message").and_then(|m| m.get("content"));
        if picker.offer(message_candidate(content)) {
            break;
        }
    }
    picker.value()
}

/// The folder a Claude transcript was recorded in — two paths can flatten to
/// the same project-folder name, so the transcript's own `cwd` decides.
fn parse_claude_cwd(head: &str) -> String {
    for entry in json_lines(head) {
        if let Some(cwd) = entry.get("cwd").and_then(Value::as_str) {
            if !cwd.is_empty() {
                return cwd.to_owned();
            }
        }
    }
    String::new()
}

fn parse_codex_meta(head: &str) -> Option<(String, String)> {
    for entry in json_lines(head) {
        if entry.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let payload = entry.get("payload")?;
        let id = payload.get("session_id").and_then(Value::as_str).unwrap_or("");
        let cwd = payload.get("cwd").and_then(Value::as_str).unwrap_or("");
        if !safe_id(id) || cwd.is_empty() {
            return None;
        }
        return Some((id.to_owned(), cwd.to_owned()));
    }
    None
}

fn parse_codex_title(head: &str) -> String {
    let mut picker = TitlePicker::default();
    for entry in json_lines(head) {
        let Some(payload) = entry.get("payload") else { continue };
        if payload.get("type").and_then(Value::as_str) != Some("message")
            || payload.get("role").and_then(Value::as_str) != Some("user")
        {
            continue;
        }
        if picker.offer(message_candidate(payload.get("content"))) {
            break;
        }
    }
    picker.value()
}

/// One history file holds every antigravity conversation interleaved, so
/// each id keeps its own picker.
fn parse_antigravity_history(tail: &str, dir: &str) -> Vec<AgentConversation> {
    let mut by_id: HashMap<String, AgentConversation> = HashMap::new();
    let mut pickers: HashMap<String, TitlePicker> = HashMap::new();
    for entry in json_lines(tail) {
        let id = entry.get("conversationId").and_then(Value::as_str).unwrap_or("");
        let workspace = entry.get("workspace").and_then(Value::as_str).unwrap_or("");
        if !safe_id(id) || !same_path(workspace, dir) {
            continue;
        }
        let at = epoch_ms(entry.get("timestamp").unwrap_or(&Value::Null));
        pickers
            .entry(id.to_owned())
            .or_default()
            .offer(entry.get("display").and_then(Value::as_str).and_then(candidate_title));
        let known = by_id.entry(id.to_owned()).or_insert_with(|| AgentConversation {
            id: id.to_owned(),
            agent_id: "antigravity",
            title: String::new(),
            updated_at: at,
            command: format!("agy --conversation {id}"),
        });
        // The last prompt of a conversation is what dates it.
        if at > known.updated_at {
            known.updated_at = at;
        }
    }
    for (id, conversation) in by_id.iter_mut() {
        if let Some(picker) = pickers.get(id) {
            conversation.title = picker.value();
        }
    }
    by_id.into_values().collect()
}

/// Grok names a session group after the working directory, URL-encoded.
fn decode_grok_cwd(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&name[i + 1..i + 3], 16) {
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

/// `summary.json` — Grok's own index entry. Every field is optional: a
/// differently-shaped summary must still leave the session resumable from
/// its directory name and file times.
struct GrokSummary {
    cwd: String,
    title: String,
    updated_at: u64,
}

fn parse_grok_summary(text: &str) -> Option<GrokSummary> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    if !parsed.is_object() {
        return None;
    }
    let info = parsed.get("info");
    let pick = |keys: &[&str]| -> String {
        keys.iter()
            .filter_map(|k| info.and_then(|i| i.get(k)).or_else(|| parsed.get(k)))
            .filter_map(Value::as_str)
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("")
            .to_owned()
    };
    let cwd = pick(&["cwd", "working_directory", "workingDirectory"]);
    let title = candidate_title(&pick(&["generated_title", "title"]))
        .or_else(|| candidate_title(&pick(&["session_summary", "last_turn_summary", "last_recap"])))
        .map(|c| c.text)
        .unwrap_or_default();
    let updated_at = ["updated_at", "created_at"]
        .iter()
        .filter_map(|k| parsed.get(k))
        .map(parse_sql_time)
        .max()
        .unwrap_or(0);
    Some(GrokSummary {
        cwd,
        title,
        updated_at,
    })
}

/// The first thing the user typed, from Grok's ACP update stream — whole
/// (`user_message`) or streamed (`user_message_chunk`), stitched back before
/// it is judged.
fn parse_grok_title(head: &str) -> String {
    let mut picker = TitlePicker::default();
    let mut chunks = String::new();
    for entry in json_lines(head) {
        let kind = entry.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
        if kind != "user_message" && kind != "user_message_chunk" {
            // Anything after the prompt belongs to the agent; a stitched
            // prompt is complete as soon as the turn moves on.
            if !chunks.trim().is_empty() {
                break;
            }
            continue;
        }
        let content = entry.get("content");
        if kind == "user_message_chunk" {
            if let Some(text) = content.and_then(|c| c.get("text")).and_then(Value::as_str) {
                chunks.push_str(text);
            }
            continue;
        }
        if picker.offer(message_candidate(content)) {
            break;
        }
        if let Some(text) = content.and_then(|c| c.get("text")).and_then(Value::as_str) {
            if picker.offer(candidate_title(text)) {
                break;
            }
        }
    }
    if picker.value().is_empty() {
        picker.offer(candidate_title(chunks.trim()));
    }
    picker.value()
}

struct Candidate {
    path: PathBuf,
    mtime: u64,
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 {
        return None;
    }
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// Newest non-empty files first, capped at `max`.
fn stat_files(paths: Vec<PathBuf>, max: usize) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = paths
        .into_iter()
        .filter_map(|path| file_mtime_ms(&path).map(|mtime| Candidate { path, mtime }))
        .collect();
    out.sort_by_key(|c| std::cmp::Reverse(c.mtime));
    out.truncate(max);
    out
}

fn list_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Claude's store is already per folder, so only the transcripts that could
/// reach the list are opened at all.
fn claude_conversations(dir: &str, home: &Path, limit: usize) -> Vec<AgentConversation> {
    let projects = home.join(".claude").join("projects");
    let mut root = PathBuf::new();
    let mut names: Vec<String> = Vec::new();
    for slug in [
        claude_project_slug(dir),
        claude_project_slug_unicode(dir),
    ]
    .into_iter()
    .collect::<HashSet<_>>()
    {
        if slug.is_empty() {
            continue;
        }
        root = projects.join(&slug);
        names = list_names(&root)
            .into_iter()
            .filter(|n| n.ends_with(".jsonl"))
            .collect();
        if !names.is_empty() {
            break;
        }
    }
    let candidates = stat_files(names.into_iter().map(|n| root.join(n)).collect(), limit);
    let mut out = Vec::new();
    for candidate in candidates {
        let Some(id) = candidate.path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !safe_id(id) {
            continue;
        }
        let head = read_chunk(&candidate.path, HEAD_BYTES, false);
        // Same-slug collisions are rare but real, and offering someone else's
        // conversation is worse than showing nothing.
        let recorded = parse_claude_cwd(&head);
        if !recorded.is_empty() && !same_path(&recorded, dir) {
            continue;
        }
        let mut title = parse_claude_title(&head);
        if title.is_empty() {
            // A transcript that resumes a compacted session opens with a long
            // summary; only those pay for the wider look.
            title = parse_claude_title(&read_chunk(&candidate.path, DEEP_TITLE_BYTES, false));
        }
        out.push(AgentConversation {
            id: id.to_owned(),
            agent_id: "claude",
            title,
            updated_at: candidate.mtime,
            command: format!("claude --resume {id}"),
        });
    }
    out
}

/// `sessions/<year>/<month>/<day>/rollout-*.jsonl`, walked newest day first.
fn codex_rollouts(root: &Path) -> Vec<PathBuf> {
    let list_desc = |dir: &Path| -> Vec<String> {
        let mut names = list_names(dir);
        names.sort_by(|a, b| b.cmp(a));
        names
    };
    let mut files = Vec::new();
    for year in list_desc(root) {
        for month in list_desc(&root.join(&year)) {
            for day in list_desc(&root.join(&year).join(&month)) {
                let day_dir = root.join(&year).join(&month).join(&day);
                for name in list_desc(&day_dir) {
                    if name.ends_with(".jsonl") {
                        files.push(day_dir.join(name));
                    }
                }
                if files.len() >= CODEX_FILE_BUDGET {
                    return files;
                }
            }
        }
    }
    files
}

/// Codex keeps one folder-agnostic pile of rollouts, so its scan reads heads
/// until it has enough conversations for *this* folder.
fn codex_conversations(dir: &str, home: &Path, limit: usize) -> Vec<AgentConversation> {
    let root = home.join(".codex").join("sessions");
    let candidates = stat_files(codex_rollouts(&root), CODEX_FILE_BUDGET);
    let mut out = Vec::new();
    for candidate in candidates {
        if out.len() >= limit {
            break;
        }
        let head = read_chunk(&candidate.path, CODEX_META_BYTES, false);
        let Some((id, cwd)) = parse_codex_meta(&head) else {
            continue;
        };
        if !same_path(&cwd, dir) {
            continue;
        }
        let mut title = parse_codex_title(&head);
        if title.is_empty() {
            title = parse_codex_title(&read_chunk(&candidate.path, CODEX_TITLE_BYTES, false));
        }
        out.push(AgentConversation {
            command: format!("codex resume {id}"),
            id,
            agent_id: "codex",
            title,
            updated_at: candidate.mtime,
        });
    }
    out
}

/// The original also joined `conversation_summaries.db` (a raw-sqlite read)
/// because the db is the only place a conversation's id is tied to its
/// folder. No sqlite dependency lives in this build, so the prompt-history
/// tail — the store that holds the user's own words — is all we scan.
fn antigravity_conversations(dir: &str, home: &Path) -> Vec<AgentConversation> {
    let root = home.join(".gemini").join("antigravity-cli");
    parse_antigravity_history(&read_chunk(&root.join("history.jsonl"), HISTORY_TAIL_BYTES, true), dir)
}

fn grok_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = std::env::var_os("GROK_HOME")
        .map(|v| PathBuf::from(v).join("sessions"))
        .into_iter()
        .collect();
    roots.push(home.join(".grok").join("sessions"));
    roots
}

/// The session group Grok recorded for this folder, or "" when it kept none.
fn grok_group_dir(root: &Path, dir: &str) -> Option<PathBuf> {
    let names = list_names(root);
    for name in &names {
        if same_path(&decode_grok_cwd(name), dir) {
            return Some(root.join(name));
        }
    }
    // A path too long to encode is stored under a slug plus a hash, with the
    // original written to `.cwd`.
    for name in &names {
        let recorded = std::fs::read_to_string(root.join(name).join(".cwd")).unwrap_or_default();
        if !recorded.trim().is_empty() && same_path(recorded.trim(), dir) {
            return Some(root.join(name));
        }
    }
    // Last resort: ask the sessions themselves. Bounded, and it only runs
    // for a folder Grok has no group for.
    let mut budget = GROK_GROUP_PROBE_BUDGET;
    for name in &names {
        let group = root.join(name);
        for id in list_names(&group) {
            if !safe_id(&id) {
                continue;
            }
            budget -= 1;
            if budget < 0 {
                break;
            }
            let text = read_chunk(&group.join(&id).join("summary.json"), GROK_SUMMARY_BYTES, false);
            if let Some(summary) = parse_grok_summary(&text) {
                if !summary.cwd.is_empty() && same_path(&summary.cwd, dir) {
                    return Some(group);
                }
            }
        }
    }
    None
}

/// Grok keeps one folder per session; only the newest are opened. A
/// directory without `updates.jsonl` holds nothing to resume.
fn grok_conversations(dir: &str, home: &Path, limit: usize) -> Vec<AgentConversation> {
    let mut group = None;
    for root in grok_roots(home) {
        group = grok_group_dir(&root, dir);
        if group.is_some() {
            break;
        }
    }
    let Some(group) = group else { return Vec::new() };
    let candidates = stat_files(
        list_names(&group)
            .into_iter()
            .filter(|name| safe_id(name))
            .map(|id| group.join(&id).join("updates.jsonl"))
            .collect(),
        limit,
    );
    let mut out = Vec::new();
    for candidate in candidates {
        let Some(id) = candidate
            .path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
        else {
            continue;
        };
        let session_dir = group.join(id);
        let summary = parse_grok_summary(&read_chunk(
            &session_dir.join("summary.json"),
            GROK_SUMMARY_BYTES,
            false,
        ));
        // The summary's own cwd is the last word on whether the session
        // belongs to this folder.
        if let Some(s) = &summary {
            if !s.cwd.is_empty() && !same_path(&s.cwd, dir) {
                continue;
            }
        }
        let title = summary
            .as_ref()
            .map(|s| s.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| parse_grok_title(&read_chunk(&candidate.path, HEAD_BYTES, false)));
        out.push(AgentConversation {
            id: id.to_owned(),
            agent_id: "grok",
            title,
            updated_at: summary.map(|s| s.updated_at).filter(|t| *t > 0).unwrap_or(candidate.mtime),
            command: format!("grok --resume {id}"),
        });
    }
    out
}

/// Every resumable conversation recorded for `dir`, newest first. One
/// agent's unreadable store does not hide the others': each provider is
/// isolated and contributes nothing when it fails.
pub fn list(dir: &str, home: Option<&Path>, limit: Option<usize>) -> Vec<AgentConversation> {
    if dir.trim().is_empty() {
        return Vec::new();
    }
    let home = home
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let limit = limit.unwrap_or(DEFAULT_LIMIT).max(1);

    let mut all: Vec<AgentConversation> = Vec::new();
    all.extend(claude_conversations(dir, &home, limit));
    all.extend(codex_conversations(dir, &home, limit));
    all.extend(antigravity_conversations(dir, &home));
    all.extend(grok_conversations(dir, &home, limit));

    let mut seen = HashSet::new();
    all.retain(|c| seen.insert(conversation_key(c.agent_id, c.id.as_str())));
    all.sort_by_key(|c| std::cmp::Reverse(c.updated_at));

    // Recency alone lets one busy agent fill the whole list, hiding that the
    // folder also has conversations in the others. So the room is dealt out
    // a round at a time, newest first within each agent, and any room an
    // agent does not use goes back to plain recency.
    let mut queues: HashMap<&'static str, VecDeque<AgentConversation>> = HashMap::new();
    for conversation in all {
        queues.entry(conversation.agent_id).or_default().push_back(conversation);
    }
    let mut picked: Vec<AgentConversation> = Vec::new();
    while picked.len() < limit {
        let mut dealt = false;
        for queue in queues.values_mut() {
            if picked.len() >= limit {
                break;
            }
            if let Some(next) = queue.pop_front() {
                picked.push(next);
                dealt = true;
            }
        }
        if !dealt {
            break;
        }
    }
    picked.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    picked
}

/// "24 minutes ago" — short enough for a row, precise enough to recognise.
pub fn relative_time(at_ms: u64, now_ms: u64) -> String {
    if at_ms == 0 {
        return "unknown".to_owned();
    }
    if now_ms <= at_ms || now_ms - at_ms < 60_000 {
        return "just now".to_owned();
    }
    let delta = now_ms - at_ms;
    let plural = |value: u64, unit: &str| format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" });
    if delta < 3_600_000 {
        return plural(delta / 60_000, "minute");
    }
    if delta < 86_400_000 {
        return plural(delta / 3_600_000, "hour");
    }
    plural(delta / 86_400_000, "day")
}

/// How many "Resume all" may open at once: never more than there are rows,
/// free session slots, or the bulk cap that keeps one click from starting a
/// dozen CLI processes.
pub const RESUME_ALL_MAX: usize = 6;
/// Rows the collapsed list shows before "Show all".
pub const COLLAPSED_ROWS: usize = 3;

/// What a bulk resume should open: a selection is explicit — capped only by
/// free slots; with nothing selected the button keeps its old meaning, the
/// newest few.
pub fn conversations_to_resume(
    visible: &[AgentConversation],
    selected: &HashSet<String>,
    remaining_slots: usize,
) -> Vec<AgentConversation> {
    let picked: Vec<AgentConversation> = visible
        .iter()
        .filter(|c| selected.contains(&conversation_key(c.agent_id, &c.id)))
        .cloned()
        .collect();
    if !picked.is_empty() {
        return picked.into_iter().take(remaining_slots).collect();
    }
    let count = visible.len().min(remaining_slots).min(RESUME_ALL_MAX);
    visible.iter().take(count).cloned().collect()
}

// ── Dismissed store ──────────────────────────────────────────────────────
// One JSON map of folder → keys; pruning on every write keeps it from
// growing as agents create and delete transcripts.

fn dismissed_path() -> PathBuf {
    crate::ipc::user_data_dir().join("code-resume-dismissed.json")
}

fn read_store() -> HashMap<String, Vec<String>> {
    std::fs::read(dismissed_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn load_dismissed(dir: &str) -> HashSet<String> {
    read_store()
        .get(dir)
        .map(|keys| keys.iter().cloned().collect())
        .unwrap_or_default()
}

/// Persists the dismissals that still match a live conversation.
pub fn save_dismissed(
    dir: &str,
    dismissed: &HashSet<String>,
    live: &HashSet<String>,
) {
    let mut store = read_store();
    let kept: Vec<String> = dismissed.iter().filter(|k| live.contains(*k)).cloned().collect();
    if kept.is_empty() {
        store.remove(dir);
    } else {
        store.insert(dir.to_owned(), kept);
    }
    if let Some(parent) = dismissed_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec_pretty(&store) {
        let _ = std::fs::write(dismissed_path(), bytes);
    }
}
