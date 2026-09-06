//! `fore model …` — connect fore to an AI model in one command.
//!
//! fore speaks the OpenAI `/chat/completions` dialect, which every serious provider now
//! exposes. So "connecting a model" is only ever three values: base URL, model id, API key.
//! This module knows the values for the common providers so nobody has to look them up,
//! and edits `[llm]` in config.toml in place (comments and other sections untouched).
//!
//!   fore model                     show what's configured and whether it answers
//!   fore model list                list the presets
//!   fore model use ollama          local, free, private (default)
//!   fore model use groq            cloud; asks for the key, stores it in a 0600 file
//!   fore model use openai --model gpt-4.1-mini
//!   fore model use custom --url http://host:8000/v1 --model my-model
//!   fore model test                one real round trip through the daemon (redaction included)

use crate::config;
use std::io::{IsTerminal, Write};

pub struct Preset {
    pub name: &'static str,
    pub base_url: &'static str,
    pub model: &'static str,
    /// Where the human gets a key. Empty = local, no key.
    pub key_url: &'static str,
    pub key_env: &'static str,
    pub note: &'static str,
}

pub const PRESETS: &[Preset] = &[
    Preset { name: "ollama",     base_url: "http://localhost:11434/v1", model: "qwen2.5-coder:1.5b", key_url: "", key_env: "", note: "local · free · private — `ollama pull qwen2.5-coder:1.5b`" },
    Preset { name: "lmstudio",   base_url: "http://localhost:1234/v1",  model: "qwen2.5-coder-1.5b-instruct", key_url: "", key_env: "", note: "local · free · private — load a model in LM Studio, start its server" },
    Preset { name: "llamacpp",   base_url: "http://localhost:8080/v1",  model: "default", key_url: "", key_env: "", note: "local · llama-server (llama.cpp) — model id is whatever the server reports" },
    Preset { name: "vllm",       base_url: "http://localhost:8000/v1",  model: "Qwen/Qwen2.5-Coder-1.5B-Instruct", key_url: "", key_env: "", note: "self-hosted GPU server" },
    Preset { name: "openai",     base_url: "https://api.openai.com/v1",  model: "gpt-4.1-mini", key_url: "https://platform.openai.com/api-keys", key_env: "OPENAI_API_KEY", note: "cloud · paid" },
    Preset { name: "anthropic",  base_url: "https://api.anthropic.com/v1", model: "claude-haiku-4-5", key_url: "https://console.anthropic.com/settings/keys", key_env: "ANTHROPIC_API_KEY", note: "cloud · paid · via Anthropic's OpenAI-compatible layer" },
    Preset { name: "gemini",     base_url: "https://generativelanguage.googleapis.com/v1beta/openai", model: "gemini-2.5-flash", key_url: "https://aistudio.google.com/apikey", key_env: "GEMINI_API_KEY", note: "cloud · free tier" },
    Preset { name: "groq",       base_url: "https://api.groq.com/openai/v1", model: "llama-3.3-70b-versatile", key_url: "https://console.groq.com/keys", key_env: "GROQ_API_KEY", note: "cloud · free tier · very fast" },
    Preset { name: "openrouter", base_url: "https://openrouter.ai/api/v1", model: "qwen/qwen-2.5-coder-32b-instruct", key_url: "https://openrouter.ai/keys", key_env: "OPENROUTER_API_KEY", note: "cloud · one key, hundreds of models" },
    Preset { name: "github",     base_url: "https://models.github.ai/inference", model: "openai/gpt-4.1-mini", key_url: "https://github.com/settings/tokens", key_env: "GITHUB_TOKEN", note: "cloud · free with a GitHub account · fine-grained PAT with the `models` permission" },
    Preset { name: "mistral",    base_url: "https://api.mistral.ai/v1", model: "codestral-latest", key_url: "https://console.mistral.ai/api-keys", key_env: "MISTRAL_API_KEY", note: "cloud · code-tuned" },
    Preset { name: "deepseek",   base_url: "https://api.deepseek.com/v1", model: "deepseek-chat", key_url: "https://platform.deepseek.com/api_keys", key_env: "DEEPSEEK_API_KEY", note: "cloud · cheap" },
    Preset { name: "together",   base_url: "https://api.together.xyz/v1", model: "Qwen/Qwen2.5-Coder-32B-Instruct", key_url: "https://api.together.xyz/settings/api-keys", key_env: "TOGETHER_API_KEY", note: "cloud · open models" },
    Preset { name: "custom",     base_url: "", model: "", key_url: "", key_env: "", note: "any OpenAI-compatible server: --url … --model … [--key …]" },
];

pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

