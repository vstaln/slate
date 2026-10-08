//! `settings.json` — the shell's `appState.ts` settings slice: the object
//! `window.api.settings.get/set` round-tripped. Defaults below are the
//! bundle's `sn` literal; keys we don't manage are preserved verbatim in
//! `extra`, matching the original's `{...defaults, ...loaded}` spread.
//!
//! Two canvas behaviours the original kept OUT of this file stay out here
//! too: `terminalsFlipped` lived in `localStorage["orcspace-flip-terminals"]`
//! and `attachmentMode` in `localStorage["orcspace-attach:<widgetId>"]`. If
//! either ever appears in a `settings.json` it rides along in `extra`,
//! untyped. (`ui_preferences.rs` is the unrelated native prefs file — this
//! module is the settings.json domain only.)

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// `Dr` — the settings UI's cap on `customCodeAgents`
/// (`const Dr=12`, `n.length===Dr` breaks the sanitize loop).
const MAX_CODE_AGENTS: usize = 12;

/// Widget kinds `mr` strips from `favoriteWidgets` on every read —
/// `.filter(n=>n!=="translator"&&n!=="id-generator"&&n!=="note")`. They were
/// removed from the registry, so a stale favorite can never resurrect one.
const RETIRED_WIDGETS: [&str; 3] = ["translator", "id-generator", "note"];

/// `zi` — agent ids the built-ins own; a custom agent may not reuse one
/// (`zi.has(c.toLowerCase())` — the reserved check is case-insensitive).
const RESERVED_AGENT_IDS: [&str; 10] = [
    "gray",
    "claude",
    "codex",
    "antigravity",
    "grok",
    "opencode",
    "kimi",
    "cursor",
    "browser",
    "custom",
];

/// `sn.favoriteWidgets` — the context menu's default favorites section, in
/// order (bundle literal: `["terminal","files","sys-monitor","timer",
/// "planner","orchestration","browser","image","links","music-player",
/// "chat","notes","calendar","kanban"]`).
const DEFAULT_FAVORITE_WIDGETS: [&str; 14] = [
    "terminal",
    "files",
    "sys-monitor",
    "timer",
    "planner",
    "orchestration",
    "browser",
    "image",
    "links",
    "music-player",
    "chat",
    "notes",
    "calendar",
    "kanban",
];

/// `Bi` — provider renames the shell applies to `aiModel` on every read
/// (`Bi[t.aiModel]??t.aiModel`). Literal:
/// `{"claude-sonnet-4-5":"claude-sonnet-5","claude-opus-4-1":"claude-opus-5",
/// "claude-haiku-4-5":"claude-haiku-4-5-20251001","grok-4":"grok-4.6",
/// "grok-4-fast":"grok-4.6","grok-3-mini":"grok-4.3"}`.
fn migrate_ai_model(model: &str) -> String {
    match model {
        "claude-sonnet-4-5" => "claude-sonnet-5",
        "claude-opus-4-1" => "claude-opus-5",
        "claude-haiku-4-5" => "claude-haiku-4-5-20251001",
        "grok-4" | "grok-4-fast" => "grok-4.6",
        "grok-3-mini" => "grok-4.3",
        other => other,
    }
    .to_owned()
}

/// One `customCodeAgents` entry — `{id, name, command}` (there is no `args`;
/// arguments ride inside `command`). `Fi` enforces: `id` trimmed must match
/// `/^[a-zA-Z0-9_-]{1,64}$/`, not be reserved or duplicated; `name` trimmed
/// non-empty, ≤ 40 chars; `command` trimmed non-empty, ≤ 512 chars; neither
/// may carry `[\u0000-\u001f\u007f]`. The settings UI mints
/// `id: crypto.randomUUID()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomCodeAgent {
    pub id: String,
    pub name: String,
    pub command: String,
}

