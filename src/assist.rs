//! The two AI features of Milestone 1:
//!
//!   fix()  — "this command failed; why, and what should I run instead?"
//!   nl()   — "turn this English into a command"
//!
//! Both build a compact context snapshot, redact it, call the model, parse the strict
//! reply format, and attach a safety assessment to whatever command comes back.
//!
//! The daemon calls fix() speculatively the moment a non-zero exit arrives, and caches
//! the result per session. When the user presses Ctrl+/ a moment later, the answer is
//! usually already there. That's the whole trick behind "it feels instant".

use crate::llm::{Llm, Redacted, SYSTEM_FIX, SYSTEM_NL};
use crate::redact::Redactor;
use crate::safety::{self, Assessment};
use crate::store::HistoryEntry;
use serde::{Deserialize, Serialize};

/// What the shell ultimately receives for a proposed command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub cmd: String,
    /// `WHY:` for fixes, `NOTE:` for NL translations.
    pub note: String,
    pub risk: Assessment,
    pub model: String,
    pub cached: bool,
}

/// Everything the model gets to see. Small on purpose: a 1.5B model reasons better
/// over 20 relevant lines than 200 irrelevant ones, and it keeps round trips short.
pub struct Context<'a> {
    pub cwd: &'a str,
    pub git_branch: Option<&'a str>,
    pub os: &'a str,
    pub shell: &'a str,
    pub project_kind: &'a str,
    pub recent: &'a [HistoryEntry],
}

impl Context<'_> {
    fn render(&self, n_recent: usize) -> String {
        let mut s = String::new();
        s.push_str(&format!("os: {}\nshell: {}\ncwd: {}\n", self.os, self.shell, self.cwd));
        if let Some(b) = self.git_branch {
            s.push_str(&format!("git branch: {b}\n"));
        }
        s.push_str(&format!("project: {}\n", self.project_kind));
        if !self.recent.is_empty() {
            s.push_str("recent commands (newest first, exit code in brackets):\n");
            for e in self.recent.iter().take(n_recent) {
                let code = e.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
                s.push_str(&format!("  [{code}] {}\n", e.cmd));
            }
        }
        s
    }
}

/// Detect the project type from marker files. Cheap: a handful of stat() calls.
pub fn detect_project(cwd: &str) -> &'static str {
    let p = std::path::Path::new(cwd);
    let has = |f: &str| p.join(f).exists();
    if has("Cargo.toml") { "rust (cargo)" }
    else if has("package.json") {
        if has("pnpm-lock.yaml") { "node (pnpm)" } else if has("yarn.lock") { "node (yarn)" } else if has("bun.lockb") { "node (bun)" } else { "node (npm)" }
    }
    else if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        if has("uv.lock") { "python (uv)" } else if has("poetry.lock") { "python (poetry)" } else { "python (pip)" }
    }
    else if has("go.mod") { "go" }
    else if has("Gemfile") { "ruby (bundler)" }
    else if has("pom.xml") { "java (maven)" }
    else if has("build.gradle") || has("build.gradle.kts") { "java/kotlin (gradle)" }
    else if has("Makefile") { "make" }
    else if has("docker-compose.yml") || has("compose.yaml") { "docker compose" }
    else if has(".git") { "git repo" }
    else { "unknown" }
}

pub async fn fix(
    llm: &Llm,
    redactor: &Redactor,
    ctx: &Context<'_>,
    failed_cmd: &str,
    exit_code: i32,
    stderr_tail: &str,
) -> Result<Proposal, String> {
    let stderr_tail = last_lines(stderr_tail, 25);
    let user = format!(
        "{}\nfailed command: {failed_cmd}\nexit code: {exit_code}\nstderr (last lines):\n{stderr_tail}\n",
        ctx.render(10)
    );
    let system: Redacted = redactor.wrap(SYSTEM_FIX);
    let user: Redacted = redactor.wrap(&user);
    let raw = llm.complete(&system, &user, 200).await?;
    let (why, cmd) = parse_two(&raw, "WHY:", "FIX:").ok_or_else(|| format!("unparseable model reply: {raw:?}"))?;
    Ok(Proposal { risk: safety::assess(&cmd), cmd, note: why, model: llm.model().to_string(), cached: false })
}

pub async fn nl(
    llm: &Llm,
    redactor: &Redactor,
    ctx: &Context<'_>,
    request: &str,
) -> Result<Proposal, String> {
    let user = format!("{}\nrequest: {request}\n", ctx.render(8));
    let system: Redacted = redactor.wrap(SYSTEM_NL);
    let user: Redacted = redactor.wrap(&user);
    let raw = llm.complete(&system, &user, 160).await?;
    let (cmd, note) = parse_two(&raw, "CMD:", "NOTE:").ok_or_else(|| format!("unparseable model reply: {raw:?}"))?;
    let note = if note == "-" { String::new() } else { note };
    Ok(Proposal { risk: safety::assess(&cmd), cmd, note, model: llm.model().to_string(), cached: false })
}

/// Parse `A: …\nB: …` tolerantly: models add backticks, blank lines, and bold markers.
/// Strip backticks only when they wrap the WHOLE value (`cargo test`), never when they
/// quote a word inside a sentence (`tset` is a typo…).
fn unwrap_backticks(v: &str) -> String {
    if v.len() >= 2 && v.starts_with('`') && v.ends_with('`') && !v[1..v.len() - 1].contains('`') {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

fn parse_two(raw: &str, a: &str, b: &str) -> Option<(String, String)> {
    let clean = raw.replace("**", "").replace("```sh", "").replace("```bash", "").replace("```", "");
    let mut va: Option<String> = None;
    let mut vb: Option<String> = None;
    for line in clean.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix(a) {
            va = Some(unwrap_backticks(rest.trim()));
        } else if let Some(rest) = l.strip_prefix(b) {
            vb = Some(unwrap_backticks(rest.trim()));
        }
    }
    match (va, vb) {
        (Some(x), Some(y)) if !y.is_empty() || !x.is_empty() => Some((x, y)),
        _ => None,
    }
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_strict_and_sloppy_replies() {
        let (w, f) = parse_two("WHY: no such file\nFIX: ls -la", "WHY:", "FIX:").unwrap();
        assert_eq!((w.as_str(), f.as_str()), ("no such file", "ls -la"));
        let (w, f) = parse_two("**WHY:** typo in subcommand\n\n**FIX:** `cargo test`\n", "WHY:", "FIX:").unwrap();
        assert_eq!((w.as_str(), f.as_str()), ("typo in subcommand", "cargo test"));
        assert!(parse_two("I think you should try again", "WHY:", "FIX:").is_none());
        let (w, f) = parse_two("WHY: `tset` is a typo of `test`.\nFIX: `cargo test`", "WHY:", "FIX:").unwrap();
        assert_eq!(w, "`tset` is a typo of `test`.");
        assert_eq!(f, "cargo test");
    }
}
