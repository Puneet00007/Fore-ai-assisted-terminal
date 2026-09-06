//! `fore` — a local-first, latency-obsessed shell copilot.
//!
//! One binary, three roles:
//!   - `fore daemon`                       the long-lived server
//!   - `fore exec|done|suggest|…`          thin clients the shell plugin calls
//!   - `fore start|doctor|install|undo|…`  things a human runs
//!
//! Client commands are deliberately dumb: build a JSON request, send it over the
//! socket, print the reply. All intelligence lives in the daemon.

mod assist;
mod config;
mod daemon;
mod doctor;
mod import;
mod insights;
mod llm;
mod predict;
mod preflight;
mod protocol;
mod providers;
mod redact;
mod safety;
mod service;
mod store;
mod undo;

use clap::{Parser, Subcommand};
use protocol::{Request, Response};
use safety::Risk;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(name = "fore", version, about = "A local-first shell copilot: ghost text, error fixes, pre-flight checks, undo")]
struct Cli {
    /// Unix socket path (default: $XDG_RUNTIME_DIR/fore/fore.sock)
    #[arg(long, global = true, env = "FORE_SOCKET")]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    // ---- lifecycle -------------------------------------------------------------
    /// Start the daemon in the background
    Start,
    /// Stop the daemon
    Stop,
    /// Restart the daemon (after upgrading the binary or editing config)
    Restart,
    /// Is the daemon running?
    Status,
    /// Run the daemon in the foreground (used by launchd/systemd)
    Daemon {
        /// SQLite database path (default: ~/.local/share/fore/history.db)
        #[arg(long, env = "FORE_DB")]
        db: Option<PathBuf>,
    },
    /// Ask the daemon to exit (works even if it wasn't started by `fore start`)
    Shutdown,

    // ---- setup -----------------------------------------------------------------
    /// Set up everything: ~/.zshrc line, config template, autostart at login, then doctor
    Install {
        /// Don't touch ~/.zshrc
        #[arg(long)] no_zshrc: bool,
        /// Don't install the launchd/systemd service
        #[arg(long)] no_service: bool,
    },
    /// Remove autostart, the ~/.zshrc line and (optionally) all data
    Uninstall {
        /// Also delete history, trash, config and logs
        #[arg(long)] purge: bool,
    },
    /// Diagnose the installation
    Doctor,
    /// Show / create / edit the config file
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Connect an AI model: `fore model use ollama|openai|groq|…`, `fore model test`
    Model {
        #[command(subcommand)]
        action: Option<ModelAction>,
    },
    /// Print the zsh plugin (usage: eval "$(fore init zsh)")
    Init { shell: String },

    // ---- daily use -------------------------------------------------------------
    /// Restore the most recent safe-rm operation (or a specific id)
    Undo {
        /// Operation id (prefix ok) from `fore trash list`
        id: Option<String>,
    },
    /// Manage the safe-rm trash
    Trash {
        #[command(subcommand)]
        action: TrashAction,
    },
    /// Safe rm: moves to trash instead of deleting (the plugin aliases rm to this)
    Rm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Import your existing ~/.zsh_history / ~/.bash_history so suggestions work on day one
    Import {
        /// History file(s); default: auto-detect zsh + bash history in $HOME
        files: Vec<PathBuf>,
        /// Parse as this shell's format (zsh|bash); default: guessed from the filename
        #[arg(long)] shell: Option<String>,
    },
    /// Usage statistics: acceptance, failures, slowest commands, time saved
    Stats {
        #[arg(long)] json: bool,
    },
    /// Alias proposals mined from your history
    Aliases {
        #[arg(long)] json: bool,
        /// Append the proposals as `alias` lines to ~/.config/fore/aliases.zsh
        #[arg(long)] install: bool,
        /// Only install this many
        #[arg(long)] top: Option<usize>,
    },
    /// Classify a command's risk without running it
    Check {
        #[arg(long)] json: bool,
        cmd: Vec<String>,
    },
    /// Show what the redactor would send to a model for the given text (audit tool)
    Redact { text: Vec<String> },
    /// Health check; prints round-trip latency
    Ping,

