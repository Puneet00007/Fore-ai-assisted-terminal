//! The always-on background process.
//!
//! Lifecycle:
//!   1. Load config; open SQLite; load recent history into the in-memory Predictor.
//!   2. Purge expired trash. Write a PID file.
//!   3. Listen on a Unix domain socket (0600, in a 0700 dir).
//!   4. For each connection: read one JSON line, handle it, write one JSON line, close.
//!
//! Concurrency model: the Store (SQLite) and Predictor live behind a single Mutex.
//! Each request holds it for microseconds. LLM calls happen OUTSIDE the lock, in
//! their own tasks, so a slow model never blocks ghost-text suggestions.

use crate::assist::{self, Context, Proposal};
use crate::config::{self, Config};
use crate::insights::{self, Counters};
use crate::llm::{Llm, LlmConfig};
use crate::predict::Predictor;
use crate::preflight;
use crate::protocol::{Request, Response};
use crate::redact::Redactor;
use crate::safety;
use crate::store::{HistoryEntry, Store, now_ms};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicI64, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, watch};
use tracing::{debug, error, info, warn};

/// What we remember about the most recent failure in a session.
struct LastFailure {
    fix: watch::Receiver<Option<Result<Proposal, String>>>,
}

struct State {
    store: Store,
    predictor: Predictor,
    /// session id → what's currently running (so `Done` knows the cwd/cmd)
    inflight: HashMap<String, (String, String, Option<String>)>,
    last_failure: HashMap<String, LastFailure>,
}

/// Circuit breaker for the model: after N consecutive failures, stop trying for a while.
/// Without this, a dead Ollama would add a 30 s timeout to every failed command's
/// speculative fix — burning CPU and making Ctrl-/ hang.
struct Circuit {
    failures: AtomicU32,
    open_until_ms: AtomicI64,
    threshold: u32,
    cooldown_ms: i64,
}

impl Circuit {
    fn is_open(&self) -> bool {
        now_ms() < self.open_until_ms.load(Ordering::Relaxed)
    }
    fn record(&self, ok: bool) {
        if ok {
            self.failures.store(0, Ordering::Relaxed);
        } else {
            let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
            if n >= self.threshold {
                self.open_until_ms.store(now_ms() + self.cooldown_ms, Ordering::Relaxed);
                warn!("model unreachable {n}× — pausing AI features for {}s", self.cooldown_ms / 1000);
            }
        }
    }
}

pub struct Shared {
    state: Mutex<State>,
    redactor: Redactor,
    llm: Llm,
    circuit: Circuit,
    cfg: Config,
    os: String,
    shell: String,
    started_ms: i64,
}

pub async fn run(socket_path: &Path, db_path: &Path, cfg: Config) -> anyhow_lite::Result<()> {
    // --- 1. Storage + predictor -------------------------------------------------
    let store = Store::open(db_path).map_err(|e| format!("open db {}: {e}", db_path.display()))?;
    let history = store.recent(cfg.history.hot_rows).map_err(|e| format!("load history: {e}"))?;
    info!(rows = history.len(), db = %db_path.display(), "loaded history");

    let llm_cfg = LlmConfig { base_url: cfg.llm.base_url.clone(), model: cfg.llm.model.clone(), api_key: cfg.llm.api_key.clone(), timeout: std::time::Duration::from_secs(cfg.llm.timeout_s) };
    info!(model = %llm_cfg.model, base_url = %llm_cfg.base_url, "llm configured");

    // --- 2. Housekeeping -------------------------------------------------------------
    let trash = config::trash_dir();
    let _ = std::fs::create_dir_all(&trash);
    let (n, b) = crate::undo::purge(&trash, cfg.undo.keep_days, false);
    if n > 0 { info!(ops = n, bytes = b, "purged expired trash"); }
    let _ = std::fs::create_dir_all(config::runtime_dir());
    let _ = std::fs::write(config::pid_path(), std::process::id().to_string());

    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            store,
            predictor: Predictor::new(history, cfg.history.hot_rows),
            inflight: HashMap::new(),
            last_failure: HashMap::new(),
        }),
        redactor: Redactor::from_env(),
        llm: Llm::new(llm_cfg),
        circuit: Circuit { failures: AtomicU32::new(0), open_until_ms: AtomicI64::new(0), threshold: cfg.llm.circuit_failures.max(1), cooldown_ms: (cfg.llm.circuit_cooldown_s as i64) * 1000 },
        os: detect_os(),
        shell: std::env::var("SHELL").unwrap_or_else(|_| "zsh".into()),
        started_ms: now_ms(),
        cfg,
    });

    // --- 3. Socket --------------------------------------------------------------
    if socket_path.exists() {
        // Another daemon alive? Don't steal its socket.
        if tokio::net::UnixStream::connect(socket_path).await.is_ok() {
            return Err(format!("another fore daemon is already listening on {}", socket_path.display()).into());
        }
        std::fs::remove_file(socket_path).ok();
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }
    let listener = UnixListener::bind(socket_path).map_err(|e| format!("bind {}: {e}", socket_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    }
    info!(socket = %socket_path.display(), pid = std::process::id(), "fore daemon listening");

    // --- 4. Accept loop ---------------------------------------------------------
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let shared = Arc::clone(&shared);
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle(stream, shared, shutdown_tx).await {
                                warn!("connection error: {e}");
                            }
                        });
                    }
                    Err(e) => error!("accept failed: {e}"),
                }
            }
            _ = shutdown_rx.recv() => { info!("shutdown requested"); break; }
            _ = tokio::signal::ctrl_c() => { info!("ctrl-c"); break; }
            _ = async { match sigterm.as_mut() { Some(s) => { s.recv().await; } None => std::future::pending::<()>().await } } => { info!("sigterm"); break; }
        }
    }

    std::fs::remove_file(socket_path).ok();
    std::fs::remove_file(config::pid_path()).ok();
    Ok(())
}

