//! Agent state detection over the live tail of a terminal's screen.
//!
//! Ported from herdr's manifest-driven detector — herdrdev/herdr,
//! `src/detect/` (Apache-2.0). The rule grammar mirrors theirs —
//! `all`/`any`/`not` over `contains`/`regex`/`line_regex` inside named
//! screen regions, highest priority wins — so manifests can be kept in
//! sync with upstream; ours are JSON embedded in the binary
//! (`manifests/*.json`) because serde_json is already a dependency.
//! Regions slate can't see (e.g. `osc_title` text the snapshot doesn't
//! carry) evaluate empty, so those rules simply never fire.

use serde::Deserialize;
use std::sync::LazyLock;

/// The detected state of a terminal pane — herdr's `AgentState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// Agent finished, prompt visible, nothing happening.
    Idle,
    /// Agent is actively working/processing.
    Working,
    /// Agent needs human input and is blocked on a response.
    Blocked,
    /// Plain shell or unrecognized program.
    Unknown,
}

impl AgentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "idle" => AgentState::Idle,
            "working" => AgentState::Working,
            "blocked" => AgentState::Blocked,
            "unknown" => AgentState::Unknown,
            _ => return None,
        })
    }
}

/// What the detector decided for one terminal.
#[derive(Clone, Debug)]
pub struct Detection {
    /// The manifest id that claimed the pane (`None` when nothing did).
    pub agent: Option<String>,
    pub state: AgentState,
    /// The winning rule's id — herdr's `agent explain` evidence.
    pub rule: Option<String>,
}

#[derive(Deserialize)]
struct Manifest {
    id: String,
    /// Lowercase substrings a terminal's name is matched against.
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
struct Rule {
    id: String,
    state: String,
    #[serde(default)]
    priority: i32,
    region: String,
    #[serde(default)]
    skip_state_update: bool,
    #[serde(flatten)]
    matcher: Matcher,
}

#[derive(Deserialize, Default)]
struct Matcher {
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    regex: Vec<String>,
    #[serde(default)]
    line_regex: Vec<String>,
    #[serde(default)]
    all: Vec<Matcher>,
    #[serde(default)]
    any: Vec<Matcher>,
    #[serde(default)]
    not: Vec<Matcher>,
}

struct CompiledMatcher {
    contains: Vec<String>,
    regex: Vec<regex::Regex>,
    line_regex: Vec<regex::Regex>,
    all: Vec<CompiledMatcher>,
    any: Vec<CompiledMatcher>,
    not: Vec<CompiledMatcher>,
}

struct CompiledRule {
    id: String,
    state: AgentState,
    priority: i32,
    region: String,
    skip_state_update: bool,
    matcher: CompiledMatcher,
}

struct CompiledManifest {
    id: String,
    names: Vec<String>,
    rules: Vec<CompiledRule>,
}

fn compile(m: &Matcher) -> CompiledMatcher {
    CompiledMatcher {
        // herdr folds needles at load (`needle.to_lowercase()`), so
        // `contains` matching is case-insensitive on both sides.
        contains: m.contains.iter().map(|c| c.to_lowercase()).collect(),
        regex: m
            .regex
            .iter()
            .filter_map(|r| regex::Regex::new(r).ok())
            .collect(),
        line_regex: m
            .line_regex
            .iter()
            .filter_map(|r| regex::Regex::new(r).ok())
            .collect(),
        all: m.all.iter().map(compile).collect(),
        any: m.any.iter().map(compile).collect(),
        not: m.not.iter().map(compile).collect(),
    }
}

fn load() -> Vec<CompiledManifest> {
    const BLOBS: &[&str] = &[
        include_str!("agent_state/manifests/claude.json"),
        include_str!("agent_state/manifests/codex.json"),
        include_str!("agent_state/manifests/opencode.json"),
        include_str!("agent_state/manifests/gray.json"),
    ];
    let mut out = Vec::new();
    for blob in BLOBS {
        let manifest: Manifest = match serde_json::from_str(blob) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("agent_state manifest: {e}");
                continue;
            }
        };
        let mut rules: Vec<CompiledRule> = manifest
            .rules
            .iter()
            .map(|r| CompiledRule {
                id: r.id.clone(),
                state: AgentState::parse(&r.state).unwrap_or(AgentState::Unknown),
                priority: r.priority,
                region: r.region.clone(),
                skip_state_update: r.skip_state_update,
                matcher: compile(&r.matcher),
            })
            .collect();
        rules.sort_by_key(|rule| std::cmp::Reverse(rule.priority));
        out.push(CompiledManifest {
            id: manifest.id,
            names: manifest.names,
            rules,
        });
    }
    out
}

static MANIFESTS: LazyLock<Vec<CompiledManifest>> = LazyLock::new(load);

