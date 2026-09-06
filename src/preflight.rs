//! Pre-flight checks: run between "user pressed Enter" and "command executes".
//!
//! Three kinds of findings, all computed locally in a few milliseconds:
//!
//!   Guard    — environment mismatch (no venv, prod kube context, dirty main…)
//!   Preview  — what a destructive command will actually touch (files, bytes, commits)
//!   Insight  — what history knows (typical duration, failed last N times)
//!
//! A finding has a severity. `Block` findings make the shell ask for a second Enter.
//! Everything else is shown but never stops the user.
//!
//! Hard budget: this runs on EVERY Enter. Filesystem walks are capped, git calls read
//! files directly instead of forking, and nothing here touches the network.

use crate::safety::{self, Risk};
use crate::store::HistoryEntry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// FYI, dim text.
    Info,
    /// Yellow. Worth a glance.
    Warn,
    /// Red. Requires a second Enter.
    Block,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// Short, one line, no trailing period.
    pub text: String,
    /// Stable identifier so users can silence a specific check (`FORE_IGNORE=venv,prod`).
    pub check: String,
}

/// What the shell tells us about its environment at Enter time. All optional: the
/// shell sends what it cheaply has; the daemon never asks it to compute anything slow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EnvSnapshot {
    #[serde(default)] pub virtual_env: Option<String>,
    #[serde(default)] pub conda_env: Option<String>,
    #[serde(default)] pub kube_context: Option<String>,
    #[serde(default)] pub aws_profile: Option<String>,
    #[serde(default)] pub git_branch: Option<String>,
    #[serde(default)] pub git_dirty: Option<bool>,
    /// Comma-separated check ids the user has silenced.
    #[serde(default)] pub ignore: Option<String>,
}

/// Tunables from config.toml.
#[derive(Debug, Clone)]
pub struct Options {
    pub protected_branches: Vec<String>,
    pub prod_markers: Vec<String>,
    pub block_files: usize,
    pub block_bytes: u64,
    /// When true, `rm` is routed through the trash: previews say so and block less aggressively.
    pub safe_rm: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            protected_branches: ["main", "master", "production", "release", "develop"].map(String::from).to_vec(),
            prod_markers: ["prod", "prd", "live"].map(String::from).to_vec(),
            block_files: 100, block_bytes: 100 * 1024 * 1024, safe_rm: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preflight {
    pub findings: Vec<Finding>,
    pub risk: Risk,
    pub took_us: u64,
}

impl Preflight {
    pub fn blocks(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Block)
    }
}

/// Walk budget for previews. Enough for `node_modules`, small enough to never stall.
const MAX_WALK_ENTRIES: usize = 50_000;
const MAX_WALK_MS: u128 = 40;

pub fn run(cmd: &str, cwd: &str, env: &EnvSnapshot, history: &[HistoryEntry], opts: &Options) -> Preflight {
    let t0 = Instant::now();
    let mut out: Vec<Finding> = Vec::new();
    let ignored: Vec<&str> = env.ignore.as_deref().unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty()).collect();

    let assessment = safety::assess(cmd);
    let cmds = split_commands(cmd);

    for c in &cmds {
        guards(c, cwd, env, opts, &mut out);
        previews(c, cwd, opts, &mut out);
    }
    insights(cmd, cwd, history, &mut out);

    // Destructive per the classifier but no specific preview fired → still say so once.
    // (Unless a preview already established it's a no-op, e.g. `rm -rf missing-dir`.)
    let noop = out.iter().any(|f| f.check == "preview" && f.text.contains("nothing matches"));
    let trashed = opts.safe_rm && out.iter().any(|f| f.check == "preview" && f.text.contains("→ trash"));
    if assessment.risk == Risk::Destructive && !noop && !trashed && !out.iter().any(|f| f.severity == Severity::Block) {
        out.push(Finding { severity: Severity::Block, check: "destructive".into(), text: assessment.reasons.join("; ") });
    }

    out.retain(|f| !ignored.contains(&f.check.as_str()));
    out.sort_by_key(|x| std::cmp::Reverse(x.severity));
    out.dedup_by(|a, b| a.text == b.text);

    Preflight { findings: out, risk: assessment.risk, took_us: t0.elapsed().as_micros() as u64 }
}

// ---------------------------------------------------------------------------
// Command splitting (so `cd x && rm -rf y` checks each piece)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SimpleCmd {
    name: String,
    args: Vec<String>,
    /// `true` if the command was wrapped in sudo/doas.
    sudo: bool,
}