async fn handle(
    stream: UnixStream,
    shared: Arc<Shared>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
) -> std::io::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    let Some(line) = lines.next_line().await? else { return Ok(()) };
    let t0 = Instant::now();

    let response = match serde_json::from_str::<Request>(&line) {
        Ok(req) => {
            let is_shutdown = matches!(req, Request::Shutdown);
            let resp = dispatch(req, &shared, t0).await;
            if is_shutdown {
                let _ = shutdown_tx.try_send(());
            }
            resp
        }
        Err(e) => Response::error(format!("bad request: {e}"), t0.elapsed().as_micros() as u64),
    };

    let mut out = serde_json::to_vec(&response).unwrap_or_default();
    out.push(b'\n');
    wr.write_all(&out).await?;
    wr.shutdown().await?;
    Ok(())
}

/// A human-readable reason the model can't be used right now, or None if it can.
fn model_unavailable(shared: &Shared) -> Option<String> {
    if shared.circuit.is_open() {
        let secs = (shared.circuit.open_until_ms.load(Ordering::Relaxed) - now_ms()).max(0) / 1000;
        return Some(format!("model at {} unreachable — AI paused for {secs}s (ghost text & pre-flight still work). `fore doctor` to diagnose.", shared.cfg.llm.base_url));
    }
    None
}

fn should_record(cfg: &Config, cmd: &str) -> bool {
    if cfg.history.ignore_space && cmd.starts_with(' ') { return false; }
    let t = cmd.trim_start();
    if t.is_empty() { return false; }
    !cfg.history.ignore_prefixes.iter().any(|p| t.starts_with(p.as_str()))
}

