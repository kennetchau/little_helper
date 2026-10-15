//! Harness configuration.
//!
//! Loads `$HARNESS_STATE/config.toml` (git-tracked) over built-in defaults.
//! Unknown keys are ignored; invalid enum values fall back to the default
//! with a stderr warning. This module never panics on bad input.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

pub fn state_dir() -> PathBuf {
    std::env::var("HARNESS_STATE")
        .unwrap_or_else(|_| "/state".into())
        .into()
}

pub fn config_path() -> PathBuf {
    state_dir().join("config.toml")
}

pub const APPROVAL_LEVELS: [&str; 3] = ["off", "ask", "auto"];
pub const SUMMARIZE_MODES: [&str; 3] = ["off", "auto", "manual"];

/// Lenient enum: any value outside the allowed set falls back to the given
/// default with a one-line stderr warning (mirrors Python's `enum_check`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LenientEnum(pub String);

impl LenientEnum {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for LenientEnum {
    fn default() -> Self {
        LenientEnum("auto".into())
    }
}

impl LenientEnum {
    fn check(value: &str, allowed: &[&str], default: &str, where_: &str) -> LenientEnum {
        if allowed.contains(&value) {
            LenientEnum(value.to_string())
        } else {
            crate::logmsg::warn(format!(
                "[config] {where_}: {value:?} invalid (want {allowed:?}), using {default:?}"
            ));
            LenientEnum(default.to_string())
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct BackendConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f64,
    pub max_tokens: u64,
    pub thinking_budget: u64,
}

impl Default for BackendConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:8080/v1".into(),
            api_key: "none".into(),
            model: "default".into(),
            temperature: 0.2,
            max_tokens: 240_000,
            thinking_budget: 8192,
        }
    }
}

/// `summarize` deserialized leniently.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ContextConfig {
    pub max_tokens: u64,
    #[serde(deserialize_with = "de_summarize", default = "default_summarize")]
    pub summarize: LenientEnum,
    pub summarize_threshold: f64,
    pub keep_recent_turns: u32,
    pub summary_max_tokens: u64,
}

fn default_summarize() -> LenientEnum {
    LenientEnum("auto".into())
}

fn de_summarize<'de, D>(d: D) -> Result<LenientEnum, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = String::deserialize(d)?;
    Ok(LenientEnum::check(
        &v,
        &SUMMARIZE_MODES,
        "auto",
        "context.summarize",
    ))
}

impl ContextConfig {
    /// Auto-compaction trigger in prompt tokens:
    /// `floor(max_tokens * summarize_threshold)`, clamped to >= 1.
    ///
    /// Single source of truth for the trigger — used by both the turn gate
    /// (`agent::loop_::check_budget`) and the compaction loop
    /// (`agent::compaction::ensure_budget`) so the two can't drift apart.
    pub fn threshold_tokens(&self) -> u64 {
        (((self.max_tokens as f64) * self.summarize_threshold) as u64).max(1)
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 240_000,
            summarize: LenientEnum("auto".into()),
            summarize_threshold: 0.7,
            keep_recent_turns: 8,
            summary_max_tokens: 4096,
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct WorkspaceConfig {
    pub path: String,
    pub auto_git: bool,
}

pub fn default_workspace_path() -> String {
    std::env::var("HARNESS_WORKSPACE").unwrap_or_else(|_| "/workspace".into())
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            path: default_workspace_path(),
            auto_git: true,
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct LimitsConfig {
    pub max_read_lines: u32,
    pub exec_timeout_sec: u64,
    pub exec_output_chars: usize,
    pub web_max_chars: usize,
    pub exec_network: bool,
    pub max_tool_rounds: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_read_lines: 2000,
            exec_timeout_sec: 300,
            exec_output_chars: 20_000,
            web_max_chars: 50_000,
            exec_network: true,
            max_tool_rounds: 200,
        }
    }
}

/// One approval level inside a map, deserialized leniently.
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ToolsConfig {
    pub enabled: bool,
    #[serde(deserialize_with = "de_approval_map", default)]
    pub approval: HashMap<String, String>,
    #[serde(default)]
    pub limits: LimitsConfig,
}

fn de_approval_map<'de, D>(d: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: HashMap<String, String> = HashMap::deserialize(d)?;
    // Merge over the defaults, like the Python build_config does.
    let mut out: HashMap<String, String> = DEFAULT_APPROVAL.clone();
    for (name, level) in raw {
        let level = if APPROVAL_LEVELS.contains(&level.as_str()) {
            level
        } else {
            let fallback = DEFAULT_APPROVAL
                .get(&name)
                .cloned()
                .unwrap_or_else(|| "ask".to_string());
            crate::logmsg::warn(format!(
                "[config] tools.approval.{name}: {level:?} invalid (want {APPROVAL_LEVELS:?}), using {fallback:?}"
            ));
            fallback
        };
        out.insert(name, level);
    }
    Ok(out)
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            approval: DEFAULT_APPROVAL.clone(),
            limits: LimitsConfig::default(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct SearchConfig {
    pub engine: String,
    pub max_results: u32,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            engine: "ddgs".into(),
            max_results: 5,
        }
    }
}