    // ---- called by the plugin ---------------------------------------------------
    /// Record that a command is about to run
    #[command(hide = true)]
    Exec {
        #[arg(long)] session: String,
        #[arg(long)] cwd: String,
        #[arg(long)] git_branch: Option<String>,
        cmd: String,
    },
    /// Record the outcome of the last command
    #[command(hide = true)]
    Done {
        #[arg(long)] session: String,
        #[arg(long)] exit_code: i32,
        #[arg(long, default_value_t = 0)] duration_ms: u64,
        #[arg(long)] stderr_file: Option<PathBuf>,
    },
    /// Best completion for a prefix
    #[command(hide = true)]
    Suggest {
        #[arg(long)] session: String,
        #[arg(long)] cwd: String,
        prefix: String,
    },
    /// Why did the last command fail, and what should I run? (Ctrl-/)
    Fix {
        #[arg(long)] session: String,
        #[arg(long)] json: bool,
        /// Line-based output for the shell plugin (no jq needed)
        #[arg(long)] shell: bool,
    },
    /// English → command (Ctrl-Space)
    Ask {
        #[arg(long)] session: String,
        #[arg(long)] cwd: String,
        #[arg(long)] git_branch: Option<String>,
        #[arg(long)] json: bool,
        #[arg(long)] shell: bool,
        request: Vec<String>,
    },
    /// Guards + previews + insights for a command about to run (called on Enter)
    #[command(hide = true)]
    Preflight {
        #[arg(long)] session: String,
        #[arg(long)] cwd: String,
        #[arg(long)] json: bool,
        #[arg(long)] shell: bool,
        cmd: String,
    },
    /// Record that a ghost-text suggestion was accepted
    #[command(hide = true)]
    Accepted {
        #[arg(long)] session: String,
        #[arg(long)] chars: u64,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Write a commented template to ~/.config/fore/config.toml (won't overwrite)
    Init,
    /// Open the config in $EDITOR
    Edit,
    /// Print the effective configuration (file + env overrides)
    Show,
    /// Print where fore keeps things
    Paths,
}

#[derive(Subcommand)]
enum ModelAction {
    /// What's configured, and does the server answer? (default)
    Status,
    /// List the built-in provider presets
    List,
    /// Switch provider/model and restart the daemon
    Use {
        /// ollama | lmstudio | llamacpp | vllm | openai | anthropic | gemini | groq | openrouter | github | mistral | deepseek | together | custom
        provider: String,
        /// Override the preset's model id
        #[arg(long)] model: Option<String>,
        /// Base URL (required for `custom`), e.g. http://host:8000/v1
        #[arg(long)] url: Option<String>,
        /// API key (otherwise: the provider's usual env var, or an interactive hidden prompt)
        #[arg(long)] key: Option<String>,
        /// Don't ask for a key
        #[arg(long)] no_key: bool,
    },
    /// Store an API key for the current provider (hidden prompt if omitted)
    Key { key: Option<String> },
    /// One real round trip through the daemon: English → command
    Test {
        /// What to ask; default: "list files modified today"
        request: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TrashAction {
    /// List restorable operations
    List,
    /// Delete expired operations (or everything with --all)
    Purge {
        #[arg(long)] all: bool,
    },
}

fn main() {
    // `fore stats | head` must not print a Rust panic when the reader goes away.
    unsafe { libc_signal(13 /* SIGPIPE */, 0 /* SIG_DFL */); }
    let cli = Cli::parse();
    let socket = cli.socket.clone().unwrap_or_else(config::socket_path);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("fore"));

    match cli.cmd {
        // ---- lifecycle ---------------------------------------------------------
        Cmd::Start => match service::start(&exe) {
            Ok(pid) => println!("fore daemon started (pid {pid}), log: {}", config::log_path().display()),
            Err(e) if e == "already running" => println!("fore daemon already running"),
            Err(e) => die(&e),
        },
        Cmd::Stop => match service::stop() {
            Ok(()) => println!("fore daemon stopped"),
            Err(e) => die(&e),
        },
        Cmd::Restart => {
            let _ = service::stop();
            if service::socket_alive() { let _ = call(&socket, &Request::Shutdown, FAST); std::thread::sleep(Duration::from_millis(300)); }
            match service::start(&exe) { Ok(pid) => println!("fore daemon restarted (pid {pid})"), Err(e) => die(&e) }
        }
        Cmd::Status => {
            let t0 = Instant::now();
            match call(&socket, &Request::Ping, FAST) {
                Ok(r) => { println!("running  {}  ({:.1} ms)", r.message.unwrap_or_default(), t0.elapsed().as_secs_f64() * 1000.0); }
                Err(_) => { println!("not running"); std::process::exit(1); }
            }
        }
        Cmd::Daemon { db } => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
                .with_target(false)
                .with_ansi(false)
                .init();
            let (cfg, warnings) = config::Config::load();
            for w in warnings { tracing::warn!("{w}"); }
            let db = db.unwrap_or_else(config::db_path);
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("tokio runtime");
            if let Err(e) = rt.block_on(daemon::run(&socket, &db, cfg)) {
                eprintln!("fore daemon: {e}");
                std::process::exit(1);
            }
        }
        Cmd::Shutdown => match call(&socket, &Request::Shutdown, FAST) {
            Ok(_) => println!("daemon stopping"),
            Err(e) => eprintln!("daemon not reachable: {e}"),
        },

        // ---- setup -----------------------------------------------------------------
        Cmd::Install { no_zshrc, no_service } => install(&exe, no_zshrc, no_service),
        Cmd::Uninstall { purge } => uninstall(purge),
        Cmd::Doctor => {
            let r = doctor::run(&exe);
            std::process::exit(if r.failures > 0 { 1 } else { 0 });
        }
        Cmd::Config { action } => match action.unwrap_or(ConfigAction::Show) {
            ConfigAction::Init => {
                let p = config::config_path();
                if p.exists() { println!("{} already exists (delete it first to regenerate)", p.display()); return; }
                std::fs::create_dir_all(p.parent().unwrap()).ok();
                std::fs::write(&p, config::Config::template()).unwrap_or_else(|e| die(&format!("write {}: {e}", p.display())));
                println!("wrote {}", p.display());
            }
            ConfigAction::Edit => {
                let p = config::config_path();
                if !p.exists() { std::fs::create_dir_all(p.parent().unwrap()).ok(); let _ = std::fs::write(&p, config::Config::template()); }
                let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
                let st = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&p).status();
                if st.map(|s| s.success()).unwrap_or(false) {
                    match config::Config::load() { (_, w) if w.is_empty() => println!("config ok — `fore restart` to apply"), (_, w) => { for x in w { eprintln!("✖ {x}"); } std::process::exit(1); } }
                }
            }
            ConfigAction::Show => {
                let (cfg, warnings) = config::Config::load();
                for w in warnings { eprintln!("! {w}"); }
                let mut shown = cfg.clone();
                if shown.llm.api_key.is_some() { shown.llm.api_key = Some("<set>".into()); }
                print!("{}", toml::to_string_pretty(&shown).unwrap_or_default());
            }
            ConfigAction::Paths => {
                println!("config   {}", config::config_path().display());
                println!("history  {}", config::db_path().display());
                println!("trash    {}", config::trash_dir().display());
                println!("socket   {}", config::socket_path().display());
                println!("log      {}", config::log_path().display());
                println!("aliases  {}", config::alias_file().display());
                println!("service  {}", service::unit_path().display());
            }
        },
        Cmd::Model { action } => match action.unwrap_or(ModelAction::Status) {
            ModelAction::Status => providers::print_status(),
            ModelAction::List => providers::print_list(),
            ModelAction::Use { provider, model, url, key, no_key } => {
                println!("\x1b[1mfore model use {provider}\x1b[0m\n");
                if let Err(e) = providers::use_provider(providers::UseOpts { provider, url, model, key, no_key }) { die(&e); }
            }
            ModelAction::Key { key } => { if let Err(e) = providers::set_key(key) { die(&e); } }
            ModelAction::Test { request } => {
                let request = if request.is_empty() { "list files modified today".to_string() } else { request.join(" ") };
                let (cfg, _) = config::Config::load();
                println!("\x1b[2masking {} @ {} … (only redacted text leaves this machine)\x1b[0m", cfg.llm.model, cfg.llm.base_url);
                let t0 = Instant::now();
                let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                match call(&socket, &Request::Nl { session: "model-test".into(), cwd, request: request.clone(), git_branch: None }, SLOW) {
                    Ok(r) => match r.proposal {
                        Some(p) => {
                            println!("  \x1b[32m✔\x1b[0m \x1b[1m{}\x1b[0m", request);
                            println!("    → {}", p.cmd);
                            if !p.note.is_empty() && p.note != "-" { println!("      {}", p.note); }
                            println!("    {}  {:.1}s", badge(p.risk.risk), t0.elapsed().as_secs_f64());
                        }
                        None => { println!("  \x1b[1;31m✖\x1b[0m {}", r.message.unwrap_or_else(|| "no proposal".into())); std::process::exit(1); }
                    },
                    Err(e) => die(&not_running(&e)),
                }
            }
        },
        Cmd::Init { shell } => match shell.as_str() {
            "zsh" => {
                let (cfg, _) = config::Config::load();
                // Bake config that the plugin needs at load time into the emitted script.
                println!("typeset -g FORE_GHOST_STYLE=\"${{FORE_GHOST_STYLE:-{}}}\"", cfg.ui.ghost_style);
                println!("typeset -g FORE_SAFE_RM=\"${{FORE_SAFE_RM:-{}}}\"", if cfg.undo.safe_rm { 1 } else { 0 });
                println!("typeset -g FORE_PREFLIGHT=\"${{FORE_PREFLIGHT:-{}}}\"", if cfg.preflight.enabled { 1 } else { 0 });
                println!("typeset -g FORE_BIND_RIGHT=\"{}\"", if cfg.ui.bind_right_arrow { 1 } else { 0 });
                print!("{}", include_str!("../shell/fore.zsh"));
            }
            other => die(&format!("unsupported shell: {other} (zsh only for now; bash/fish are on the roadmap)")),
        },