/// `localModel` — the embedded-llama block, default
/// `{enabled:!1,serverBin:"",modelPath:"",contextSize:32768,gpuLayers:99,
/// idleTimeoutMs:5*6e4,offloadVision:!1}`. On update the shell shallow-merges
/// (`{...d.localModel,...i.localModel}`); per-field defaults here reproduce
/// that merge on read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalModel {
    /// `enabled` — default `false` (`enabled:!1`).
    pub enabled: bool,
    /// `serverBin` — default `""` (`serverBin:""`).
    pub server_bin: String,
    /// `modelPath` — default `""` (`modelPath:""`).
    pub model_path: String,
    /// `contextSize` — default `32768` (`contextSize:32768`).
    pub context_size: u64,
    /// `gpuLayers` — default `99` (`gpuLayers:99` — effectively "all").
    pub gpu_layers: u64,
    /// `idleTimeoutMs` — default `300000` (`idleTimeoutMs:5*6e4`).
    pub idle_timeout_ms: u64,
    /// `offloadVision` — default `false` (`offloadVision:!1`).
    pub offload_vision: bool,
}

impl Default for LocalModel {
    fn default() -> Self {
        Self {
            enabled: false,
            server_bin: String::new(),
            model_path: String::new(),
            context_size: 32768,
            gpu_layers: 99,
            idle_timeout_ms: 300_000,
            offload_vision: false,
        }
    }
}

/// The persisted shape: every key of `sn` typed, plus `extra` for anything
/// the fuller schema writes that we don't manage.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `linkSyntax` — default `"both"` (`linkSyntax:"both"`).
    pub link_syntax: String,
    /// `windowsShell` — default `"cmd"` (`windowsShell:"cmd"`); the only
    /// other value the UI writes is `"powershell"`.
    pub windows_shell: String,
    /// `commandPrefix` — default `"any"` (`commandPrefix:"any"`), i.e. the
    /// canvas treats any submitted text as a command rather than requiring
    /// a `/ . @` invocation prefix.
    pub command_prefix: String,
    /// `userName` — default `"you"` (`userName:"you"`; readers fall back
    /// with `userName?.trim()||"you"`).
    pub user_name: String,
    /// `backgroundDim` — default `45` (`backgroundDim:45`, `??45`).
    pub background_dim: u64,
    /// `backgroundBlur` — default `40` (`backgroundBlur:40`, `??40`).
    pub background_blur: u64,
    /// `aiProvider` — default `"chatgpt"` (`aiProvider:"chatgpt"`).
    pub ai_provider: String,
    /// `aiModel` — default `"gpt-5.6-sol"` (`aiModel:"gpt-5.6-sol"`);
    /// renamed ids are rewritten through `Bi` on load.
    pub ai_model: String,
    /// `aiReasoningEffort` — default `"medium"`
    /// (`aiReasoningEffort:"medium"`).
    pub ai_reasoning_effort: String,
    /// `aiConnectedProviders` — default `[]` (`aiConnectedProviders:[]`).
    pub ai_connected_providers: Vec<String>,
    /// `localModel` — default block quoted on [`LocalModel`].
    pub local_model: LocalModel,
    /// `favoriteWidgets` — default [`DEFAULT_FAVORITE_WIDGETS`]; retired
    /// kinds are filtered on load and on set.
    pub favorite_widgets: Vec<String>,
    /// `favoriteTerminalNames` — default `[]` (`favoriteTerminalNames:[]`).
    /// These are the user's picks; the pool they pick from is
    /// [`crate::terminal_names::NAMES`].
    pub favorite_terminal_names: Vec<String>,
    /// `customCodeAgents` — default `[]` (`customCodeAgents:[]`); shape and
    /// limits on [`CustomCodeAgent`].
    pub custom_code_agents: Vec<CustomCodeAgent>,
    /// `imageInsertShortcut` — default `"Mod+Shift+I"`
    /// (`imageInsertShortcut:"Mod+Shift+I"`).
    pub image_insert_shortcut: String,
    /// `autoApprovePermissions` — default `false`
    /// (`autoApprovePermissions:!1`, `??!1`).
    pub auto_approve_permissions: bool,
    /// Keys we don't manage — preserved verbatim on rewrite.
    extra: Map<String, Value>,
}

impl Default for Settings {
    /// The bundle's `sn` object, verbatim.
    fn default() -> Self {
        Self {
            link_syntax: "both".into(),
            windows_shell: "cmd".into(),
            command_prefix: "any".into(),
            user_name: "you".into(),
            background_dim: 45,
            background_blur: 40,
            ai_provider: "chatgpt".into(),
            ai_model: "gpt-5.6-sol".into(),
            ai_reasoning_effort: "medium".into(),
            ai_connected_providers: Vec::new(),
            local_model: LocalModel::default(),
            favorite_widgets: DEFAULT_FAVORITE_WIDGETS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            favorite_terminal_names: Vec::new(),
            custom_code_agents: Vec::new(),
            image_insert_shortcut: "Mod+Shift+I".into(),
            auto_approve_permissions: false,
            extra: Map::new(),
        }
    }
}