pub static DEFAULT_APPROVAL: std::sync::LazyLock<HashMap<String, String>> =
    std::sync::LazyLock::new(|| {
        [
            ("list_dir", "auto"),
            ("read_file", "auto"),
            ("grep", "auto"),
            ("glob", "auto"),
            ("git_diff", "auto"),
            ("web_search", "auto"),
            ("web_fetch", "auto"),
            ("write_file", "ask"),
            ("edit_file", "ask"),
            ("run_command", "ask"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    });

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub search: SearchConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendConfig::default(),
            context: ContextConfig::default(),
            workspace: WorkspaceConfig::default(),
            tools: ToolsConfig::default(),
            search: SearchConfig::default(),
        }
    }
}

impl Config {
    pub fn approval_for(&self, name: &str) -> &str {
        self.tools
            .approval
            .get(name)
            .map(|s| s.as_str())
            .unwrap_or("auto")
    }

    /// Whether tools are enabled at all (per-tool `off` filtering happens
    /// in `tools::registry`, which sees registration order).
    pub fn enabled_tools(&self) -> bool {
        self.tools.enabled
    }
}

/// Build a Config from a raw deserialized table (kept separate so tests can
/// exercise the lenient paths without touching the filesystem).
pub fn build_config(raw: serde_json::Value) -> Config {
    serde_json::from_value(raw).unwrap_or_else(|e| {
        crate::logmsg::warn(format!("[config] could not parse config table: {e}; using defaults"));
        Config::default()
    })
}

/// Load config from `path` (default `$HARNESS_STATE/config.toml`) over
/// built-in defaults. A missing file yields pure defaults; a corrupt file
/// yields defaults with a warning. This function never panics.
pub fn load_config() -> Config {
    load_config_from(&config_path())
}

pub fn load_config_from(path: &std::path::Path) -> Config {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Config::default();
    };
    match toml::from_str::<Config>(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            crate::logmsg::warn(format!("[config] {path:?}: {e}; using defaults"));
            Config::default()
        }
    }
}

pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# Harness configuration. Git-tracked in /state.

[backend]
base_url = "http://localhost:8080/v1"
api_key = "none"
model = "default"
temperature = 0.2
max_tokens = 256000
thinking_budget = 8192

[context]
max_tokens = 240000
summarize = "auto"            # off | auto | manual
summarize_threshold = 0.7     # compact when prompt tokens exceed max_tokens * threshold
keep_recent_turns = 8
summary_max_tokens = 4096

[workspace]
# path = "/workspace"
auto_git = true

[tools]
enabled = true

[tools.approval]              # off | ask | auto
list_dir = "auto"
read_file = "auto"
grep = "auto"
glob = "auto"
git_diff = "auto"
web_search = "auto"
web_fetch = "auto"
write_file = "ask"
edit_file = "ask"
run_command = "ask"

[tools.limits]
max_read_lines = 2000
exec_timeout_sec = 300
exec_output_chars = 20000
web_max_chars = 50000
exec_network = true           # false wraps commands in `unshare -n`
max_tool_rounds = 200