fn split_commands(cmdline: &str) -> Vec<SimpleCmd> {
    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_bash::LANGUAGE.into()).is_err() {
        return vec![];
    }
    let Some(tree) = parser.parse(cmdline, None) else { return vec![] };
    let mut out = Vec::new();
    collect(tree.root_node(), cmdline.as_bytes(), &mut out);
    out
}

fn collect(node: Node, src: &[u8], out: &mut Vec<SimpleCmd>) {
    if node.kind() == "command" {
        let mut words: Vec<String> = Vec::new();
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            match ch.kind() {
                "command_name" | "word" | "number" | "string" | "raw_string" | "concatenation" => {
                    words.push(ch.utf8_text(src).unwrap_or("").trim_matches(|q| q == '"' || q == '\'').to_string())
                }
                _ => {}
            }
        }
        let mut sudo = false;
        let mut i = 0;
        while i < words.len() {
            let w = words[i].rsplit('/').next().unwrap_or(&words[i]).to_string();
            if !matches!(w.as_str(), "sudo" | "doas" | "env" | "time" | "nohup" | "nice" | "command" | "builtin" | "exec" | "timeout" | "caffeinate" | "stdbuf" | "unbuffer" | "hyperfine") { break; }
            if w == "sudo" || w == "doas" { sudo = true; }
            i += 1;
            if w == "timeout" && i < words.len() && !words[i].starts_with('-') { i += 1; }
            while i < words.len() && (words[i].starts_with('-') || (words[i].contains('=') && !words[i].starts_with('-'))) {
                if matches!(words[i].as_str(), "-u" | "-g" | "-n" | "-I" | "-f" | "-o" | "-a" | "-e" | "-i" | "-s" | "-k") { i += 1; }
                i += 1;
            }
        }
        if i < words.len() {
            let name = words[i].rsplit('/').next().unwrap_or(&words[i]).to_string();
            out.push(SimpleCmd { name, args: words[i + 1..].to_vec(), sudo });
        }
        return;
    }
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        collect(ch, src, out);
    }
}

// ---------------------------------------------------------------------------
// Guards: environment mismatches
// ---------------------------------------------------------------------------