impl Settings {
    /// `~/.config/Slate/settings.json` on Linux (platform variants in
    /// [`crate::ipc::user_data_dir`]).
    pub fn path() -> PathBuf {
        crate::ipc::user_data_dir().join("settings.json")
    }

    /// `mr` — recovered-tolerant read. A missing or corrupt file yields the
    /// `sn` defaults; un-typeable managed values fall back per field while
    /// unknown keys stay in `extra`.
    pub fn load_from(path: &Path) -> Self {
        let Some(bytes) = crate::ipc::read_store_recovered(path) else {
            return Self::default();
        };
        let Ok(Value::Object(mut raw)) = serde_json::from_slice::<Value>(&bytes) else {
            return Self::default();
        };
        let default = Self::default();
        // `aiModel:typeof t.aiModel=="string"?Bi[t.aiModel]??t.aiModel:sn.aiModel`
        let ai_model = migrate_ai_model(&take_string(&mut raw, "aiModel", &default.ai_model));
        // `(t.favoriteWidgets??sn.favoriteWidgets??[]).filter(retired)` — a
        // stored array wins even when empty; a non-array is un-typeable and
        // falls back to defaults.
        let favorite_widgets = match raw.remove("favoriteWidgets") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .filter(|kind| !RETIRED_WIDGETS.contains(kind))
                .map(str::to_owned)
                .collect(),
            _ => default.favorite_widgets.clone(),
        };
        // `Array.isArray(t.favoriteTerminalNames)?…strings only:sn…`
        let favorite_terminal_names = match raw.remove("favoriteTerminalNames") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => default.favorite_terminal_names.clone(),
        };
        let ai_connected_providers = match raw.remove("aiConnectedProviders") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => default.ai_connected_providers.clone(),
        };
        Self {
            link_syntax: take_string(&mut raw, "linkSyntax", &default.link_syntax),
            windows_shell: take_string(&mut raw, "windowsShell", &default.windows_shell),
            command_prefix: take_string(&mut raw, "commandPrefix", &default.command_prefix),
            user_name: take_string(&mut raw, "userName", &default.user_name),
            background_dim: take_u64(&mut raw, "backgroundDim", default.background_dim),
            background_blur: take_u64(&mut raw, "backgroundBlur", default.background_blur),
            ai_provider: take_string(&mut raw, "aiProvider", &default.ai_provider),
            ai_model,
            ai_reasoning_effort: take_string(
                &mut raw,
                "aiReasoningEffort",
                &default.ai_reasoning_effort,
            ),
            ai_connected_providers,
            local_model: take_local_model(&mut raw),
            favorite_widgets,
            favorite_terminal_names,
            custom_code_agents: take_agents(raw.remove("customCodeAgents")),
            image_insert_shortcut: take_string(
                &mut raw,
                "imageInsertShortcut",
                &default.image_insert_shortcut,
            ),
            auto_approve_permissions: raw
                .remove("autoApprovePermissions")
                .and_then(|v| v.as_bool())
                .unwrap_or(default.auto_approve_permissions),
            extra: raw,
        }
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    fn to_json(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("linkSyntax".into(), Value::from(self.link_syntax.as_str()));
        map.insert(
            "windowsShell".into(),
            Value::from(self.windows_shell.as_str()),
        );
        map.insert(
            "commandPrefix".into(),
            Value::from(self.command_prefix.as_str()),
        );
        map.insert("userName".into(), Value::from(self.user_name.as_str()));
        map.insert("backgroundDim".into(), Value::from(self.background_dim));
        map.insert("backgroundBlur".into(), Value::from(self.background_blur));
        map.insert("aiProvider".into(), Value::from(self.ai_provider.as_str()));
        map.insert("aiModel".into(), Value::from(self.ai_model.as_str()));
        map.insert(
            "aiReasoningEffort".into(),
            Value::from(self.ai_reasoning_effort.as_str()),
        );
        map.insert(
            "aiConnectedProviders".into(),
            Value::Array(
                self.ai_connected_providers
                    .iter()
                    .map(|s| Value::from(s.as_str()))
                    .collect(),
            ),
        );
        map.insert(
            "localModel".into(),
            json!({
                "enabled": self.local_model.enabled,
                "serverBin": self.local_model.server_bin,
                "modelPath": self.local_model.model_path,
                "contextSize": self.local_model.context_size,
                "gpuLayers": self.local_model.gpu_layers,
                "idleTimeoutMs": self.local_model.idle_timeout_ms,
                "offloadVision": self.local_model.offload_vision,
            }),
        );
        map.insert(
            "favoriteWidgets".into(),
            Value::Array(
                self.favorite_widgets
                    .iter()
                    .map(|s| Value::from(s.as_str()))
                    .collect(),
            ),
        );
        map.insert(
            "favoriteTerminalNames".into(),
            Value::Array(
                self.favorite_terminal_names
                    .iter()
                    .map(|s| Value::from(s.as_str()))
                    .collect(),
            ),
        );
        map.insert(
            "customCodeAgents".into(),
            Value::Array(
                self.custom_code_agents
                    .iter()
                    .map(|a| {
                        json!({
                            "id": a.id,
                            "name": a.name,
                            "command": a.command,
                        })
                    })
                    .collect(),
            ),
        );
        map.insert(
            "imageInsertShortcut".into(),
            Value::from(self.image_insert_shortcut.as_str()),
        );
        map.insert(
            "autoApprovePermissions".into(),
            Value::from(self.auto_approve_permissions),
        );
        Value::Object(map)
    }

    /// `settings.set` — the whole merged object goes through the shell's
    /// atomic writer (temp → fsync → `.bak` → rename).
    pub fn save(&self) {
        let bytes = crate::jsjson::to_js_json_pretty(&self.to_json(), 2).into_bytes();
        let _ = crate::ipc::write_file_atomic(&Self::path(), &bytes);
    }

    /// The full current shape — what a `settings.get` reply would carry.
    pub fn to_value(&self) -> Value {
        self.to_json()
    }

    /// Unmanaged key read — `extra` keys like `background` ride the file
    /// verbatim; callers that know a key pull it here.
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.extra.get(key)
    }

    /// Unmanaged key write — same verbatim ride; returns whether it changed.
    pub fn set_extra(&mut self, key: &str, value: Value) -> bool {
        if self.extra.get(key) == Some(&value) {
            return false;
        }
        self.extra.insert(key.to_owned(), value);
        true
    }

    pub fn link_syntax(&self) -> &str {
        &self.link_syntax
    }

    /// Returns whether anything changed; call [`Settings::save`] to persist.
    pub fn set_link_syntax(&mut self, value: &str) -> bool {
        set_str(&mut self.link_syntax, value)
    }

    pub fn windows_shell(&self) -> &str {
        &self.windows_shell
    }

    pub fn set_windows_shell(&mut self, value: &str) -> bool {
        set_str(&mut self.windows_shell, value)
    }

    pub fn command_prefix(&self) -> &str {
        &self.command_prefix
    }

    pub fn set_command_prefix(&mut self, value: &str) -> bool {
        set_str(&mut self.command_prefix, value)
    }

    pub fn user_name(&self) -> &str {
        &self.user_name
    }

    pub fn set_user_name(&mut self, value: &str) -> bool {
        set_str(&mut self.user_name, value)
    }

    pub fn background_dim(&self) -> u64 {
        self.background_dim
    }

    pub fn set_background_dim(&mut self, value: u64) -> bool {
        if self.background_dim == value {
            return false;
        }
        self.background_dim = value;
        true
    }

    pub fn background_blur(&self) -> u64 {
        self.background_blur
    }

    pub fn set_background_blur(&mut self, value: u64) -> bool {
        if self.background_blur == value {
            return false;
        }
        self.background_blur = value;
        true
    }

    pub fn ai_provider(&self) -> &str {
        &self.ai_provider
    }

    pub fn set_ai_provider(&mut self, value: &str) -> bool {
        set_str(&mut self.ai_provider, value)
    }

    pub fn ai_model(&self) -> &str {
        &self.ai_model
    }

    /// Stored value is migrated through `Bi` so what we hold matches what a
    /// reload would produce.
    pub fn set_ai_model(&mut self, value: &str) -> bool {
        let value = migrate_ai_model(value);
        if self.ai_model == value {
            return false;
        }
        self.ai_model = value;
        true
    }

    pub fn ai_reasoning_effort(&self) -> &str {
        &self.ai_reasoning_effort
    }

    pub fn set_ai_reasoning_effort(&mut self, value: &str) -> bool {
        set_str(&mut self.ai_reasoning_effort, value)
    }

    pub fn ai_connected_providers(&self) -> &[String] {
        &self.ai_connected_providers
    }

    pub fn set_ai_connected_providers(&mut self, value: Vec<String>) -> bool {
        if self.ai_connected_providers == value {
            return false;
        }
        self.ai_connected_providers = value;
        true
    }

    pub fn local_model(&self) -> &LocalModel {
        &self.local_model
    }

    pub fn set_local_model(&mut self, value: LocalModel) -> bool {
        if self.local_model == value {
            return false;
        }
        self.local_model = value;
        true
    }

    pub fn favorite_widgets(&self) -> &[String] {
        &self.favorite_widgets
    }

    /// The retired-kind filter runs here too, so a set can never store a
    /// kind `mr` would strip on the next read.
    pub fn set_favorite_widgets<I, S>(&mut self, value: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let value: Vec<String> = value
            .into_iter()
            .map(Into::into)
            .filter(|kind| !RETIRED_WIDGETS.contains(&kind.as_str()))
            .collect();
        if self.favorite_widgets == value {
            return false;
        }
        self.favorite_widgets = value;
        true
    }

    /// `(favoriteWidgets??[]).includes(kind)` — the context menu check.
    pub fn is_favorite_widget(&self, kind: &str) -> bool {
        self.favorite_widgets.iter().any(|k| k == kind)
    }

    /// The menu's toggle: `filter(V=>V!==kind)` when present, `[…,kind]`
    /// when absent. Returns whether the kind is a favorite now.
    pub fn toggle_favorite_widget(&mut self, kind: &str) -> bool {
        if self.is_favorite_widget(kind) {
            self.favorite_widgets.retain(|k| k != kind);
            false
        } else {
            self.favorite_widgets.push(kind.to_owned());
            true
        }
    }

    pub fn favorite_terminal_names(&self) -> &[String] {
        &self.favorite_terminal_names
    }

    pub fn set_favorite_terminal_names<I, S>(&mut self, value: I) -> bool
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let value: Vec<String> = value.into_iter().map(Into::into).collect();
        if self.favorite_terminal_names == value {
            return false;
        }
        self.favorite_terminal_names = value;
        true
    }

    pub fn custom_code_agents(&self) -> &[CustomCodeAgent] {
        &self.custom_code_agents
    }

    /// Runs the same validation as load (`Fi`): invalid or duplicate
    /// entries are dropped and the list caps at `MAX_CODE_AGENTS`.
    pub fn set_custom_code_agents(&mut self, value: Vec<CustomCodeAgent>) -> bool {
        let value = sanitize_agents(value);
        if self.custom_code_agents == value {
            return false;
        }
        self.custom_code_agents = value;
        true
    }

    /// The settings UI's add path: `id: crypto.randomUUID()`, name/command
    /// trimmed and validated. `None` when the entry is invalid or the cap
    /// is reached; on success returns the new agent's id.
    pub fn add_custom_code_agent(&mut self, name: &str, command: &str) -> Option<String> {
        if self.custom_code_agents.len() >= MAX_CODE_AGENTS {
            return None;
        }
        let name = name.trim();
        let command = command.trim();
        if !valid_agent_name(name) || !valid_agent_command(command) {
            return None;
        }
        let agent = CustomCodeAgent {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_owned(),
            command: command.to_owned(),
        };
        let id = agent.id.clone();
        self.custom_code_agents.push(agent);
        Some(id)
    }

    /// `customCodeAgents.filter(a=>a.id!==id)` — returns whether one was
    /// removed.
    pub fn remove_custom_code_agent(&mut self, id: &str) -> bool {
        let before = self.custom_code_agents.len();
        self.custom_code_agents.retain(|a| a.id != id);
        self.custom_code_agents.len() != before
    }

    pub fn image_insert_shortcut(&self) -> &str {
        &self.image_insert_shortcut
    }

    pub fn set_image_insert_shortcut(&mut self, value: &str) -> bool {
        set_str(&mut self.image_insert_shortcut, value)
    }

    pub fn auto_approve_permissions(&self) -> bool {
        self.auto_approve_permissions
    }

    pub fn set_auto_approve_permissions(&mut self, value: bool) -> bool {
        if self.auto_approve_permissions == value {
            return false;
        }
        self.auto_approve_permissions = value;
        true
    }
}