[search]
engine = "ddgs"
max_results = 5
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        let c = Config::default();
        assert_eq!(c.backend.base_url, "http://localhost:8080/v1");
        assert_eq!(c.backend.temperature, 0.2);
        assert_eq!(c.context.summarize.as_str(), "auto");
        assert!((c.context.summarize_threshold - 0.7).abs() < f64::EPSILON);
        assert_eq!(c.context.keep_recent_turns, 8);
        assert!(c.workspace.auto_git);
        assert!(c.tools.enabled);
        assert_eq!(c.tools.approval.get("write_file").map(String::as_str), Some("ask"));
        assert_eq!(c.tools.approval.get("list_dir").map(String::as_str), Some("auto"));
        assert!(!c.tools.approval.contains_key("delete_file"));
        assert_eq!(c.search.max_results, 5);
    }

    #[test]
    fn threshold_tokens_is_the_floor_product_clamped() {
        let c = Config::default();
        assert_eq!(c.context.threshold_tokens(), 168_000); // 240_000 * 0.7
        let tiny = Config {
            context: ContextConfig {
                max_tokens: 1,
                summarize_threshold: 0.2,
                ..ContextConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(tiny.context.threshold_tokens(), 1); // clamped, never 0
    }

    #[test]
    fn template_round_trips_to_defaults() {
        let c: Config = toml::from_str(DEFAULT_CONFIG_TEMPLATE).expect("template is valid");
        assert_eq!(c.backend.model, "default");
        assert_eq!(c.backend.max_tokens, 256_000); // template intentionally differs from code default
        assert_eq!(c.context.max_tokens, 240_000);
        assert_eq!(c.tools.approval.get("run_command").map(String::as_str), Some("ask"));
    }

    #[test]
    fn lenient_enums_fall_back_with_warning() {
        let text = r#"
[context]
summarize = "bogus"
[tools.approval]
write_file = "sometimes"
grep = "ask"
"#;
        let c: Config = toml::from_str(text).expect("parses");
        assert_eq!(c.context.summarize.as_str(), "auto");
        assert_eq!(c.tools.approval.get("write_file").map(String::as_str), Some("ask"));
        assert_eq!(c.tools.approval.get("grep").map(String::as_str), Some("ask"));
        // defaults for unmentioned tools survive
        assert_eq!(c.tools.approval.get("list_dir").map(String::as_str), Some("auto"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let c: Config = toml::from_str(
            r#"
[backend]
base_url = "http://x/v1"
mystery_key = 42
[nope]
a = 1
"#,
        )
        .expect("unknown keys ignored");
        assert_eq!(c.backend.base_url, "http://x/v1");
    }

    #[test]
    fn custom_values_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("harness-cfg-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            r#"
[backend]
base_url = "https://llm.example.lan/v1"
api_key = "sk-test"
model = "deepseek-v4"
temperature = 0.5
[context]
max_tokens = 100000
summarize = "manual"
keep_recent_turns = 4
[tools]
enabled = false
[tools.approval]
run_command = "auto"
[tools.limits]
exec_timeout_sec = 60
[search]
max_results = 9
"#,
        )
        .unwrap();
        let c = load_config_from(&p);
        assert_eq!(c.backend.base_url, "https://llm.example.lan/v1");
        assert_eq!(c.backend.api_key, "sk-test");
        assert_eq!(c.backend.model, "deepseek-v4");
        assert!((c.backend.temperature - 0.5).abs() < f64::EPSILON);
        // untouched keys keep defaults
        assert_eq!(c.backend.max_tokens, 240_000);
        assert_eq!(c.context.max_tokens, 100_000);
        assert_eq!(c.context.summarize.as_str(), "manual");
        assert_eq!(c.context.keep_recent_turns, 4);
        assert!(!c.tools.enabled);
        assert_eq!(c.tools.approval.get("run_command").map(String::as_str), Some("auto"));
        // merge: unlisted tool keeps its default
        assert_eq!(c.tools.approval.get("write_file").map(String::as_str), Some("ask"));
        assert_eq!(c.tools.limits.exec_timeout_sec, 60);
        assert_eq!(c.tools.limits.max_tool_rounds, 200);
        assert_eq!(c.search.max_results, 9);
        assert_eq!(c.approval_for("run_command"), "auto");
        assert_eq!(c.approval_for("nonexistent_tool"), "auto");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_level_for_unknown_tool_falls_back_to_ask() {
        let c: Config = toml::from_str(
            r#"[tools.approval]
future_tool = "sometimes"
"#,
        )
        .expect("parses");
        assert_eq!(
            c.tools.approval.get("future_tool").map(String::as_str),
            Some("ask")
        );
    }

    #[test]
    fn corrupt_file_returns_defaults() {
        let dir = std::env::temp_dir().join(format!("harness-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "this is [[ not toml").unwrap();
        let c = load_config_from(&p);
        assert_eq!(c.backend.base_url, "http://localhost:8080/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