fn guards(c: &SimpleCmd, cwd: &str, env: &EnvSnapshot, opts: &Options, out: &mut Vec<Finding>) {
    let sub = c.args.first().map(String::as_str).unwrap_or("");
    let argstr = c.args.join(" ");
    let prodish = |s: &str| { let s = s.to_ascii_lowercase(); opts.prod_markers.iter().any(|m| s.contains(m.as_str())) };

    // --- Python: installing outside a virtualenv -------------------------------
    if matches!(c.name.as_str(), "pip" | "pip3" | "python" | "python3") {
        let is_install = (c.name.starts_with("pip") && matches!(sub, "install" | "uninstall"))
            || (c.name.starts_with("python") && argstr.contains("-m pip install"));
        if is_install && env.virtual_env.is_none() && env.conda_env.is_none() && !argstr.contains("--user") && !argstr.contains("--break-system-packages") {
            let hint = if Path::new(cwd).join(".venv").exists() {
                "a .venv exists here — `source .venv/bin/activate` first"
            } else if Path::new(cwd).join("uv.lock").exists() {
                "this project uses uv — `uv add …` instead"
            } else if Path::new(cwd).join("poetry.lock").exists() {
                "this project uses poetry — `poetry add …` instead"
            } else {
                "no virtualenv active — this installs into the system Python"
            };
            out.push(Finding { severity: Severity::Warn, check: "venv".into(), text: format!("pip: {hint}") });
        }
        if c.sudo && is_install {
            out.push(Finding { severity: Severity::Block, check: "sudo-pip".into(), text: "sudo pip install breaks system Python packages — use a venv or pipx".into() });
        }
    }

    // --- Node: global installs / wrong package manager ----------------------------
    if matches!(c.name.as_str(), "npm" | "yarn" | "pnpm" | "bun") && matches!(sub, "install" | "i" | "add" | "ci") {
        let p = Path::new(cwd);
        let lock = if p.join("pnpm-lock.yaml").exists() { Some("pnpm") }
            else if p.join("yarn.lock").exists() { Some("yarn") }
            else if p.join("bun.lockb").exists() || p.join("bun.lock").exists() { Some("bun") }
            else if p.join("package-lock.json").exists() { Some("npm") }
            else { None };
        if let Some(l) = lock
            && l != c.name {
                out.push(Finding { severity: Severity::Warn, check: "pkg-manager".into(), text: format!("this project uses {l} (lockfile present) — mixing with {} will create a second lockfile", c.name) });
            }
        if c.sudo {
            out.push(Finding { severity: Severity::Warn, check: "sudo-npm".into(), text: "sudo npm install: fix npm's prefix instead (`npm config set prefix ~/.npm-global`)".into() });
        }
    }

    // --- Kubernetes / cloud: production contexts ----------------------------------
    if matches!(c.name.as_str(), "kubectl" | "k" | "helm" | "oc") {
        let writes = matches!(sub, "apply" | "delete" | "create" | "patch" | "edit" | "scale" | "rollout" | "drain" | "cordon" | "exec" | "install" | "upgrade" | "uninstall" | "set" | "label" | "annotate" | "replace" | "run");
        if writes {
            let ctx = env.kube_context.as_deref().unwrap_or("");
            if prodish(ctx) || prodish(&argstr) {
                let which = if prodish(ctx) { format!("context `{ctx}`") } else { "a production namespace".to_string() };
                out.push(Finding { severity: Severity::Block, check: "prod".into(), text: format!("{} {sub} against {which}", c.name) });
            } else if env.kube_context.is_none() {
                out.push(Finding { severity: Severity::Info, check: "kube-ctx".into(), text: "kube context unknown to fore (export KUBE_CONTEXT or set FORE_KUBE_CONTEXT_CMD)".into() });
            } else {
                out.push(Finding { severity: Severity::Info, check: "kube-ctx".into(), text: format!("kube context: {ctx}") });
            }
        }
    }
    if matches!(c.name.as_str(), "terraform" | "tofu") && matches!(sub, "apply" | "destroy") {
        let ws = env.aws_profile.as_deref().unwrap_or("default");
        out.push(Finding {
            severity: if prodish(ws) || sub == "destroy" { Severity::Block } else { Severity::Warn },
            check: "terraform".into(),
            text: format!("{} {sub} with AWS profile `{ws}`{}", c.name, if argstr.contains("-auto-approve") { " and NO plan review (-auto-approve)" } else { "" }),
        });
    }
    if c.name == "aws"
        && let Some(p) = &env.aws_profile
            && prodish(p) && c.args.iter().any(|a| matches!(a.as_str(), "rm" | "rb" | "delete" | "terminate-instances" | "delete-stack" | "delete-bucket")) {
                out.push(Finding { severity: Severity::Block, check: "prod".into(), text: format!("aws delete with profile `{p}`") });
            }

    // --- Git: protected branches, dirty trees, force pushes -------------------------
    if c.name == "git" {
        let branch = env.git_branch.as_deref().unwrap_or("");
        let protected = !branch.is_empty() && opts.protected_branches.iter().any(|b| b == branch);
        match sub {
            "commit" if protected => {
                out.push(Finding { severity: Severity::Warn, check: "main-commit".into(), text: format!("committing directly to `{branch}`") });
            }
            "push" => {
                let force = argstr.contains("--force") || c.args.iter().any(|a| a == "-f") || !argstr.contains("--force-with-lease") && argstr.contains("+");
                if force && protected {
                    out.push(Finding { severity: Severity::Block, check: "force-push".into(), text: format!("force-pushing `{branch}` rewrites shared history — prefer --force-with-lease on a feature branch") });
                } else if force {
                    out.push(Finding { severity: Severity::Warn, check: "force-push".into(), text: "force push: --force-with-lease is safer (fails if someone else pushed)".into() });
                }
            }
            "checkout" | "switch" | "pull" | "rebase" | "merge" | "stash" if env.git_dirty == Some(true) && sub != "stash" => {
                out.push(Finding { severity: Severity::Info, check: "dirty".into(), text: format!("working tree has uncommitted changes (git {sub} may conflict)") });
            }
            "add" => {
                // Secrets about to be staged.
                let staged_secret = c.args.iter().any(|a| {
                    let a = a.rsplit('/').next().unwrap_or(a);
                    a == ".env" || a.starts_with(".env.") || a.ends_with(".pem") || a.ends_with(".key") || a == "id_rsa" || a == "credentials.json"
                });
                let add_all = c.args.iter().any(|a| matches!(a.as_str(), "-A" | "--all" | "." | "*"));
                if staged_secret {
                    out.push(Finding { severity: Severity::Block, check: "secret-file".into(), text: "staging a secrets file (.env / key / pem) — add it to .gitignore instead".into() });
                } else if add_all {
                    for f in [".env", ".env.local", "id_rsa", "credentials.json"] {
                        let p = Path::new(cwd).join(f);
                        if p.exists() && !is_gitignored(cwd, f) {
                            out.push(Finding { severity: Severity::Block, check: "secret-file".into(), text: format!("`git {}` would stage `{f}` (not in .gitignore)", c.args.join(" ")) });
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // --- Docker / system -------------------------------------------------------------
    if matches!(c.name.as_str(), "docker" | "podman") && sub == "system" && c.args.get(1).map(String::as_str) == Some("prune")
        && (argstr.contains("--volumes") || argstr.contains("-a")) {
            out.push(Finding { severity: Severity::Warn, check: "docker-prune".into(), text: "docker system prune -a/--volumes removes ALL unused images and volumes (databases live in volumes)".into() });
        }
    if c.name == "chmod" && c.args.iter().any(|a| a == "777") {
        out.push(Finding { severity: Severity::Warn, check: "chmod777".into(), text: "chmod 777 makes it world-writable — 755 (dirs) / 644 (files) is almost always what you want".into() });
    }
    if matches!(c.name.as_str(), "curl" | "wget") && !c.args.iter().any(|a| a.starts_with("https://")) && c.args.iter().any(|a| a.starts_with("http://")) {
        out.push(Finding { severity: Severity::Info, check: "http".into(), text: "plain http:// — no TLS".into() });
    }
}

/// Cheap .gitignore check: literal line match only. Good enough for `.env`.
fn is_gitignored(cwd: &str, name: &str) -> bool {
    let mut dir = PathBuf::from(cwd);
    loop {
        if let Ok(s) = std::fs::read_to_string(dir.join(".gitignore"))
            && s.lines().map(str::trim).any(|l| l == name || l == format!("/{name}") || l == format!("{name}*") || l == "*.env" && name.ends_with(".env")) {
                return true;
            }
        if dir.join(".git").exists() || !dir.pop() {
            return false;
        }
    }
}

// ---------------------------------------------------------------------------
// Previews: what will this actually touch?
// ---------------------------------------------------------------------------

fn previews(c: &SimpleCmd, cwd: &str, opts: &Options, out: &mut Vec<Finding>) {
    match c.name.as_str() {
        "rm" => preview_rm(c, cwd, opts, out),
        "git" => preview_git(c, cwd, out),
        "find" if c.args.iter().any(|a| a == "-delete") => preview_find_delete(c, cwd, out),
        "rsync" if c.args.iter().any(|a| a == "--delete") => {
            if let Some(dst) = c.args.iter().rev().find(|a| !a.starts_with('-')) {
                let (n, bytes, _) = walk(&resolve(cwd, dst));
                if n > 0 {
                    out.push(Finding { severity: Severity::Warn, check: "preview".into(), text: format!("rsync --delete: destination `{dst}` currently holds {} files ({}); anything not in the source is removed", fmt_n(n), fmt_bytes(bytes)) });
                }
            }
        }
        "truncate" | "shred" => {
            for a in c.args.iter().filter(|a| !a.starts_with('-')) {
                if let Ok(m) = std::fs::metadata(resolve(cwd, a)) {
                    out.push(Finding { severity: Severity::Block, check: "preview".into(), text: format!("{} `{a}` ({}) — irreversible", c.name, fmt_bytes(m.len())) });
                }
            }
        }
        _ => {}
    }
    // `> file` truncation of an existing, non-empty file is handled in the daemon by the redirect
    // scan on the raw line; nothing to do per simple command.
}

fn preview_rm(c: &SimpleCmd, cwd: &str, opts: &Options, out: &mut Vec<Finding>) {
    let flags: String = c.args.iter().filter(|a| a.starts_with('-') && !a.starts_with("--")).map(|a| a.trim_start_matches('-')).collect();
    let recursive = flags.contains('r') || flags.contains('R') || c.args.iter().any(|a| a == "--recursive");
    let targets: Vec<&String> = c.args.iter().filter(|a| !a.starts_with('-')).collect();
    if targets.is_empty() {
        return;
    }
    let mut total_files = 0usize;
    let mut total_bytes = 0u64;
    let mut missing = Vec::new();
    let mut truncated = false;
    let mut git_tracked = false;
    let mut details = Vec::new();

    for t in &targets {
        let p = resolve(cwd, t);
        if t.contains('*') || t.contains('?') {
            // glob: expand cheaply via the parent dir
            let (n, b, tr) = walk_glob(cwd, t);
            total_files += n; total_bytes += b; truncated |= tr;
            details.push(format!("{t}: {} files", fmt_n(n)));
            continue;
        }
        match std::fs::symlink_metadata(&p) {
            Err(_) => missing.push(t.to_string()),
            Ok(m) if m.is_dir() => {
                if !recursive {
                    details.push(format!("{t}/ is a directory (rm without -r will refuse)"));
                    continue;
                }
                let (n, b, tr) = walk(&p);
                total_files += n; total_bytes += b; truncated |= tr;
                if p.join(".git").exists() { git_tracked = true; }
                details.push(format!("{t}/: {} files, {}", fmt_n(n), fmt_bytes(b)));
            }
            Ok(m) => {
                total_files += 1; total_bytes += m.len();
            }
        }
    }
    if is_inside_repo(cwd) && targets.iter().any(|t| !matches!(t.as_str(), "node_modules" | "target" | "dist" | "build" | ".venv" | "__pycache__" | ".next" | "out")) {
        git_tracked = true;
    }

    if total_files == 0 && missing.len() == targets.len() {
        out.push(Finding { severity: Severity::Info, check: "preview".into(), text: format!("rm: nothing matches ({} not found)", missing.join(", ")) });
        return;
    }
    let mut text = format!("rm will delete {}{} files ({})", if truncated { "≥" } else { "" }, fmt_n(total_files), fmt_bytes(total_bytes));
    if details.len() > 1 { text.push_str(&format!(" — {}", details.join(", "))); }
    if !missing.is_empty() { text.push_str(&format!("; not found: {}", missing.join(", "))); }
    let big = total_files >= opts.block_files || total_bytes >= opts.block_bytes;
    let severity = if opts.safe_rm {
        text.push_str(" → trash (`fore undo` restores)");
        if big { Severity::Warn } else { Severity::Info }
    } else {
        if git_tracked { text.push_str("; inside a git repo (untracked files are NOT recoverable)"); }
        if big || git_tracked { Severity::Block } else { Severity::Warn }
    };
    out.push(Finding { severity, check: "preview".into(), text });
}

fn preview_find_delete(c: &SimpleCmd, cwd: &str, out: &mut Vec<Finding>) {
    // Approximate: count entries under the start path matching -name if present.
    let start = c.args.first().filter(|a| !a.starts_with('-')).map(String::as_str).unwrap_or(".");
    let name = c.args.iter().position(|a| a == "-name" || a == "-iname").and_then(|i| c.args.get(i + 1)).cloned();
    let (n, b, tr) = match &name {
        Some(pat) => walk_glob_recursive(&resolve(cwd, start), pat),
        None => walk(&resolve(cwd, start)),
    };
    out.push(Finding { severity: if n >= 50 { Severity::Block } else { Severity::Warn }, check: "preview".into(), text: format!("find -delete will remove {}{} entries ({}){}", if tr { "≥" } else { "" }, fmt_n(n), fmt_bytes(b), name.map(|p| format!(" matching {p}")).unwrap_or_default()) });
}

fn preview_git(c: &SimpleCmd, cwd: &str, out: &mut Vec<Finding>) {
    let sub = c.args.first().map(String::as_str).unwrap_or("");
    let argstr = c.args.join(" ");
    match sub {
        "reset" if argstr.contains("--hard") => {
            let (staged, unstaged, untracked) = git_status_counts(cwd);
            if staged + unstaged > 0 {
                out.push(Finding { severity: Severity::Block, check: "preview".into(), text: format!("git reset --hard discards {staged} staged + {unstaged} unstaged changed files (untracked {untracked} kept)") });
            } else {
                out.push(Finding { severity: Severity::Info, check: "preview".into(), text: "git reset --hard: working tree is clean, nothing to lose".into() });
            }
        }
        "clean" if argstr.contains('f') => {
            let (_, _, untracked) = git_status_counts(cwd);
            let with_ignored = argstr.contains('x');
            out.push(Finding { severity: if untracked > 0 { Severity::Block } else { Severity::Info }, check: "preview".into(), text: format!("git clean will delete {untracked} untracked files{}", if with_ignored { " + all ignored files (node_modules, .env, build dirs…)" } else { "" }) });
        }
        "checkout" | "restore" if c.args.iter().any(|a| a == "--" || a == ".") && !argstr.contains("--staged") => {
            let (staged, unstaged, _) = git_status_counts(cwd);
            if unstaged + staged > 0 {
                out.push(Finding { severity: Severity::Block, check: "preview".into(), text: format!("git {sub} discards {unstaged} unstaged changed files") });
            }
        }
        "stash" if c.args.get(1).map(String::as_str) == Some("drop") || c.args.get(1).map(String::as_str) == Some("clear") => {
            out.push(Finding { severity: Severity::Warn, check: "preview".into(), text: "stash entries are not in reflog by name — `git stash list` first".into() });
        }
        "branch" if c.args.iter().any(|a| a == "-D") => {
            if let Some(b) = c.args.iter().find(|a| !a.starts_with('-') && a.as_str() != "branch") {
                let merged = git_branch_merged(cwd, b);
                if merged == Some(false) {
                    out.push(Finding { severity: Severity::Block, check: "preview".into(), text: format!("branch `{b}` is NOT merged — its commits will only survive in reflog for ~30 days") });
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Insights: what history knows
// ---------------------------------------------------------------------------

fn insights(cmd: &str, cwd: &str, history: &[HistoryEntry], out: &mut Vec<Finding>) {
    let same: Vec<&HistoryEntry> = history.iter().filter(|e| e.cmd == cmd && (e.cwd == cwd)).take(20).collect();
    if same.is_empty() {
        return;
    }
    // Consecutive recent failures of this exact command here.
    let mut streak = 0;
    for e in &same {
        match e.exit_code { Some(0) => break, Some(_) => streak += 1, None => {} }
    }
    if streak >= 2 {
        out.push(Finding { severity: Severity::Warn, check: "streak".into(), text: format!("this exact command failed the last {streak} times here — Ctrl-/ has a fix ready") });
    }
    // Typical duration (median of successful runs).
    let mut durs: Vec<u64> = same.iter().filter(|e| e.exit_code == Some(0)).filter_map(|e| e.duration_ms).collect();
    if durs.len() >= 2 {
        durs.sort_unstable();
        let med = durs[durs.len() / 2];
        if med >= 10_000 {
            out.push(Finding { severity: Severity::Info, check: "duration".into(), text: format!("usually takes ~{} here ({} runs)", fmt_dur(med), durs.len()) });
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem helpers with hard budgets
// ---------------------------------------------------------------------------

fn resolve(cwd: &str, p: &str) -> PathBuf {
    let p = if let Some(rest) = p.strip_prefix("~/") {
        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest)
    } else if p == "~" {
        PathBuf::from(std::env::var("HOME").unwrap_or_default())
    } else {
        PathBuf::from(p)
    };
    if p.is_absolute() { p } else { Path::new(cwd).join(p) }
}

/// (entries, bytes, truncated)
fn walk(root: &Path) -> (usize, u64, bool) {
    let t0 = Instant::now();
    let mut n = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            n += 1;
            if let Ok(m) = e.metadata() {
                if m.is_dir() && !m.file_type().is_symlink() {
                    stack.push(e.path());
                } else {
                    bytes += m.len();
                }
            }
            if n >= MAX_WALK_ENTRIES || t0.elapsed().as_millis() > MAX_WALK_MS {
                return (n, bytes, true);
            }
        }
    }
    (n, bytes, false)
}

fn glob_match(pat: &str, name: &str) -> bool {
    // Minimal glob: `*` and `?` only. Enough for `*.log`, `tmp-*`, `?.txt`.
    fn rec(p: &[u8], s: &[u8]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some(b'*'), _) => rec(&p[1..], s) || (!s.is_empty() && rec(p, &s[1..])),
            (Some(b'?'), Some(_)) => rec(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &s[1..]),
            _ => false,
        }
    }
    rec(pat.as_bytes(), name.as_bytes())
}

fn walk_glob(cwd: &str, pattern: &str) -> (usize, u64, bool) {
    let full = resolve(cwd, pattern);
    let dir = full.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(cwd));
    let pat = full.file_name().and_then(|s| s.to_str()).unwrap_or("*").to_string();
    let mut n = 0; let mut bytes = 0; let mut tr = false;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && !pat.starts_with('.') { continue; }
            if glob_match(&pat, &name) {
                if e.path().is_dir() { let (a, b, c) = walk(&e.path()); n += a; bytes += b; tr |= c; } else { n += 1; bytes += e.metadata().map(|m| m.len()).unwrap_or(0); }
            }
        }
    }
    (n, bytes, tr)
}

fn walk_glob_recursive(root: &Path, pat: &str) -> (usize, u64, bool) {
    let t0 = Instant::now();
    let mut n = 0usize; let mut bytes = 0u64; let mut seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(m) = e.metadata() else { continue };
            if glob_match(pat, &name) { n += 1; bytes += m.len(); }
            if m.is_dir() { stack.push(e.path()); }
            if seen >= MAX_WALK_ENTRIES || t0.elapsed().as_millis() > MAX_WALK_MS { return (n, bytes, true); }
        }
    }
    (n, bytes, false)
}

fn is_inside_repo(cwd: &str) -> bool {
    let mut d = PathBuf::from(cwd);
    loop {
        if d.join(".git").exists() { return true; }
        if !d.pop() { return false; }
    }
}

/// (staged, unstaged, untracked) via one `git status --porcelain`. Forks git — only
/// called for destructive git subcommands, never on the hot path.
fn git_status_counts(cwd: &str) -> (usize, usize, usize) {
    let Ok(o) = std::process::Command::new("git").args(["status", "--porcelain", "--untracked-files=all"]).current_dir(cwd).output() else { return (0, 0, 0) };
    let s = String::from_utf8_lossy(&o.stdout);
    let (mut st, mut un, mut ut) = (0, 0, 0);
    for l in s.lines() {
        let b = l.as_bytes();
        if b.len() < 2 { continue; }
        if &l[..2] == "??" { ut += 1; continue; }
        if b[0] != b' ' { st += 1; }
        if b[1] != b' ' { un += 1; }
    }
    (st, un, ut)
}

fn git_branch_merged(cwd: &str, branch: &str) -> Option<bool> {
    let o = std::process::Command::new("git").args(["branch", "--merged"]).current_dir(cwd).output().ok()?;
    if !o.status.success() { return None; }
    Some(String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim().trim_start_matches("* ") == branch))
}

pub fn fmt_bytes(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64; let mut i = 0;
    while v >= 1024.0 && i < 4 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn fmt_n(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(ch);
    }
    out
}

pub fn fmt_dur(ms: u64) -> String {
    if ms < 1000 { format!("{ms}ms") }
    else if ms < 60_000 { format!("{:.1}s", ms as f64 / 1000.0) }
    else { format!("{}m {:02}s", ms / 60_000, (ms % 60_000) / 1000) }
}

/// Used by the daemon: does the raw line contain a `>` that truncates an existing non-empty file?
pub fn truncating_redirect_targets(cmdline: &str, cwd: &str) -> Vec<(String, u64)> {
    let mut parser = Parser::new();
    let mut out = Vec::new();
    if parser.set_language(&tree_sitter_bash::LANGUAGE.into()).is_err() { return out; }
    let Some(tree) = parser.parse(cmdline, None) else { return out };
    let src = cmdline.as_bytes();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "file_redirect" {
            let t = n.utf8_text(src).unwrap_or("").trim();
            if t.starts_with('>') && !t.starts_with(">>") && !t.contains("/dev/null") {
                let target = t.trim_start_matches('>').trim().trim_matches(|q| q == '"' || q == '\'');
                if let Ok(m) = std::fs::metadata(resolve(cwd, target))
                    && m.is_file() && m.len() > 0 { out.push((target.to_string(), m.len())); }
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) { stack.push(ch); }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fore-pf-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn run_(cmd: &str, cwd: &str, env: &EnvSnapshot, hist: &[HistoryEntry]) -> Preflight {
        run(cmd, cwd, env, hist, &Options::default())
    }

    fn texts(p: &Preflight) -> String { p.findings.iter().map(|f| format!("[{:?}] {}", f.severity, f.text)).collect::<Vec<_>>().join("\n") }

    #[test]
    fn venv_guard() {
        let d = tmp("venv");
        let env = EnvSnapshot::default();
        let p = run_("pip install requests", d.to_str().unwrap(), &env, &[]);
        assert!(texts(&p).contains("no virtualenv active"), "{}", texts(&p));
        let env2 = EnvSnapshot { virtual_env: Some("/x/.venv".into()), ..Default::default() };
        let p2 = run_("pip install requests", d.to_str().unwrap(), &env2, &[]);
        assert!(!p2.findings.iter().any(|f| f.check == "venv"));
        fs::create_dir(d.join(".venv")).unwrap();
        let p3 = run_("pip install requests", d.to_str().unwrap(), &env, &[]);
        assert!(texts(&p3).contains(".venv exists here"));
    }

    #[test]
    fn prod_guard_blocks_writes_only() {
        let env = EnvSnapshot { kube_context: Some("gke-prod-eu".into()), ..Default::default() };
        assert!(run_("kubectl delete pod web", "/tmp", &env, &[]).blocks());
        assert!(run_("kubectl apply -f x.yaml", "/tmp", &env, &[]).blocks());
        assert!(!run_("kubectl get pods", "/tmp", &env, &[]).blocks());
    }

    #[test]
    fn rm_preview_counts_files() {
        let d = tmp("rm");
        let b = d.join("build");
        fs::create_dir_all(b.join("sub")).unwrap();
        for i in 0..120 { fs::write(b.join(format!("f{i}.o")), vec![0u8; 1024]).unwrap(); }
        fs::write(b.join("sub/x"), b"hello").unwrap();
        let p = run_("rm -rf build", d.to_str().unwrap(), &EnvSnapshot::default(), &[]);
        let t = texts(&p);
        assert!(t.contains("rm will delete 122 files"), "{t}");
        assert!(p.blocks(), "{t}");
        let p2 = run_("rm -rf nope", d.to_str().unwrap(), &EnvSnapshot::default(), &[]);
        assert!(texts(&p2).contains("nothing matches"), "{}", texts(&p2));
    }

    #[test]
    fn rm_glob_preview() {
        let d = tmp("glob");
        for i in 0..5 { fs::write(d.join(format!("a{i}.log")), b"x").unwrap(); }
        fs::write(d.join("keep.txt"), b"x").unwrap();
        let p = run_("rm *.log", d.to_str().unwrap(), &EnvSnapshot::default(), &[]);
        assert!(texts(&p).contains("delete 5 files"), "{}", texts(&p));
    }

    #[test]
    fn secret_file_guard() {
        let d = tmp("secret");
        fs::write(d.join(".env"), b"KEY=1").unwrap();
        let p = run_("git add -A", d.to_str().unwrap(), &EnvSnapshot::default(), &[]);
        assert!(p.blocks(), "{}", texts(&p));
        fs::write(d.join(".gitignore"), b".env\n").unwrap();
        let p2 = run_("git add -A", d.to_str().unwrap(), &EnvSnapshot::default(), &[]);
        assert!(!p2.findings.iter().any(|f| f.check == "secret-file"), "{}", texts(&p2));
        assert!(run_("git add .env", d.to_str().unwrap(), &EnvSnapshot::default(), &[]).blocks());
    }

    #[test]
    fn insights_from_history() {
        let h = |code: i32, dur: u64| HistoryEntry { cmd: "cargo build".into(), cwd: "/p".into(), exit_code: Some(code), ts_ms: 0, duration_ms: Some(dur) };
        let hist = vec![h(1, 100), h(1, 100), h(0, 130_000), h(0, 125_000)];
        let p = run_("cargo build", "/p", &EnvSnapshot::default(), &hist);
        let t = texts(&p);
        assert!(t.contains("failed the last 2 times"), "{t}");
        assert!(t.contains("usually takes ~2m"), "{t}");
    }

    #[test]
    fn ignore_list_silences_checks() {
        let env = EnvSnapshot { ignore: Some("venv".into()), ..Default::default() };
        let p = run_("pip install x", "/tmp", &env, &[]);
        assert!(!p.findings.iter().any(|f| f.check == "venv"));
    }

    #[test]
    fn second_enter_semantics_are_shell_side() {
        // Preflight itself is stateless: same input → same findings. The "press Enter again"
        // memory lives in the zsh plugin, keyed on the exact buffer.
        let a = run_("rm -rf /tmp/does-not-exist-xyz", "/tmp", &EnvSnapshot::default(), &[]);
        let b = run_("rm -rf /tmp/does-not-exist-xyz", "/tmp", &EnvSnapshot::default(), &[]);
        assert_eq!(texts(&a), texts(&b));
    }

    #[test]
    fn helpers() {
        assert_eq!(fmt_n(1204), "1,204");
        assert_eq!(fmt_bytes(312 * 1024 * 1024), "312.0 MB");
        assert_eq!(fmt_dur(130_000), "2m 10s");
        assert!(glob_match("*.log", "a.log") && !glob_match("*.log", "a.txt") && glob_match("f?.o", "f1.o"));
    }
}