async fn dispatch(req: Request, shared: &Arc<Shared>, t0: Instant) -> Response {
    let us = |t0: Instant| t0.elapsed().as_micros() as u64;

    match req {
        Request::Ping => {
            let st = shared.state.lock().await;
            Response::message(format!("pong  v{}  pid {}  up {}  rows {}  model {}{}",
                env!("CARGO_PKG_VERSION"), std::process::id(), preflight::fmt_dur((now_ms() - shared.started_ms) as u64),
                st.predictor.len(), shared.cfg.llm.model,
                if shared.circuit.is_open() { "  [AI paused]" } else { "" }), us(t0))
        }
        Request::Shutdown => Response::message("bye", us(t0)),

        Request::Reload => {
            let mut st = shared.state.lock().await;
            match st.store.recent(shared.cfg.history.hot_rows) {
                Ok(h) => { let n = h.len(); st.predictor.replace(h); Response::message(format!("reloaded {n} rows"), us(t0)) }
                Err(e) => Response::error(format!("db: {e}"), us(t0)),
            }
        }

        Request::Exec { cmd, cwd, session, git_branch } => {
            if !should_record(&shared.cfg, &cmd) {
                return Response::ok(us(t0));
            }
            let raw = cmd.trim().to_string();
            // Redact BEFORE the database sees it. The history file must never be the leak.
            let cmd = if shared.cfg.history.redact_on_write { shared.redactor.redact(&raw) } else { raw.clone() };
            let mut st = shared.state.lock().await;
            match st.store.record_exec(&session, &cmd, &cwd, git_branch.as_deref()) {
                Ok(_) => {
                    st.predictor.push(HistoryEntry { cmd: cmd.clone(), cwd: cwd.clone(), exit_code: None, ts_ms: now_ms(), duration_ms: None });
                    st.inflight.insert(session, (cmd, cwd, git_branch));
                    debug!(rows = st.predictor.len(), "exec recorded");
                    Response::ok(us(t0))
                }
                Err(e) => Response::error(format!("db: {e}"), us(t0)),
            }
        }

        Request::Done { session, exit_code, duration_ms, stderr_tail } => {
            let mut st = shared.state.lock().await;
            let closed = match st.store.record_done(&session, exit_code, duration_ms) {
                Ok(c) => c,
                Err(e) => return Response::error(format!("db: {e}"), us(t0)),
            };
            if closed {
                st.predictor.mark_done(exit_code, duration_ms);
            }
            let inflight = st.inflight.remove(&session);

            // Speculative fix: the user hasn't asked yet, but they probably will.
            if exit_code != 0 && exit_code != 130 /* Ctrl-C */ && exit_code != 148 /* Ctrl-Z */ && !shared.circuit.is_open()
                && let Some((cmd, cwd, git_branch)) = inflight {
                    let stderr_tail = stderr_tail.unwrap_or_default();
                    let recent = st.predictor.recent_in(&cwd, 10);
                    let (tx, rx) = watch::channel(None);
                    st.last_failure.insert(session.clone(), LastFailure { fix: rx });
                    drop(st); // release the lock before the slow part
                    let shared = Arc::clone(shared);
                    tokio::spawn(async move {
                        let t = Instant::now();
                        let ctx = Context {
                            cwd: &cwd, git_branch: git_branch.as_deref(), os: &shared.os, shell: &shared.shell,
                            project_kind: assist::detect_project(&cwd), recent: &recent,
                        };
                        let r = assist::fix(&shared.llm, &shared.redactor, &ctx, &cmd, exit_code, &stderr_tail).await;
                        shared.circuit.record(r.is_ok());
                        match &r {
                            Ok(p) => info!(took_ms = t.elapsed().as_millis(), fix = %p.cmd, "speculative fix ready"),
                            Err(e) => warn!(took_ms = t.elapsed().as_millis(), "speculative fix failed: {e}"),
                        }
                        let _ = tx.send(Some(r));
                    });
                }
            Response::ok(us(t0))
        }

        Request::Fix { session } => {
            if let Some(msg) = model_unavailable(shared) {
                return Response::error(msg, us(t0));
            }
            let mut rx = {
                let st = shared.state.lock().await;
                let _ = st.store.bump_counter("fix_requested", 1);
                match st.last_failure.get(&session) {
                    Some(lf) => lf.fix.clone(),
                    None => return Response::error("no failed command in this session yet", us(t0)),
                }
            };
            let already = rx.borrow().is_some();
            if !already {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(shared.cfg.llm.timeout_s), rx.wait_for(|v| v.is_some())).await;
            }
            let result = rx.borrow().clone();
            match result {
                Some(Ok(mut p)) => { p.cached = already; Response::proposal(p, us(t0)) }
                Some(Err(e)) => Response::error(friendly_llm_error(&e, &shared.cfg), us(t0)),
                None => Response::error("fix is still computing; try again", us(t0)),
            }
        }

        Request::Nl { session: _, cwd, request, git_branch } => {
            if let Some(msg) = model_unavailable(shared) {
                return Response::error(msg, us(t0));
            }
            let recent = {
                let st = shared.state.lock().await;
                let _ = st.store.bump_counter("ask_requested", 1);
                st.predictor.recent_in(&cwd, 8)
            };
            let ctx = Context {
                cwd: &cwd, git_branch: git_branch.as_deref(), os: &shared.os, shell: &shared.shell,
                project_kind: assist::detect_project(&cwd), recent: &recent,
            };
            let r = assist::nl(&shared.llm, &shared.redactor, &ctx, &request).await;
            shared.circuit.record(r.is_ok());
            match r {
                Ok(p) => Response::proposal(p, us(t0)),
                Err(e) => Response::error(friendly_llm_error(&e, &shared.cfg), us(t0)),
            }
        }

        Request::Assess { cmd } => Response::assessment(safety::assess(&cmd), us(t0)),

        Request::Preflight { session: _, cmd, cwd, mut env } => {
            if !shared.cfg.preflight.enabled {
                return Response::preflight(preflight::Preflight { findings: vec![], risk: safety::Risk::ReadOnly, took_us: 0 }, us(t0));
            }
            // Merge config-level ignores with the shell's.
            let mut ign: Vec<String> = shared.cfg.preflight.ignore.clone();
            if let Some(e) = env.ignore.take() { ign.extend(e.split(',').map(|s| s.trim().to_string())); }
            env.ignore = Some(ign.join(","));
            let opts = preflight::Options {
                protected_branches: shared.cfg.preflight.protected_branches.clone(),
                prod_markers: shared.cfg.preflight.prod_markers.clone(),
                block_files: shared.cfg.preflight.block_files,
                block_bytes: shared.cfg.preflight.block_bytes,
                safe_rm: shared.cfg.undo.safe_rm,
            };
            let hist: Vec<HistoryEntry> = {
                let st = shared.state.lock().await;
                st.predictor.all().iter().filter(|e| e.cmd == cmd).take(20).cloned().collect()
            };
            let cmd2 = cmd.clone();
            let cwd2 = cwd.clone();
            let mut pf = tokio::task::spawn_blocking(move || preflight::run(&cmd2, &cwd2, &env, &hist, &opts))
                .await
                .unwrap_or_else(|_| preflight::Preflight { findings: vec![], risk: safety::Risk::Mutating, took_us: 0 });
            for (target, size) in preflight::truncating_redirect_targets(&cmd, &cwd) {
                pf.findings.insert(0, preflight::Finding {
                    severity: preflight::Severity::Warn, check: "truncate".into(),
                    text: format!("`> {target}` overwrites an existing {} file — did you mean `>>`?", preflight::fmt_bytes(size)),
                });
            }
            if pf.blocks() {
                let st = shared.state.lock().await;
                let _ = st.store.bump_counter("blocks_shown", 1);
            }
            Response::preflight(pf, us(t0))
        }

        Request::Accepted { session: _, chars } => {
            let st = shared.state.lock().await;
            let _ = st.store.bump_counter("suggest_accepted", 1);
            let _ = st.store.bump_counter("chars_saved", chars);
            Response::ok(us(t0))
        }

        Request::Stats => {
            let st = shared.state.lock().await;
            let counters = Counters {
                suggest_shown: st.store.counter("suggest_shown").unwrap_or(0),
                suggest_accepted: st.store.counter("suggest_accepted").unwrap_or(0),
                chars_saved: st.store.counter("chars_saved").unwrap_or(0),
                fix_requested: st.store.counter("fix_requested").unwrap_or(0),
                ask_requested: st.store.counter("ask_requested").unwrap_or(0),
                blocks_shown: st.store.counter("blocks_shown").unwrap_or(0),
            };
            let sessions = st.store.session_count().unwrap_or(0) as usize;
            let s = insights::stats(st.predictor.all(), sessions, now_ms(), &counters);
            Response::stats(s, us(t0))
        }

        Request::Aliases { existing } => {
            let st = shared.state.lock().await;
            let hist = st.predictor.all();
            let oldest = hist.iter().map(|e| e.ts_ms).min().unwrap_or(now_ms());
            let days = ((now_ms() - oldest) as f64 / 86_400_000.0).max(1.0);
            Response::aliases(insights::mine_aliases(hist, &existing, days), us(t0))
        }

        Request::Suggest { prefix, cwd, .. } => {
            let st = shared.state.lock().await;
            let s = st.predictor.suggest(&prefix, &cwd, now_ms());
            if s.is_some() && prefix.len() == 1 {
                let _ = st.store.bump_counter("suggest_shown", 1);
            }
            Response::suggestion(s, us(t0))
        }
    }
}