        // ---- daily use -------------------------------------------------------------
        Cmd::Undo { id } => {
            let trash = config::trash_dir();
            match undo::undo(&trash, id.as_deref()) {
                undo::UndoResult::Restored { journal, conflicts } => {
                    println!("restored {} ({} files, {}) from `{}`", journal.entries.iter().map(|e| e.from.display().to_string()).collect::<Vec<_>>().join(", "), journal.total_files, preflight::fmt_bytes(journal.total_bytes), journal.cmdline);
                    for c in conflicts { println!("  ! {c}"); }
                }
                undo::UndoResult::NothingToUndo => { println!("nothing to undo (only safe-rm operations are undoable; `fore trash list`)"); std::process::exit(1); }
                undo::UndoResult::Failed(e) => die(&e),
            }
        }
        Cmd::Trash { action } => {
            let trash = config::trash_dir();
            match action {
                TrashAction::List => {
                    let js = undo::list(&trash);
                    if js.is_empty() { println!("trash is empty"); return; }
                    println!("{:<22} {:>8} {:>10}  command", "id", "files", "size");
                    for j in js.iter().take(50) {
                        let age = preflight::fmt_dur((store::now_ms() - j.ts_ms).max(0) as u64);
                        println!("{:<22} {:>8} {:>10}  {}{}  \x1b[2m({age} ago{})\x1b[0m", j.id, j.total_files, preflight::fmt_bytes(j.total_bytes), j.cmdline, if j.restored { "  [restored]" } else { "" }, if j.cwd.is_empty() { String::new() } else { format!(", in {}", j.cwd) });
                    }
                    let (n, b) = undo::trash_size(&trash);
                    println!("\n{n} restorable operation(s), {} — `fore undo [id]` · `fore trash purge [--all]`", preflight::fmt_bytes(b));
                }
                TrashAction::Purge { all } => {
                    let (cfg, _) = config::Config::load();
                    let (n, b) = undo::purge(&trash, cfg.undo.keep_days, all);
                    println!("purged {n} operation(s), freed {}", preflight::fmt_bytes(b));
                }
            }
        }
        Cmd::Rm { args } => {
            let (cfg, _) = config::Config::load();
            let plan = undo::parse_rm_args(&args);
            if plan.interactive {
                // -i semantics are rm's business; hand over.
                exec_real_rm(&args);
            }
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            let cmdline = format!("rm {}", args.join(" "));
            let out = undo::safe_rm(&config::trash_dir(), &cwd, &cmdline, &plan, cfg.undo.max_bytes);
            for m in &out.messages { eprintln!("{m}"); }
            if let Some(j) = &out.journal {
                eprintln!("\x1b[2m→ trash ({} files, {}) — `fore undo` restores\x1b[0m", j.total_files, preflight::fmt_bytes(j.total_bytes));
            }
            std::process::exit(out.exit_code);
        }
        Cmd::Import { files, shell } => import_history(&socket, files, shell),
        Cmd::Stats { json } => match call(&socket, &Request::Stats, SLOW) {
            Ok(r) => { if json { println!("{}", serde_json::to_string(&r).unwrap()); } else if let Some(s) = r.stats { print_stats(&s); } }
            Err(e) => die(&not_running(&e)),
        },
        Cmd::Aliases { json, install, top } => aliases(&socket, json, install, top),
        Cmd::Check { json, cmd } => {
            let a = safety::assess(&cmd.join(" "));
            if json { println!("{}", serde_json::to_string(&a).unwrap()); }
            else { println!("{}  {}", badge(a.risk), if a.reasons.is_empty() { "no side effects detected".to_string() } else { a.reasons.join("; ") }); }
            std::process::exit(match a.risk { Risk::Destructive => 3, Risk::Privileged => 2, Risk::Remote | Risk::Mutating => 1, Risk::ReadOnly => 0 });
        }
        Cmd::Redact { text } => {
            let text = if text.is_empty() { let mut s = String::new(); std::io::stdin().read_to_string(&mut s).ok(); s } else { text.join(" ") };
            print!("{}", redact::Redactor::from_env().redact(&text));
            if !text.ends_with('\n') { println!(); }
        }
        Cmd::Ping => {
            let t0 = Instant::now();
            match call(&socket, &Request::Ping, FAST) {
                Ok(r) => println!("{}  round-trip {:?}  daemon-side {}µs  socket {}", r.message.unwrap_or_default(), t0.elapsed(), r.took_us, socket.display()),
                Err(e) => die(&not_running(&e)),
            }
        }