pub fn print_list() {
    println!("\x1b[1mmodel presets\x1b[0m   (fore model use <name> [--model ID] [--key KEY])\n");
    for p in PRESETS {
        let key = if p.key_env.is_empty() { "" } else { "key " };
        println!("  \x1b[1m{:<11}\x1b[0m {:<34} {:<58} \x1b[2m{}{}\x1b[0m", p.name, if p.model.is_empty() { "-" } else { p.model }, if p.base_url.is_empty() { "(you supply --url)" } else { p.base_url }, key, p.note);
    }
    println!("\nLocal presets are free and nothing leaves your machine. Cloud presets only ever see redacted text (`fore redact` shows exactly what).");
}

pub struct UseOpts {
    pub provider: String,
    pub url: Option<String>,
    pub model: Option<String>,
    pub key: Option<String>,
    pub no_key: bool,
}

/// Write [llm] and restart the daemon. Returns a human summary or an error.
pub fn use_provider(o: UseOpts) -> Result<(), String> {
    let preset = find(&o.provider).ok_or_else(|| format!("unknown provider `{}` — `fore model list` shows the presets, or use `custom --url … --model …`", o.provider))?;
    let base_url = o.url.clone().or_else(|| if preset.base_url.is_empty() { None } else { Some(preset.base_url.to_string()) })
        .ok_or("custom needs --url http://host:port/v1")?;
    let model = o.model.clone().or_else(|| if preset.model.is_empty() { None } else { Some(preset.model.to_string()) })
        .ok_or("custom needs --model <id>")?;
    let base_url = base_url.trim_end_matches('/').to_string();
    let needs_key = !preset.key_env.is_empty() || o.key.is_some();

    // --- key ---------------------------------------------------------------------------------
    let mut key_file: Option<std::path::PathBuf> = None;
    if needs_key && !o.no_key {
        let key = match o.key.clone() {
            Some(k) => Some(k),
            None => {
                // 1. already in the provider's usual env var?  2. ask (only if we have a terminal)
                let from_env = if preset.key_env.is_empty() { None } else { std::env::var(preset.key_env).ok().filter(|s| !s.is_empty()) };
                match from_env {
                    Some(k) => { println!("  · using the key from ${}", preset.key_env); Some(k) }
                    None if std::io::stdin().is_terminal() => {
                        if !preset.key_url.is_empty() { println!("  get a key at: {}", preset.key_url); }
                        print!("  paste API key (input hidden, Enter to skip): ");
                        std::io::stdout().flush().ok();
                        let k = read_secret();
                        if k.is_empty() { None } else { Some(k) }
                    }
                    None => None,
                }
            }
        };
        match key {
            Some(k) => {
                let f = config::config_dir().join("api_key");
                std::fs::create_dir_all(config::config_dir()).map_err(|e| e.to_string())?;
                write_private(&f, &(k.trim().to_string() + "\n"))?;
                println!("  ✔ key saved to {}  (mode 600)", f.display());
                key_file = Some(f);
            }
            None => println!("  ! no key set — add later with: fore model key   (or export {})", if preset.key_env.is_empty() { "FORE_LLM_API_KEY" } else { preset.key_env }),
        }
    }

    // --- config ------------------------------------------------------------------------------
    let path = config::config_path();
    let mut doc = read_or_template(&path)?;
    set_llm(&mut doc, "base_url", &format!("\"{base_url}\""));
    set_llm(&mut doc, "model", &format!("\"{model}\""));
    if let Some(f) = &key_file { set_llm(&mut doc, "api_key_file", &format!("\"{}\"", f.display())); }
    else if o.no_key || preset.key_env.is_empty() { unset_llm(&mut doc, "api_key_file"); unset_llm(&mut doc, "api_key"); }
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(&path, &doc).map_err(|e| format!("write {}: {e}", path.display()))?;
    println!("  ✔ [llm] base_url = {base_url}");
    println!("  ✔ [llm] model    = {model}");
    // Validate what we just wrote parses.
    let (_, warnings) = config::Config::load();
    if !warnings.is_empty() { return Err(format!("config did not parse after edit: {}", warnings.join("; "))); }

    // --- apply --------------------------------------------------------------------------------
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = crate::service::stop();
    match crate::service::start(&exe) { Ok(_) => println!("  ✔ daemon restarted with the new model"), Err(e) => println!("  ! daemon not restarted: {e}") }
    println!();
    print_status();
    Ok(())
}

