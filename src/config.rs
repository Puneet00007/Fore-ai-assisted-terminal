//! Configuration: `~/.config/fore/config.toml`, overridable by environment variables.
//!
//! Precedence (highest first): CLI flag → env var → config file → default.
//! Every field has a default, so a missing file is fine and `fore config init`
//! writes a fully-commented template.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub llm: LlmSection,
    pub preflight: PreflightSection,
    pub history: HistorySection,
    pub undo: UndoSection,
    pub ui: UiSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmSection {
    /// OpenAI-compatible base URL. Ollama: http://localhost:11434/v1
    pub base_url: String,
    pub model: String,
    /// Read from `FORE_LLM_API_KEY` or from `api_key_file` if not set here.
    pub api_key: Option<String>,
    /// Path to a file containing only the key (keeps it out of the config).
    pub api_key_file: Option<PathBuf>,
    pub timeout_s: u64,
    /// After this many consecutive failures the daemon stops trying for `circuit_cooldown_s`.
    pub circuit_failures: u32,
    pub circuit_cooldown_s: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PreflightSection {
    pub enabled: bool,
    /// Check ids to silence, e.g. ["venv", "kube-ctx"].
    pub ignore: Vec<String>,
    /// Branch names that count as protected for git guards.
    pub protected_branches: Vec<String>,
    /// Substrings that mark a kube context / AWS profile / namespace as production.
    pub prod_markers: Vec<String>,
    /// rm previews block at or above this many files…
    pub block_files: usize,
    /// …or this many bytes.
    pub block_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySection {
    /// Rows kept hot in memory for suggestions.
    pub hot_rows: usize,
    /// Commands starting with a space are never recorded (like HIST_IGNORE_SPACE).
    pub ignore_space: bool,
    /// Command prefixes never recorded, e.g. ["history", "fore "].
    pub ignore_prefixes: Vec<String>,
    /// Redact secrets before writing to the database (recommended).
    pub redact_on_write: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UndoSection {
    /// Route `rm` through the trash so `fore undo` can restore.
    pub safe_rm: bool,
    /// Trash entries older than this are purged on daemon start.
    pub keep_days: u64,
    /// Never trash more than this many bytes per rm (falls back to real rm with confirmation).
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSection {
    /// zsh highlight spec for ghost text.
    pub ghost_style: String,
    /// Accept keys are bound in the plugin; this only toggles the → binding.
    pub bind_right_arrow: bool,
}

impl Default for LlmSection {
    fn default() -> Self {
        Self { base_url: "http://localhost:11434/v1".into(), model: "qwen2.5-coder:1.5b".into(), api_key: None, api_key_file: None, timeout_s: 30, circuit_failures: 3, circuit_cooldown_s: 120 }
    }
}
impl Default for PreflightSection {
    fn default() -> Self {
        Self {
            enabled: true, ignore: vec![],
            protected_branches: ["main", "master", "production", "release", "develop"].map(String::from).to_vec(),
            prod_markers: ["prod", "prd", "live"].map(String::from).to_vec(),
            block_files: 100, block_bytes: 100 * 1024 * 1024,
        }
    }
}
impl Default for HistorySection {
    fn default() -> Self {
        Self { hot_rows: 20_000, ignore_space: true, ignore_prefixes: vec![], redact_on_write: true }
    }
}
impl Default for UndoSection {
    fn default() -> Self {
        Self { safe_rm: true, keep_days: 14, max_bytes: 2 * 1024 * 1024 * 1024 }
    }
}
impl Default for UiSection {
    fn default() -> Self {
        Self { ghost_style: "fg=8".into(), bind_right_arrow: true }
    }
}

// ---------------------------------------------------------------------------
// Paths (XDG on Linux, ~/Library on macOS for the service, ~/.config for config everywhere)
// ---------------------------------------------------------------------------

pub fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("fore")
}

pub fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(".local/share")).join("fore")
}

pub fn state_dir() -> PathBuf {
    std::env::var("XDG_STATE_HOME").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| home().join(".local/state")).join("fore")
}

pub fn config_path() -> PathBuf {
    std::env::var("FORE_CONFIG").ok().map(PathBuf::from).unwrap_or_else(|| config_dir().join("config.toml"))
}

pub fn db_path() -> PathBuf {
    std::env::var("FORE_DB").ok().map(PathBuf::from).unwrap_or_else(|| data_dir().join("history.db"))
}

pub fn trash_dir() -> PathBuf {
    data_dir().join("trash")
}

pub fn log_path() -> PathBuf {
    state_dir().join("daemon.log")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("fore.pid")
}

/// Where the socket lives. $XDG_RUNTIME_DIR is a tmpfs owned by the user on Linux;
/// macOS has no equivalent, so we use a 0700 dir under ~/.local/state.
pub fn runtime_dir() -> PathBuf {
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR")
        && !d.is_empty() && Path::new(&d).is_dir() {
            return PathBuf::from(d).join("fore");
        }
    state_dir().join("run")
}

pub fn socket_path() -> PathBuf {
    std::env::var("FORE_SOCKET").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| runtime_dir().join("fore.sock"))
}

/// The rc file zsh actually reads: $ZDOTDIR/.zshrc if set, else ~/.zshrc.
pub fn zshrc_path() -> PathBuf {
    std::env::var("ZDOTDIR").ok().filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(home).join(".zshrc")
}