        // ---- plugin calls ------------------------------------------------------------
        Cmd::Exec { session, cwd, git_branch, cmd } => { let _ = call(&socket, &Request::Exec { cmd, cwd, session, git_branch }, FAST); }
        Cmd::Done { session, exit_code, duration_ms, stderr_file } => {
            let stderr_tail = stderr_file.and_then(|p| {
                let s = if p.as_os_str() == "-" { let mut s = String::new(); std::io::stdin().read_to_string(&mut s).ok().map(|_| s) } else { let s = std::fs::read_to_string(&p).ok(); let _ = std::fs::remove_file(&p); s };
                s.filter(|s| !s.trim().is_empty()).map(|s| tail(&s, 40, 4000))
            });
            let _ = call(&socket, &Request::Done { session, exit_code, duration_ms, stderr_tail }, FAST);
        }
        Cmd::Suggest { session, cwd, prefix } => {
            if let Ok(r) = call(&socket, &Request::Suggest { prefix, cwd, session }, FAST) && let Some(s) = r.suggestion { println!("{s}"); }
        }
        Cmd::Fix { session, json, shell } => match call(&socket, &Request::Fix { session }, SLOW) {
            Ok(r) => print_proposal(r, json, shell),
            Err(e) => { if shell { println!("ERR\t{}", not_running(&e)); } else { die(&not_running(&e)); } }
        },
        Cmd::Ask { session, cwd, git_branch, json, shell, request } => {
            let request = request.join(" ");
            if request.trim().is_empty() { die("empty request"); }
            match call(&socket, &Request::Nl { session, cwd, request, git_branch }, SLOW) {
                Ok(r) => print_proposal(r, json, shell),
                Err(e) => { if shell { println!("ERR\t{}", not_running(&e)); } else { die(&not_running(&e)); } }
            }
        }
        Cmd::Preflight { session, cwd, json, shell, cmd } => {
            let env = env_snapshot();
            match call(&socket, &Request::Preflight { session, cmd, cwd, env }, Duration::from_millis(400)) {
                Ok(r) => {
                    if json { println!("{}", serde_json::to_string(&r).unwrap()); }
                    else if let Some(pf) = r.preflight {
                        if shell {
                            // Line format: SEVERITY<TAB>text. First line BLOCK/OK summary.
                            println!("{}", if pf.blocks() { "BLOCK" } else { "OK" });
                            for f in &pf.findings { println!("{}\t{}", match f.severity { preflight::Severity::Info => "info", preflight::Severity::Warn => "warn", preflight::Severity::Block => "block" }, f.text); }
                        } else {
                            for f in &pf.findings { println!("{} {}", sev_badge(f.severity), f.text); }
                            std::process::exit(if pf.blocks() { 3 } else if pf.findings.is_empty() { 0 } else { 1 });
                        }
                    }
                }
                Err(_) => { if shell { println!("OK"); } } // daemon down: never block Enter
            }
        }
        Cmd::Accepted { session, chars } => { let _ = call(&socket, &Request::Accepted { session, chars }, FAST); }
    }
}