/// `fore model` with no args: what's configured, does the server answer, is the model there.
pub fn print_status() {
    let (cfg, _) = config::Config::load();
    let which = PRESETS.iter().find(|p| !p.base_url.is_empty() && cfg.llm.base_url.trim_end_matches('/') == p.base_url.trim_end_matches('/')).map(|p| p.name).unwrap_or("custom");
    println!("\x1b[1mmodel\x1b[0m  {}  @  {}  \x1b[2m({which})\x1b[0m", cfg.llm.model, cfg.llm.base_url);
    let key_state = match (&cfg.llm.api_key, &cfg.llm.api_key_file) {
        (Some(_), Some(f)) => format!("key: from {}", f.display()),
        (Some(_), None) => "key: set".into(),
        (None, Some(f)) => format!("key: {} is empty or unreadable", f.display()),
        (None, None) => if which == "ollama" || which == "lmstudio" || which == "llamacpp" || which == "vllm" || cfg.llm.base_url.contains("localhost") || cfg.llm.base_url.contains("127.0.0.1") { "no key (local)".into() } else { "no key — `fore model key`".into() },
    };
    println!("       {key_state}");
    match crate::llm::probe_models(&cfg.llm.base_url, cfg.llm.api_key.as_deref()) {
        Ok(models) if models.is_empty() => println!("  \x1b[32m✔\x1b[0m server reachable"),
        Ok(models) if crate::llm::model_listed(&models, &cfg.llm.model) => println!("  \x1b[32m✔\x1b[0m server reachable, model available"),
        Ok(models) => {
            let shown: Vec<&str> = models.iter().take(6).map(String::as_str).collect();
            println!("  \x1b[33m!\x1b[0m server reachable but `{}` is not in its list", cfg.llm.model);
            println!("    available: {}{}", shown.join(", "), if models.len() > 6 { ", …" } else { "" });
            if which == "ollama" { println!("    → ollama pull {}", cfg.llm.model); } else { println!("    → fore model use {which} --model <one of the above>"); }
        }
        Err(crate::llm::ProbeError::Http(401)) | Err(crate::llm::ProbeError::Http(403)) => println!("  \x1b[1;31m✖\x1b[0m server rejected the key (HTTP 401/403) — `fore model key` to set a new one"),
        Err(crate::llm::ProbeError::Http(404)) => println!("  \x1b[33m!\x1b[0m server has no /models endpoint (fine — `fore model test` does a real request)"),
        Err(e) => {
            println!("  \x1b[1;31m✖\x1b[0m server not reachable: {e}");
            if which == "ollama" { println!("    → start it: ollama serve   (then: ollama pull {})", cfg.llm.model); }
        }
    }
    println!("  \x1b[2mchange: fore model use <ollama|groq|openai|…>   test: fore model test   presets: fore model list\x1b[0m");
}

/// Store a key for the currently configured provider.
pub fn set_key(key: Option<String>) -> Result<(), String> {
    let key = match key {
        Some(k) => k,
        None => {
            if !std::io::stdin().is_terminal() { return Err("no terminal — pass the key: fore model key <KEY>".into()); }
            print!("  paste API key (input hidden): ");
            std::io::stdout().flush().ok();
            read_secret()
        }
    };
    if key.trim().is_empty() { return Err("empty key".into()); }
    let f = config::config_dir().join("api_key");
    std::fs::create_dir_all(config::config_dir()).map_err(|e| e.to_string())?;
    write_private(&f, &(key.trim().to_string() + "\n"))?;
    let path = config::config_path();
    let mut doc = read_or_template(&path)?;
    set_llm(&mut doc, "api_key_file", &format!("\"{}\"", f.display()));
    std::fs::write(&path, &doc).map_err(|e| e.to_string())?;
    println!("  ✔ key saved to {} and referenced from config", f.display());
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let _ = crate::service::stop();
    let _ = crate::service::start(&exe);
    print_status();
    Ok(())
}

// ---------------------------------------------------------------------------
// tiny TOML surgery: change a key inside [llm] without touching anything else
// ---------------------------------------------------------------------------