pub fn alias_file() -> PathBuf {
    config_dir().join("aliases.zsh")
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Config {
    /// File → env overrides. Never fails: a broken file logs a warning and falls back to defaults.
    pub fn load() -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let path = config_path();
        let mut cfg = match std::fs::read_to_string(&path) {
            Ok(s) => match toml::from_str::<Config>(&s) {
                Ok(c) => c,
                Err(e) => { warnings.push(format!("config {}: {e} — using defaults", path.display())); Config::default() }
            },
            Err(_) => Config::default(),
        };
        let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        if let Some(v) = env("FORE_LLM_BASE_URL") { cfg.llm.base_url = v; }
        if let Some(v) = env("FORE_LLM_MODEL") { cfg.llm.model = v; }
        if let Some(v) = env("FORE_LLM_API_KEY") { cfg.llm.api_key = Some(v); }
        if let Some(v) = env("FORE_LLM_TIMEOUT_S").and_then(|s| s.parse().ok()) { cfg.llm.timeout_s = v; }
        if let Some(v) = env("FORE_PREFLIGHT") { cfg.preflight.enabled = v != "0"; }
        if let Some(v) = env("FORE_IGNORE") { cfg.preflight.ignore.extend(v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty())); }
        if let Some(v) = env("FORE_SAFE_RM") { cfg.undo.safe_rm = v != "0"; }
        if cfg.llm.api_key.is_none()
            && let Some(f) = &cfg.llm.api_key_file {
                let f = expand_tilde(f);
                match std::fs::read_to_string(&f) {
                    Ok(k) => cfg.llm.api_key = Some(k.trim().to_string()).filter(|k| !k.is_empty()),
                    Err(e) => warnings.push(format!("api_key_file {}: {e}", f.display())),
                }
            }
        (cfg, warnings)
    }

    pub fn template() -> String {
        let d = Config::default();
        format!(r##"# fore configuration — https://github.com/YOUR_GITHUB_USER/fore
# Every key is optional; these are the defaults. Env vars override this file:
#   FORE_LLM_BASE_URL  FORE_LLM_MODEL  FORE_LLM_API_KEY  FORE_PREFLIGHT=0  FORE_IGNORE=a,b  FORE_SAFE_RM=0

[llm]
# Any OpenAI-compatible endpoint. Easiest: `fore model use <ollama|lmstudio|openai|anthropic|gemini|groq|openrouter|github|…>`
# (fills these three keys in and restarts the daemon). `fore model list` shows every preset.
# Local (free, private):  Ollama http://localhost:11434/v1 · LM Studio http://localhost:1234/v1
# Cloud: OpenAI https://api.openai.com/v1 · Groq https://api.groq.com/openai/v1 · OpenRouter https://openrouter.ai/api/v1
base_url = "{base}"
model = "{model}"
# api_key = "sk-..."                    # or, better:
# api_key_file = "~/.config/fore/api_key"
timeout_s = {timeout}
circuit_failures = {cf}                 # stop calling the model after N consecutive failures…
circuit_cooldown_s = {cc}               # …for this long. Everything else keeps working offline.

[preflight]
enabled = true
ignore = []                             # check ids to silence, e.g. ["venv", "kube-ctx", "main-commit"]
protected_branches = {pb:?}
prod_markers = {pm:?}
block_files = {bf}                      # rm previews require a second Enter at/above this many files…
block_bytes = {bb}                  # …or bytes

[history]
hot_rows = {hr}
ignore_space = true                     # " secret-command" (leading space) is never recorded
ignore_prefixes = []                    # e.g. ["history", "fore "]
redact_on_write = true                  # secrets are masked before they touch the database

[undo]
safe_rm = true                          # rm moves to ~/.local/share/fore/trash; `fore undo` restores
keep_days = {kd}
max_bytes = {mb}                 # larger deletes fall back to real rm (after the usual second Enter)

[ui]
ghost_style = "{gs}"                       # zsh highlight spec for ghost text (fg=8 = grey, fg=242, "fg=8,italic"…)
bind_right_arrow = true
"##,
            base = d.llm.base_url, model = d.llm.model, timeout = d.llm.timeout_s, cf = d.llm.circuit_failures, cc = d.llm.circuit_cooldown_s,
            pb = d.preflight.protected_branches, pm = d.preflight.prod_markers, bf = d.preflight.block_files, bb = d.preflight.block_bytes,
            hr = d.history.hot_rows, kd = d.undo.keep_days, mb = d.undo.max_bytes, gs = d.ui.ghost_style)
    }
}

pub fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") { home().join(rest) } else { p.to_path_buf() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let parsed: Config = toml::from_str(&Config::template()).expect("template must be valid TOML");
        let d = Config::default();
        assert_eq!(parsed.llm.base_url, d.llm.base_url);
        assert_eq!(parsed.preflight.block_files, d.preflight.block_files);
        assert_eq!(parsed.undo.keep_days, d.undo.keep_days);
        assert_eq!(parsed.history.ignore_prefixes, d.history.ignore_prefixes);
        assert_eq!(parsed.llm.circuit_failures, d.llm.circuit_failures);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let c: Config = toml::from_str("[llm]\nmodel = \"llama3\"\n").unwrap();
        assert_eq!(c.llm.model, "llama3");
        assert_eq!(c.llm.base_url, LlmSection::default().base_url);
        assert!(c.preflight.enabled);
    }
}