// ---------------------------------------------------------------------------
// install / uninstall
// ---------------------------------------------------------------------------

fn install(exe: &PathBuf, no_zshrc: bool, no_service: bool) {
    println!("\x1b[1mfore install\x1b[0m\n");
    // 1. config template
    let cp = config::config_path();
    if !cp.exists() {
        std::fs::create_dir_all(cp.parent().unwrap()).ok();
        if std::fs::write(&cp, config::Config::template()).is_ok() { println!("  ✔ wrote config template {}", cp.display()); }
    } else { println!("  ✔ config exists {}", cp.display()); }
    // 2. .zshrc
    if !no_zshrc {
        let zshrc = config::zshrc_path();
        let rc = std::fs::read_to_string(&zshrc).unwrap_or_default();
        if rc.contains("fore init zsh") { println!("  ✔ ~/.zshrc already sources the plugin"); }
        else {
            let bin_dir = exe.parent().map(|p| p.display().to_string()).unwrap_or_default();
            // Always export the bin dir unless it's a system one: bash puts ~/.local/bin on PATH
            // via ~/.profile, which a zsh login shell never reads. An extra export is harmless.
            let system_dir = matches!(bin_dir.as_str(), "/usr/local/bin" | "/usr/bin" | "/bin" | "/opt/homebrew/bin" | "/opt/local/bin");
            let mut block = String::from("\n# fore — shell copilot (https://github.com/YOUR_GITHUB_USER/fore)\n");
            if !system_dir { block.push_str(&format!("export PATH=\"{bin_dir}:$PATH\"\n")); }
            block.push_str("command -v fore >/dev/null 2>&1 && eval \"$(fore init zsh)\"\n");
            match std::fs::OpenOptions::new().create(true).append(true).open(&zshrc).and_then(|mut f| f.write_all(block.as_bytes())) {
                Ok(()) => println!("  ✔ added plugin to ~/.zshrc"),
                Err(e) => println!("  ✖ could not edit ~/.zshrc: {e}\n    add manually:  eval \"$(fore init zsh)\""),
            }
        }
    }
    // 3. service
    if !no_service {
        match service::install_autostart(exe) { Ok(m) => println!("  ✔ {m}"), Err(e) => println!("  ! autostart not installed: {e}\n    (the plugin auto-starts the daemon anyway)") }
    }
    // 4. make sure it's running now
    if !service::socket_alive() {
        match service::start(exe) { Ok(_) => println!("  ✔ daemon started"), Err(e) => println!("  ✖ daemon failed to start: {e}") }
    } else { println!("  ✔ daemon running"); }
    // 5. seed from existing shell history so ghost text works immediately
    let fresh = store::Store::open(&config::db_path()).and_then(|s| s.count()).map(|n| n < 50).unwrap_or(true);
    if fresh && !import::candidates().is_empty() {
        println!("  · importing your existing shell history (so suggestions work today):");
        import_history(&config::socket_path(), vec![], None);
    }
    println!("\nOpen a new terminal (or run: source ~/.zshrc). Then try typing a command you've used before.\n");
    let _ = doctor::run(exe);
}