fn read_or_template(path: &std::path::Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(config::Config::template()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Lines of the `[llm]` table: from its header to the next `[header]` (or EOF).
fn llm_span(lines: &[String]) -> Option<(usize, usize)> {
    let start = lines.iter().position(|l| l.trim() == "[llm]")?;
    let end = lines[start + 1..].iter().position(|l| l.trim_start().starts_with('[') && !l.trim_start().starts_with("[[")).map(|i| start + 1 + i).unwrap_or(lines.len());
    Some((start, end))
}

fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') { return None; }
    let (k, _) = t.split_once('=')?;
    Some(k.trim())
}

pub fn set_llm(doc: &mut String, key: &str, value: &str) {
    let mut lines: Vec<String> = doc.lines().map(String::from).collect();
    let (start, end) = match llm_span(&lines) {
        Some(s) => s,
        None => { lines.insert(0, "[llm]".into()); lines.insert(1, String::new()); (0, 2) }
    };
    // 1. live key → replace, keep any trailing comment
    if let Some(i) = (start + 1..end).find(|&i| key_of(&lines[i]) == Some(key)) {
        let comment = lines[i].split_once(" #").map(|(_, c)| format!("  #{c}")).unwrap_or_default();
        lines[i] = format!("{key} = {value}{comment}");
    } else if let Some(i) = (start + 1..end).find(|&i| { let t = lines[i].trim_start(); t.starts_with('#') && t.trim_start_matches('#').trim_start().starts_with(&format!("{key} ")) }) {
        // 2. commented-out template line → uncomment in place
        lines[i] = format!("{key} = {value}");
    } else {
        // 3. append at the end of the table (before trailing blank lines)
        let mut at = end;
        while at > start + 1 && lines[at - 1].trim().is_empty() { at -= 1; }
        lines.insert(at, format!("{key} = {value}"));
    }
    *doc = lines.join("\n") + "\n";
}

pub fn unset_llm(doc: &mut String, key: &str) {
    let mut lines: Vec<String> = doc.lines().map(String::from).collect();
    if let Some((start, end)) = llm_span(&lines)
        && let Some(i) = (start + 1..end).find(|&i| key_of(&lines[i]) == Some(key)) {
            lines[i] = format!("# {}", lines[i]);
        }
    *doc = lines.join("\n") + "\n";
}

fn write_private(path: &std::path::Path, content: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path).map_err(|e| format!("write {}: {e}", path.display()))?;
    f.write_all(content.as_bytes()).map_err(|e| e.to_string())?;
    // If the file pre-existed with looser permissions, tighten them.
    let _ = std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    Ok(())
}

/// Read a line from the tty with echo off. Falls back to plain read if `stty` is unavailable.
fn read_secret() -> String {
    let off = std::process::Command::new("stty").arg("-echo").stdin(std::process::Stdio::inherit()).status().map(|s| s.success()).unwrap_or(false);
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    if off { let _ = std::process::Command::new("stty").arg("echo").stdin(std::process::Stdio::inherit()).status(); println!(); }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_llm_replaces_live_key_and_keeps_comment() {
        let mut d = "[llm]\nbase_url = \"http://a\"   # local\nmodel = \"m\"\n\n[ui]\nghost_style = \"fg=8\"\n".to_string();
        set_llm(&mut d, "base_url", "\"http://b\"");
        assert!(d.contains("base_url = \"http://b\"  # local"), "{d}");
        assert!(d.contains("[ui]\nghost_style"), "other tables untouched: {d}");
        let c: crate::config::Config = toml::from_str(&d).unwrap();
        assert_eq!(c.llm.base_url, "http://b");
    }

    #[test]
    fn set_llm_uncomments_template_line_or_appends() {
        let mut d = config::Config::template();
        set_llm(&mut d, "api_key_file", "\"/x/key\"");
        assert!(d.contains("\napi_key_file = \"/x/key\""), "{d}");
        assert!(!d.contains("# api_key_file = \"~/.config/fore/api_key\""));
        set_llm(&mut d, "brand_new", "1");
        let c: toml::Value = toml::from_str(&d).unwrap();
        assert_eq!(c["llm"]["brand_new"].as_integer(), Some(1));
        assert_eq!(c["llm"]["api_key_file"].as_str(), Some("/x/key"));
    }

    #[test]
    fn set_llm_creates_table_when_missing() {
        let mut d = "[ui]\nghost_style = \"fg=8\"\n".to_string();
        set_llm(&mut d, "model", "\"x\"");
        let c: crate::config::Config = toml::from_str(&d).unwrap();
        assert_eq!(c.llm.model, "x");
        assert_eq!(c.ui.ghost_style, "fg=8");
    }

    #[test]
    fn unset_comments_out() {
        let mut d = "[llm]\napi_key = \"sk\"\nmodel = \"m\"\n".to_string();
        unset_llm(&mut d, "api_key");
        let c: crate::config::Config = toml::from_str(&d).unwrap();
        assert!(c.llm.api_key.is_none());
        assert_eq!(c.llm.model, "m");
    }

    #[test]
    fn every_preset_has_sane_shape() {
        for p in PRESETS {
            if p.name == "custom" { continue; }
            assert!(p.base_url.starts_with("http"), "{}", p.name);
            assert!(!p.base_url.ends_with('/'), "{}", p.name);
            assert!(!p.model.is_empty(), "{}", p.name);
            assert_eq!(p.key_env.is_empty(), p.key_url.is_empty(), "{}: key_env and key_url go together", p.name);
        }
        assert!(find("Groq").is_some());
        assert!(find("nope").is_none());
    }
}