/// String field setter — `false` when the value is already stored.
fn set_str(field: &mut String, value: &str) -> bool {
    if field == value {
        return false;
    }
    *field = value.to_owned();
    true
}

/// `raw.remove(key)` → `as_str`, else the `sn` default. The original kept a
/// wrong-typed value verbatim; the port falls back so every read is typed.
fn take_string(raw: &mut Map<String, Value>, key: &str, default: &str) -> String {
    raw.remove(key)
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| default.to_owned())
}

/// Same idea for numbers — accepts `45` and `45.0`, else the default.
fn take_u64(raw: &mut Map<String, Value>, key: &str, default: u64) -> u64 {
    raw.remove(key)
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_f64().map(|f| f.round().max(0.0) as u64))
        })
        .unwrap_or(default)
}

/// The `{...defaults.localModel, ...stored}` merge the settings writer
/// applies, reproduced per field on read.
fn take_local_model(raw: &mut Map<String, Value>) -> LocalModel {
    let default = LocalModel::default();
    let Some(Value::Object(mut obj)) = raw.remove("localModel") else {
        return default;
    };
    LocalModel {
        enabled: obj
            .remove("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.enabled),
        server_bin: take_string(&mut obj, "serverBin", &default.server_bin),
        model_path: take_string(&mut obj, "modelPath", &default.model_path),
        context_size: take_u64(&mut obj, "contextSize", default.context_size),
        gpu_layers: take_u64(&mut obj, "gpuLayers", default.gpu_layers),
        idle_timeout_ms: take_u64(&mut obj, "idleTimeoutMs", default.idle_timeout_ms),
        offload_vision: obj
            .remove("offloadVision")
            .and_then(|v| v.as_bool())
            .unwrap_or(default.offload_vision),
    }
}

/// `[\u0000-\u001f\u007f]` — the control range `Fi` rejects in names and
/// commands.
fn has_control_chars(value: &str) -> bool {
    value
        .chars()
        .any(|c| ('\u{0}'..='\u{1f}').contains(&c) || c == '\u{7f}')
}

/// `/^[a-zA-Z0-9_-]{1,64}$/` on the trimmed id.
fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `name` trimmed: non-empty, ≤ 40 chars, no control characters.
fn valid_agent_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 40 && !has_control_chars(name)
}