fn uninstall(purge: bool) {
    println!("\x1b[1mfore uninstall\x1b[0m\n");
    let _ = service::stop();
    match service::uninstall_autostart() { Ok(m) => println!("  ✔ {m}"), Err(e) => println!("  ! {e}") }
    let zshrc = config::zshrc_path();
    if let Ok(rc) = std::fs::read_to_string(&zshrc) {
        // Remove exactly the block `fore install` wrote: the comment line, the optional PATH export
        // that immediately follows it, and the eval line.
        let lines: Vec<&str> = rc.lines().collect();
        let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            let l = lines[i];
            if l.contains("# fore — shell copilot") {
                i += 1;
                if i < lines.len() && lines[i].starts_with("export PATH=") { i += 1; }
                continue;
            }
            if l.contains("fore init zsh") { i += 1; continue; }
            kept.push(l);
            i += 1;
        }
        if kept.len() != rc.lines().count() {
            let _ = std::fs::write(&zshrc, kept.join("\n") + "\n");
            println!("  ✔ removed plugin line from ~/.zshrc");
        }
    }
    if purge {
        for p in [config::data_dir(), config::state_dir(), config::config_dir()] {
            if p.exists() { let _ = std::fs::remove_dir_all(&p); println!("  ✔ removed {}", p.display()); }
        }
    } else {
        println!("  · kept history/config/trash (use --purge to delete):\n      {}\n      {}", config::data_dir().display(), config::config_dir().display());
    }
    println!("\nDelete the binary to finish:  rm $(which fore)");
}

fn import_history(socket: &PathBuf, files: Vec<PathBuf>, shell: Option<String>) {
    let list: Vec<(PathBuf, &'static str)> = if files.is_empty() {
        import::candidates()
    } else {
        files.into_iter().map(|f| {
            let guess = if f.to_string_lossy().contains("bash") { "bash" } else { "zsh" };
            (f, match shell.as_deref() { Some("bash") => "bash", Some(_) => "zsh", None => guess })
        }).collect()
    };
    if list.is_empty() { println!("no history files found (looked for ~/.zsh_history, ~/.bash_history, $HISTFILE)"); return; }
    let mut store = match store::Store::open(&config::db_path()) { Ok(s) => s, Err(e) => die(&format!("open {}: {e}", config::db_path().display())) };
    // Same rule as live ingestion: secrets are masked before they touch the database.
    let (cfg, _) = config::Config::load();
    let redactor = redact::Redactor::from_env();
    let mut total = 0;
    let mut redacted = 0usize;
    for (path, sh) in list {
        match import::read(&path, sh, store::now_ms()) {
            Ok(rows) => {
                let parsed = rows.len();
                let rows: Vec<(String, i64)> = rows.into_iter().map(|r| {
                    if cfg.history.redact_on_write {
                        let clean = redactor.redact(&r.cmd);
                        if clean != r.cmd { redacted += 1; }
                        (clean, r.ts_ms)
                    } else { (r.cmd, r.ts_ms) }
                }).collect();
                match store.import(&rows) {
                    Ok(n) => { total += n; println!("  {}  {parsed} entries → {n} new", path.display()); }
                    Err(e) => println!("  {}  db error: {e}", path.display()),
                }
            }
            Err(e) => println!("  {}  {e}", path.display()),
        }
    }
    drop(store);
    if redacted > 0 { println!("  {redacted} entries contained secrets — stored redacted (`fore redact` shows the rule)"); }
    match call(socket, &Request::Reload, SLOW) {
        Ok(r) => println!("imported {total} commands; daemon {}", r.message.unwrap_or_default()),
        Err(_) => println!("imported {total} commands (daemon not running — they'll load on next start)"),
    }
}