/// Turn reqwest/HTTP noise into something a person can act on.
fn friendly_llm_error(e: &str, cfg: &Config) -> String {
    let low = e.to_ascii_lowercase();
    if low.contains("connection refused") || low.contains("error trying to connect") || low.contains("dns") {
        if cfg.llm.base_url.contains("11434") {
            return format!("no model server at {} — start Ollama (`ollama serve`) and `ollama pull {}`, or set [llm] in `fore config edit`", cfg.llm.base_url, cfg.llm.model);
        }
        return format!("cannot reach model server at {} — check [llm].base_url in `fore config edit`", cfg.llm.base_url);
    }
    if low.contains("404") && low.contains("model") || low.contains("not found") {
        return format!("model `{}` not found on the server — `ollama pull {}` or change [llm].model", cfg.llm.model, cfg.llm.model);
    }
    if low.contains("401") || low.contains("403") || low.contains("api key") || low.contains("unauthorized") {
        return "model server rejected the API key — set FORE_LLM_API_KEY or [llm].api_key_file".into();
    }
    if low.contains("timed out") || low.contains("timeout") {
        return format!("model took longer than {}s — a smaller model or a higher [llm].timeout_s helps", cfg.llm.timeout_s);
    }
    if low.contains("unparseable") {
        return "model replied in an unexpected format — try again, or use a code-tuned model (qwen2.5-coder, codellama)".into();
    }
    e.to_string()
}

fn detect_os() -> String {
    if let Ok(s) = std::fs::read_to_string("/etc/os-release")
        && let Some(l) = s.lines().find(|l| l.starts_with("PRETTY_NAME=")) {
            return l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string();
        }
    if cfg!(target_os = "macos")
        && let Ok(o) = std::process::Command::new("sw_vers").arg("-productVersion").output() {
            return format!("macOS {}", String::from_utf8_lossy(&o.stdout).trim());
        }
    std::env::consts::OS.to_string()
}

pub mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