/// `command` trimmed: non-empty, ≤ 512 chars, no control characters.
fn valid_agent_command(command: &str) -> bool {
    !command.is_empty() && command.len() <= 512 && !has_control_chars(command)
}

/// `Fi` over typed entries — trims fields, drops invalid ids/names/
/// commands, reserved ids (case-insensitive) and duplicate ids, and stops
/// at `MAX_CODE_AGENTS`.
fn sanitize_agents(list: Vec<CustomCodeAgent>) -> Vec<CustomCodeAgent> {
    let mut agents = Vec::new();
    for agent in list {
        let id = agent.id.trim();
        let name = agent.name.trim();
        let command = agent.command.trim();
        if !valid_agent_id(id)
            || RESERVED_AGENT_IDS.contains(&id.to_lowercase().as_str())
            || agents.iter().any(|a: &CustomCodeAgent| a.id == id)
            || !valid_agent_name(name)
            || !valid_agent_command(command)
        {
            continue;
        }
        agents.push(CustomCodeAgent {
            id: id.to_owned(),
            name: name.to_owned(),
            command: command.to_owned(),
        });
        if agents.len() == MAX_CODE_AGENTS {
            break;
        }
    }
    agents
}

/// `Fi` over raw JSON — non-objects and non-string id/name/command entries
/// are skipped, then the typed sanitizer runs.
fn take_agents(value: Option<Value>) -> Vec<CustomCodeAgent> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let candidates: Vec<CustomCodeAgent> = items
        .iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            Some(CustomCodeAgent {
                id: obj.get("id")?.as_str()?.to_owned(),
                name: obj.get("name")?.as_str()?.to_owned(),
                command: obj.get("command")?.as_str()?.to_owned(),
            })
        })
        .collect();
    sanitize_agents(candidates)
}