fn exec_real_rm(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("/bin/rm").args(args).exec();
    die(&format!("exec /bin/rm: {err}"));
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

const FAST: Duration = Duration::from_millis(200);
const SLOW: Duration = Duration::from_secs(60);

fn call(socket: &PathBuf, req: &Request, timeout: Duration) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(FAST))?;
    let mut buf = serde_json::to_vec(req).expect("serialize");
    buf.push(b'\n');
    stream.write_all(&buf)?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn not_running(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::ConnectionRefused {
        "daemon not running — `fore start` (or `fore doctor`)".into()
    } else {
        format!("daemon error: {e}")
    }
}

fn print_proposal(r: Response, json: bool, shell: bool) {
    if json { println!("{}", serde_json::to_string(&r).unwrap()); return; }
    match r.proposal {
        Some(p) => {
            if shell {
                // Line format the plugin parses without jq: KEY<TAB>value
                println!("CMD\t{}", p.cmd);
                println!("NOTE\t{}", p.note.replace('\n', " "));
                println!("RISK\t{}", p.risk.risk.label());
                println!("WHY\t{}", p.risk.reasons.join("; "));
                println!("SRC\t{}", if p.cached { "precomputed".to_string() } else { format!("{}ms", r.took_us / 1000) });
            } else {
                if !p.note.is_empty() { eprintln!("  {}", p.note); }
                eprintln!("  {}  {}{}", badge(p.risk.risk), p.risk.reasons.join("; "), if p.cached { "  (precomputed)" } else { "" });
                println!("{}", p.cmd);
            }
        }
        None => {
            let msg = r.message.unwrap_or_else(|| "no proposal".into());
            if shell { println!("ERR\t{msg}"); } else { die(&msg); }
        }
    }
}

fn badge(r: Risk) -> String {
    let (color, text) = match r {
        Risk::ReadOnly => ("\x1b[32m", "read-only"), Risk::Mutating => ("\x1b[33m", "mutating"), Risk::Remote => ("\x1b[34m", "remote"),
        Risk::Privileged => ("\x1b[35m", "privileged"), Risk::Destructive => ("\x1b[1;31m", "DESTRUCTIVE"),
    };
    format!("{color}[{text}]\x1b[0m")
}

fn sev_badge(s: preflight::Severity) -> &'static str {
    match s { preflight::Severity::Info => "\x1b[2m·\x1b[0m", preflight::Severity::Warn => "\x1b[33m!\x1b[0m", preflight::Severity::Block => "\x1b[1;31m✖\x1b[0m" }
}

fn env_snapshot() -> preflight::EnvSnapshot {
    let v = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let kube = v("FORE_KUBE_CONTEXT").or_else(kube_context_from_config);
    preflight::EnvSnapshot {
        virtual_env: v("VIRTUAL_ENV"), conda_env: v("CONDA_DEFAULT_ENV").filter(|e| e != "base"), kube_context: kube,
        aws_profile: v("AWS_PROFILE").or_else(|| v("AWS_DEFAULT_PROFILE")), git_branch: v("FORE_GIT_BRANCH"),
        git_dirty: v("FORE_GIT_DIRTY").map(|d| d == "1"), ignore: v("FORE_IGNORE"),
    }
}

fn kube_context_from_config() -> Option<String> {
    let path = std::env::var("KUBECONFIG").ok().filter(|s| !s.is_empty()).map(|s| PathBuf::from(s.split(':').next().unwrap_or("")))
        .unwrap_or_else(|| config::home().join(".kube/config"));
    let s = std::fs::read_to_string(path).ok()?;
    s.lines().find_map(|l| l.strip_prefix("current-context:")).map(|c| c.trim().trim_matches('"').to_string()).filter(|c| !c.is_empty())
}

