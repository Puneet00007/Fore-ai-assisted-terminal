//! `fore doctor`: one command that answers "why isn't it working?"
//!
//! Each check prints ✔ / ! / ✖ with a one-line fix. Exit code 1 if anything is ✖.

use crate::config::{self, Config};
use crate::protocol::{Request, Response};
use crate::service;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct Report {
    pub failures: usize,
}

enum S { Ok, Warn, Fail }

fn line(s: S, what: &str, detail: &str) {
    let (mark, color) = match s { S::Ok => ("✔", "\x1b[32m"), S::Warn => ("!", "\x1b[33m"), S::Fail => ("✖", "\x1b[1;31m") };
    if detail.is_empty() { println!("  {color}{mark}\x1b[0m {what}"); } else { println!("  {color}{mark}\x1b[0m {what}  \x1b[2m{detail}\x1b[0m"); }
}

pub fn run(exe: &std::path::Path) -> Report {
    let mut failures = 0;
    let mut warns = 0;
    macro_rules! ok { ($w:expr, $d:expr) => { line(S::Ok, $w, $d) } }
    macro_rules! warn { ($w:expr, $d:expr) => { { line(S::Warn, $w, $d); warns += 1; } } }
    macro_rules! fail { ($w:expr, $d:expr) => { { line(S::Fail, $w, $d); failures += 1; } } }

    println!("\x1b[1mfore doctor\x1b[0m  v{}\n", env!("CARGO_PKG_VERSION"));

    // --- binary & PATH -------------------------------------------------------------
    println!("\x1b[1mbinary\x1b[0m");
    ok!(&format!("{}", exe.display()), "");
    let bin_dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
    let on_path = std::env::var("PATH").unwrap_or_default().split(':').any(|d| PathBuf::from(d).join("fore").exists());
    let zshrc_rc = std::fs::read_to_string(config::zshrc_path()).unwrap_or_default();
    if on_path { ok!("`fore` is on PATH", "") }
    else if zshrc_rc.contains(&bin_dir) { ok!("`fore` will be on PATH in new shells", &format!("~/.zshrc exports {bin_dir}")) }
    else { fail!("`fore` is not on PATH", &format!("add `export PATH=\"{bin_dir}:$PATH\"` to ~/.zshrc")) }

    // --- shell integration ----------------------------------------------------------
    println!("\n\x1b[1mshell\x1b[0m");
    let zshrc = config::zshrc_path();
    let rc = std::fs::read_to_string(&zshrc).unwrap_or_default();
    if rc.contains("fore init zsh") { ok!("~/.zshrc sources the plugin", "") } else { fail!("~/.zshrc does not source the plugin", "add:  eval \"$(fore init zsh)\"   (run `fore install` to do it for you)") }
    if std::env::var("FORE_PLUGIN").is_ok() { ok!("plugin active in this shell", "") } else { warn!("plugin not active in this shell", "open a new terminal, or run: eval \"$(fore init zsh)\"") }
    match std::env::var("SHELL") { Ok(s) if s.ends_with("zsh") => ok!("login shell is zsh", &s), Ok(s) => warn!("login shell is not zsh", &format!("{s} — fore currently supports zsh; `chsh -s $(which zsh)`")), Err(_) => {} }
    if rc.contains("zsh-autosuggestions") { ok!("zsh-autosuggestions detected", "fore registers itself as its suggestion strategy") }

    // --- config -------------------------------------------------------------------------
    println!("\n\x1b[1mconfig\x1b[0m");
    let (cfg, cfg_warnings) = Config::load();
    let cp = config::config_path();
    if cp.exists() { ok!(&format!("{}", cp.display()), "") } else { warn!("no config file (defaults in use)", "`fore config init` writes a commented template") }
    for w in &cfg_warnings { fail!("config error", w) }

    // --- daemon -------------------------------------------------------------------------
    println!("\n\x1b[1mdaemon\x1b[0m");
    let sock = config::socket_path();
    let alive = service::socket_alive();
    if alive {
        let t0 = Instant::now();
        match call(&sock, &Request::Ping, Duration::from_millis(500)) {
            Ok(r) => ok!(&format!("responding  ({:.1} ms round trip)", t0.elapsed().as_secs_f64() * 1000.0), r.message.as_deref().unwrap_or("")),
            Err(e) => fail!("socket exists but daemon not answering", &format!("{e} — `fore restart`")),
        }
    } else {
        fail!("not running", "`fore start`  (or `fore install` for autostart at login)");
    }
    ok!(&format!("socket {}", sock.display()), "");
    let db = config::db_path();
    if db.exists() {
        let size = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
        ok!(&format!("history {}", db.display()), &crate::preflight::fmt_bytes(size));
    } else { warn!("no history database yet", "it's created on first command") }
    let log = config::log_path();
    if log.exists() {
        // Surface the last error line, if any.
        if let Ok(s) = std::fs::read_to_string(&log)
            && let Some(l) = s.lines().rev().find(|l| l.contains("ERROR") || l.contains("WARN")) {
                let l: String = l.chars().take(120).collect();
                warn!("recent log warning", &l);
            }
    }
    if service::autostart_installed() { ok!("autostart at login", &service::unit_path().display().to_string()) } else { warn!("no autostart at login", "`fore install` sets up launchd/systemd; the plugin also auto-starts the daemon") }

    // --- latency -------------------------------------------------------------------------
    if alive {
        println!("\n\x1b[1mlatency\x1b[0m");
        let mut samples = Vec::new();
        for _ in 0..20 {
            let t0 = Instant::now();
            let _ = call(&sock, &Request::Suggest { prefix: "g".into(), cwd: "/".into(), session: "doctor".into() }, Duration::from_millis(500));
            samples.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = samples[10]; let p99 = samples[19];
        let d = format!("p50 {p50:.2} ms · max {p99:.2} ms  (target < 10 ms)");
        if p99 < 10.0 { ok!("suggest round trip", &d) } else if p99 < 50.0 { warn!("suggest round trip is slow", &d) } else { fail!("suggest round trip is very slow", &format!("{d} — is the disk/CPU saturated? `fore restart`")) }
    }

    // --- model --------------------------------------------------------------------------
    println!("\n\x1b[1mmodel\x1b[0m");
    ok!(&format!("{}  @  {}", cfg.llm.model, cfg.llm.base_url), "");
    let local = cfg.llm.base_url.contains("localhost") || cfg.llm.base_url.contains("127.0.0.1");
    if !local && cfg.llm.api_key.is_none() { warn!("cloud endpoint but no API key", "`fore model key` (or `fore model use <provider>`)") }
    match crate::llm::probe_models(&cfg.llm.base_url, cfg.llm.api_key.as_deref()) {
        Ok(models) => {
            if models.is_empty() { ok!("server reachable", "") }
            else if crate::llm::model_listed(&models, &cfg.llm.model) { ok!("server reachable, model available", "") }
            else {
                let shown: Vec<&str> = models.iter().take(5).map(String::as_str).collect();
                let fix = if cfg.llm.base_url.contains("11434") { format!("`ollama pull {}`", cfg.llm.model) } else { "`fore model use <provider> --model <id>`".to_string() };
                warn!("server reachable but model not listed", &format!("available: {}{}  → {fix}", shown.join(", "), if models.len() > 5 { ", …" } else { "" }))
            }
        }
        Err(crate::llm::ProbeError::Http(401)) | Err(crate::llm::ProbeError::Http(403)) => fail!("model server rejected the API key", "`fore model key` to set a new one"),
        Err(crate::llm::ProbeError::Http(404)) => ok!("server reachable", "(no /models endpoint — `fore model test` does a real request)"),
        Err(e) => {
            let hint = if cfg.llm.base_url.contains("11434") { "start Ollama: `ollama serve` then `ollama pull qwen2.5-coder:1.5b`  (ghost text & pre-flight work without it)" } else { "check with `fore model`, switch with `fore model use <provider>`" };
            warn!("model server not reachable", &format!("{e} — {hint}"))
        }
    }

    // --- undo -----------------------------------------------------------------------------
    println!("\n\x1b[1mundo\x1b[0m");
    let trash = config::trash_dir();
    let (ops, bytes) = crate::undo::trash_size(&trash);
    if cfg.undo.safe_rm { ok!("safe rm enabled", &format!("{ops} restorable operations, {} in {}", crate::preflight::fmt_bytes(bytes), trash.display())) } else { ok!("safe rm disabled", "rm is the real rm") }

    println!();
    if failures == 0 && warns == 0 { println!("\x1b[32mAll good.\x1b[0m"); }
    else if failures == 0 { println!("\x1b[33m{warns} warning(s), nothing blocking.\x1b[0m"); }
    else { println!("\x1b[1;31m{failures} problem(s)\x1b[0m, {warns} warning(s)."); }
    Report { failures }
}

fn call(socket: &PathBuf, req: &Request, timeout: Duration) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut buf = serde_json::to_vec(req).expect("serialize");
    buf.push(b'\n');
    stream.write_all(&buf)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