/// `id`/`aliases` in herdr — which manifest claims this pane. Code-session
/// cards pass their `agent_id`; everything else is matched by the terminal
/// name (launch names every PTY after its agent's label).
pub fn manifest_for(agent_id: &str, name: &str) -> Option<String> {
    let manifests = &*MANIFESTS;
    if let Some(m) = manifests.iter().find(|m| m.id == agent_id) {
        return Some(m.id.clone());
    }
    let lowered = name.to_lowercase();
    manifests
        .iter()
        .find(|m| m.names.iter().any(|n| lowered.contains(n.as_str())))
        .map(|m| m.id.clone())
}

/// The text a rule's `region` selects. Unimplemented regions return `None`
/// so their rules cannot fire — the safe direction: a rule that cannot see
/// its evidence never claims a state.
fn region_text<'a>(region: &str, screen: &'a str, title: &'a str) -> Option<String> {
    if region == "screen" || region == "whole_recent" || region == "whole_recent_without_current_prompt_marker" {
        return Some(screen.to_owned());
    }
    if region == "osc_title" {
        return Some(title.to_owned());
    }
    if let Some(rest) = region.strip_prefix("bottom_non_empty_lines(") {
        let n: usize = rest.trim_end_matches(')').parse().ok()?;
        let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        return Some(lines[start..].join("\n"));
    }
    if let Some(rest) = region.strip_prefix("top_non_empty_lines(") {
        let n: usize = rest.trim_end_matches(')').parse().ok()?;
        let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
        return Some(lines[..n.min(lines.len())].join("\n"));
    }
    if region == "after_last_horizontal_rule" || region == "prompt_box_body" {
        // A rule line is mostly box-drawing — Claude and Codex both frame
        // prompts with `─`/`━` rules.
        let mut last = 0usize;
        for (i, line) in screen.lines().enumerate() {
            let trimmed = line.trim();
            let boxish = trimmed
                .chars()
                .filter(|c| "─━═│┌┐└┘├┤┬┴┼".contains(*c))
                .count();
            if trimmed.len() >= 4 && boxish * 3 >= trimmed.chars().count() {
                last = i + 1;
            }
        }
        return Some(screen.lines().skip(last).collect::<Vec<_>>().join("\n"));
    }
    if region == "after_last_prompt_marker" {
        let mut last = 0usize;
        for (i, line) in screen.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with('❯') || t.starts_with('›') || t.starts_with("$ ") {
                last = i + 1;
            }
        }
        return Some(screen.lines().skip(last).collect::<Vec<_>>().join("\n"));
    }
    None
}

fn matcher_hits(m: &CompiledMatcher, text: &str) -> bool {
    // herdr's `lower_text` — the haystack folds the same way the needles did.
    let text = text.to_lowercase();
    let text = text.as_str();
    m.contains.iter().all(|c| text.contains(c.as_str()))
        && m.regex.iter().all(|r| r.is_match(text))
        && m
            .line_regex
            .iter()
            .all(|r| text.lines().any(|l| r.is_match(l)))
        && m.all.iter().all(|x| matcher_hits(x, text))
        && (m.any.is_empty() || m.any.iter().any(|x| matcher_hits(x, text)))
        && !m.not.iter().any(|x| matcher_hits(x, text))
}

/// herdr's `detect_state` — highest-priority firing rule wins; a
/// `skip_state_update` rule maps to `Unknown` (the screen is a viewer, not
/// live chrome). A claimed agent with no firing rule reads `Idle` — the
/// alternative (flapping between `unknown` and `working` mid-turn) is the
/// worse signal.
pub fn detect(screen: &str, title: &str, agent: Option<&str>) -> Detection {
    let Some(agent_id) = agent else {
        return Detection {
            agent: None,
            state: AgentState::Unknown,
            rule: None,
        };
    };
    let manifests = &*MANIFESTS;
    let Some(manifest) = manifests.iter().find(|m| m.id == *agent_id) else {
        return Detection {
            agent: Some(agent_id.to_owned()),
            state: AgentState::Unknown,
            rule: None,
        };
    };
    for rule in &manifest.rules {
        let Some(text) = region_text(&rule.region, screen, title) else {
            continue;
        };
        if matcher_hits(&rule.matcher, &text) {
            return Detection {
                agent: Some(manifest.id.clone()),
                state: if rule.skip_state_update {
                    AgentState::Unknown
                } else {
                    rule.state
                },
                rule: Some(rule.id.clone()),
            };
        }
    }
    Detection {
        agent: Some(manifest.id.clone()),
        state: AgentState::Idle,
        rule: None,
    }
}

/// One `/orchestration/agents` row — id, name, claimed agent and state.
pub fn row(
    terminal_id: &str,
    name: &str,
    screen_tail: &str,
    title: &str,
    agent_hint: Option<&str>,
) -> serde_json::Value {
    let hint = agent_hint
        .map(str::to_owned)
        .or_else(|| manifest_for("", name));
    let detection = detect(screen_tail, title, hint.as_deref());
    serde_json::json!({
        "id": terminal_id,
        "name": name,
        "oscTitle": title,
        "agent": detection.agent,
        "state": detection.state.as_str(),
        "rule": detection.rule,
    })
}