fn aliases(socket: &PathBuf, json: bool, install: bool, top: Option<usize>) {
    let mut existing: Vec<String> = std::env::var("FORE_EXISTING_ALIASES").unwrap_or_default().split_whitespace().map(String::from).collect();
    if let Ok(s) = std::fs::read_to_string(config::alias_file()) {
        for l in s.lines() { if let Some(rest) = l.strip_prefix("alias ") && let Some(n) = rest.split('=').next() { existing.push(n.to_string()); } }
    }
    match call(socket, &Request::Aliases { existing }, SLOW) {
        Ok(r) => {
            let props = r.aliases.unwrap_or_default();
            if json { println!("{}", serde_json::to_string(&props).unwrap()); return; }
            if props.is_empty() { println!("No alias proposals yet — fore needs a few days of history (or nothing repeats ≥4×)."); return; }
            println!("\x1b[1mAlias proposals\x1b[0m  (ranked by characters you'd stop typing per month)\n");
            for (i, p) in props.iter().enumerate() {
                let kind = match p.kind { insights::AliasKind::Sequence => "sequence", insights::AliasKind::Long => "command" };
                println!("  {:>2}. \x1b[1;36m{}\x1b[0m  →  {}\n      seen {}×  ·  saves ~{} chars/month  ·  {}", i + 1, p.name, p.expansion, p.count, preflight::fmt_n(p.chars_saved_per_month), kind);
            }
            if install {
                let path = config::alias_file();
                if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
                let mut body = std::fs::read_to_string(&path).unwrap_or_default();
                let mut added = 0;
                for p in props.iter().take(top.unwrap_or(props.len())) {
                    let line = format!("alias {}='{}'", p.name, p.expansion.replace('\'', "'\\''"));
                    if !body.contains(&format!("alias {}=", p.name)) { body.push_str(&line); body.push('\n'); added += 1; }
                }
                std::fs::write(&path, body).unwrap_or_else(|e| die(&format!("write {}: {e}", path.display())));
                println!("\n✔ added {added} alias(es) to {}\n  (auto-sourced by the plugin in new shells; now: source {})", path.display(), path.display());
            } else {
                println!("\nInstall with:  fore aliases --install [--top N]");
            }
        }
        Err(e) => die(&not_running(&e)),
    }
}

fn print_stats(s: &insights::Stats) {
    let b = "\x1b[1m"; let d = "\x1b[2m"; let r = "\x1b[0m"; let c = "\x1b[36m";
    println!("{b}fore stats{r}  {d}last {:.1} days · {} sessions{r}\n", s.window_days, s.sessions);
    println!("  {c}commands{r}      {}  ({:.0}/day, {} distinct)", preflight::fmt_n(s.commands), s.commands_per_day, s.distinct_commands);
    println!("  {c}failures{r}      {}  ({:.0}%)", s.failures, s.failure_rate * 100.0);
    println!("  {c}time waiting{r}  {}  {d}(sum of commands ≥1s){r}", preflight::fmt_dur(s.time_waiting_ms));
    println!();
    let acc = if s.suggest_shown > 0 { s.suggest_accepted as f64 / s.suggest_shown as f64 * 100.0 } else { 0.0 };
    println!("  {c}ghost text{r}    shown {}  accepted {}  ({acc:.0}%)  →  {b}{} keystrokes saved{r}", s.suggest_shown, s.suggest_accepted, preflight::fmt_n(s.chars_saved as usize));
    println!("  {c}ai{r}            Ctrl-/ fixes {}  ·  Ctrl-Space asks {}", s.fix_requested, s.ask_requested);
    println!("  {c}pre-flight{r}    {} destructive commands got a second look", s.blocks_shown);
    if !s.top_commands.is_empty() { println!("\n  {b}most used{r}"); for (cmd, n) in &s.top_commands { println!("    {n:>5}×  {cmd}"); } }
    if !s.slowest.is_empty() { println!("\n  {b}slowest{r}"); for (cmd, ms) in &s.slowest { println!("    {:>7}  {cmd}", preflight::fmt_dur(*ms)); } }
    if !s.most_failing.is_empty() { println!("\n  {b}most failing{r}"); for (cmd, f, n) in &s.most_failing { println!("    {f:>3}/{n:<3}  {cmd}"); } }
    if !s.busiest_dirs.is_empty() { println!("\n  {b}busiest directories{r}"); for (dir, n) in &s.busiest_dirs { println!("    {n:>5}×  {dir}"); } }
    println!("\n  {d}fore aliases  →  shortcuts mined from what you repeat{r}");
}

fn tail(s: &str, max_lines: usize, max_bytes: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut out = lines[start..].join("\n");
    if out.len() > max_bytes {
        let mut idx = out.len() - max_bytes;
        while !out.is_char_boundary(idx) { idx += 1; }
        out = out[idx..].to_string();
    }
    out
}

unsafe extern "C" {
    #[link_name = "signal"]
    fn libc_signal(sig: i32, handler: usize) -> usize;
}

fn die(msg: &str) -> ! {
    eprintln!("fore: {msg}");
    std::process::exit(1);
}
