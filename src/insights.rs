//! The feedback loop: how do you KNOW the terminal is getting optimized?
//!
//!   mine_aliases()  — find command sequences and long commands you repeat, propose shortcuts
//!   stats()         — acceptance rate, errors, slowest commands, keystrokes saved
//!
//! Both are pure functions over the history slice. `fore stats` and `fore aliases` render them.

use crate::store::HistoryEntry;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Alias mining
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasProposal {
    /// Suggested short name.
    pub name: String,
    /// The full command (or `a && b && c` sequence).
    pub expansion: String,
    /// Times observed in the analysed window.
    pub count: usize,
    /// Characters the user would stop typing per month at the observed rate.
    pub chars_saved_per_month: usize,
    pub kind: AliasKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AliasKind {
    /// One long command typed many times.
    Long,
    /// A sequence of 2–3 commands typed back to back, in order.
    Sequence,
}

/// Session gap (ms) beyond which two consecutive commands are not a "sequence".
const SEQ_GAP_MS: i64 = 5 * 60 * 1000;

pub fn mine_aliases(history: &[HistoryEntry], existing_aliases: &[String], window_days: f64) -> Vec<AliasProposal> {
    if history.is_empty() {
        return vec![];
    }
    // history is newest-first; work oldest-first for sequences.
    let mut h: Vec<&HistoryEntry> = history.iter().filter(|e| e.exit_code == Some(0)).collect();
    h.reverse();

    let month_factor = 30.0 / window_days.max(1.0);
    let mut out = Vec::new();

    // --- 1. Long commands repeated ------------------------------------------------
    let mut freq: HashMap<&str, usize> = HashMap::new();
    for e in &h {
        if e.cmd.len() >= 16 && !is_noise(&e.cmd) {
            *freq.entry(e.cmd.as_str()).or_insert(0) += 1;
        }
    }
    for (cmd, n) in freq {
        if n < 4 { continue; }
        let name = short_name(cmd);
        if existing_aliases.iter().any(|a| a == &name) { continue; }
        let saved = (cmd.len().saturating_sub(name.len())) * n;
        out.push(AliasProposal {
            name, expansion: cmd.to_string(), count: n,
            chars_saved_per_month: (saved as f64 * month_factor) as usize, kind: AliasKind::Long,
        });
    }

    // --- 2. Sequences: a *workflow*, not just adjacency ------------------------------
    // Two commands only count as a sequence when they are (a) close in time, (b) in the same
    // directory, and (c) the FIRST one is not something you'd naturally stop and look at.
    // Test runners, builds, and pulls produce output you read; what you type after them is a
    // decision, not a habit. So sequences start only after "fire-and-forget" commands.
    let mut seq: HashMap<String, usize> = HashMap::new();
    let close = |a: &HistoryEntry, b: &HistoryEntry| b.ts_ms - a.ts_ms < SEQ_GAP_MS && a.cwd == b.cwd && a.cmd != b.cmd;
    for w in h.windows(3) {
        if !close(w[0], w[1]) || is_noise(&w[0].cmd) || is_noise(&w[1].cmd) || is_checkpoint(&w[0].cmd) {
            continue;
        }
        *seq.entry(format!("{} && {}", w[0].cmd, w[1].cmd)).or_insert(0) += 1;
        if close(w[1], w[2]) && !is_noise(&w[2].cmd) && !is_checkpoint(&w[1].cmd) {
            *seq.entry(format!("{} && {} && {}", w[0].cmd, w[1].cmd, w[2].cmd)).or_insert(0) += 1;
        }
    }
    // Prefer the longest chain; drop bigrams that are a strict part of a kept trigram
    // with (nearly) the same count — they're the same habit.
    let mut seqs: Vec<(String, usize)> = seq.into_iter().filter(|(_, n)| *n >= 3).collect();
    seqs.sort_by(|a, b| b.0.matches("&&").count().cmp(&a.0.matches("&&").count()).then(b.1.cmp(&a.1)));
    let mut kept: Vec<(String, usize)> = Vec::new();
    for (s, n) in seqs {
        if kept.iter().any(|(k, kn)| k.contains(&s) && *kn * 10 >= n * 8) { continue; }
        // Also drop chains that merely *overlap* a kept chain (share ≥2 commands): one habit, one alias.
        let parts: Vec<&str> = s.split(" && ").collect();
        if kept.iter().any(|(k, _)| parts.iter().filter(|p| k.contains(*p)).count() >= 2) { continue; }
        kept.push((s, n));
    }
    for (s, n) in kept {
        let name = short_name(&s);
        if existing_aliases.iter().any(|a| a == &name) { continue; }
        let saved = s.len().saturating_sub(name.len()) * n;
        out.push(AliasProposal { name, expansion: s, count: n, chars_saved_per_month: (saved as f64 * month_factor) as usize, kind: AliasKind::Sequence });
    }

    out.sort_by_key(|x| std::cmp::Reverse(x.chars_saved_per_month));
    out.truncate(10);
    out
}

/// Commands whose output you stop and read before deciding what to do next.
/// A sequence never *starts* with one of these.
fn is_checkpoint(cmd: &str) -> bool {
    let mut it = cmd.split_whitespace();
    let first = it.next().unwrap_or("");
    let second = it.next().unwrap_or("");
    matches!(first, "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "make" | "just" | "go" | "pytest" | "jest" | "mvn" | "gradle" | "docker" | "kubectl" | "terraform" | "curl" | "wget" | "ssh" | "grep" | "rg" | "find" | "diff")
        && !matches!(second, "install" | "add" | "i" | "ci" | "fmt" | "login")
        || (first == "git" && matches!(second, "status" | "log" | "diff" | "show" | "blame" | "pull" | "push" | "fetch" | "clone"))
}

fn is_noise(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    matches!(first, "cd" | "ls" | "ll" | "la" | "pwd" | "clear" | "exit" | "history" | "fore" | "echo" | "cat" | "vim" | "vi" | "nvim" | "nano" | "man")
        || cmd.starts_with('#')
}

/// `git add -A && git commit -m wip && git push` → `gacp`; `cargo build --release` → `cbr`;
/// `docker compose up -d` → `dcud`. Deterministic, short, memorable enough to start from.
fn short_name(cmd: &str) -> String {
    let parts: Vec<&str> = cmd.split("&&").collect();
    // Sequences: program + subcommand initial per step (`ga gc gp` → gagcgp).
    // Single commands: program + up to 3 arg initials (`cargo build --release` → cbr).
    let per_part = if parts.len() > 1 { 1 } else { 3 };
    let mut name = String::new();
    for part in parts {
        let mut words = part.split_whitespace().filter(|w| !w.starts_with('"') && !w.starts_with('\''));
        if let Some(prog) = words.next() {
            let prog = prog.rsplit('/').next().unwrap_or(prog);
            name.push(prog.chars().next().unwrap_or('x'));
            for w in words.take(per_part) {
                let w = w.trim_start_matches('-');
                if let Some(c) = w.chars().next()
                    && c.is_ascii_alphanumeric() && !w.contains('=') && !w.contains('/') && !w.contains('.') { name.push(c); }
            }
        }
    }
    let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    // never shadow a real program
    let reserved = ["cd", "ls", "rm", "mv", "cp", "dd", "sh", "ps", "du", "df", "bg", "fg", "cc", "gc", "go", "vi", "ln", "nc", "od", "tr", "wc", "ar", "as", "ld", "nl", "pr", "ul", "ex", "ed", "id", "w", "ip"];
    if name.len() < 2 || reserved.contains(&name.as_str()) { format!("{name}x") } else { name }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stats {
    pub window_days: f64,
    pub commands: usize,
    pub commands_per_day: f64,
    pub failures: usize,
    pub failure_rate: f64,
    pub distinct_commands: usize,
    pub sessions: usize,
    pub median_duration_ms: u64,
    pub time_waiting_ms: u64,
    pub top_commands: Vec<(String, usize)>,
    pub slowest: Vec<(String, u64)>,
    pub most_failing: Vec<(String, usize, usize)>, // cmd, failures, runs
    pub busiest_dirs: Vec<(String, usize)>,
    pub suggest_shown: u64,
    pub suggest_accepted: u64,
    pub chars_saved: u64,
    pub fix_requested: u64,
    pub ask_requested: u64,
    pub blocks_shown: u64,
}

/// Counters the daemon accumulates in memory (persisted to SQLite by the store).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counters {
    pub suggest_shown: u64,
    pub suggest_accepted: u64,
    pub chars_saved: u64,
    pub fix_requested: u64,
    pub ask_requested: u64,
    pub blocks_shown: u64,
}

pub fn stats(history: &[HistoryEntry], sessions: usize, now_ms: i64, counters: &Counters) -> Stats {
    let oldest = history.iter().map(|e| e.ts_ms).min().unwrap_or(now_ms);
    let window_days = ((now_ms - oldest) as f64 / 86_400_000.0).max(1.0 / 24.0);
    let n = history.len();
    let failures = history.iter().filter(|e| matches!(e.exit_code, Some(c) if c != 0 && c != 130)).count();

    let mut freq: HashMap<&str, (usize, usize)> = HashMap::new(); // runs, failures
    let mut dirs: HashMap<&str, usize> = HashMap::new();
    let mut durs: Vec<u64> = Vec::new();
    let mut waiting = 0u64;
    let mut slowest: HashMap<&str, u64> = HashMap::new();
    for e in history {
        let ent = freq.entry(e.cmd.as_str()).or_insert((0, 0));
        ent.0 += 1;
        if matches!(e.exit_code, Some(c) if c != 0 && c != 130) { ent.1 += 1; }
        *dirs.entry(e.cwd.as_str()).or_insert(0) += 1;
        if let Some(d) = e.duration_ms {
            durs.push(d);
            if d >= 1000 { waiting += d; }
            let s = slowest.entry(e.cmd.as_str()).or_insert(0);
            if d > *s { *s = d; }
        }
    }
    durs.sort_unstable();

    let mut top: Vec<(String, usize)> = freq.iter().map(|(c, (r, _))| (c.to_string(), *r)).collect();
    top.sort_by_key(|x| std::cmp::Reverse(x.1));
    top.truncate(8);

    let mut slow: Vec<(String, u64)> = slowest.into_iter().map(|(c, d)| (c.to_string(), d)).collect();
    slow.sort_by_key(|x| std::cmp::Reverse(x.1));
    slow.truncate(5);

    let mut failing: Vec<(String, usize, usize)> = freq.iter().filter(|(_, (r, f))| *f >= 2 && *r >= 2).map(|(c, (r, f))| (c.to_string(), *f, *r)).collect();
    failing.sort_by_key(|x| std::cmp::Reverse(x.1));
    failing.truncate(5);

    let mut busiest: Vec<(String, usize)> = dirs.into_iter().map(|(d, n)| (d.to_string(), n)).collect();
    busiest.sort_by_key(|x| std::cmp::Reverse(x.1));
    busiest.truncate(5);

    Stats {
        window_days, commands: n, commands_per_day: n as f64 / window_days, failures,
        failure_rate: if n > 0 { failures as f64 / n as f64 } else { 0.0 },
        distinct_commands: freq.len(), sessions,
        median_duration_ms: durs.get(durs.len() / 2).copied().unwrap_or(0),
        time_waiting_ms: waiting, top_commands: top, slowest: slow, most_failing: failing, busiest_dirs: busiest,
        suggest_shown: counters.suggest_shown, suggest_accepted: counters.suggest_accepted, chars_saved: counters.chars_saved,
        fix_requested: counters.fix_requested, ask_requested: counters.ask_requested, blocks_shown: counters.blocks_shown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(cmd: &str, ts: i64) -> HistoryEntry {
        HistoryEntry { cmd: cmd.into(), cwd: "/p".into(), exit_code: Some(0), ts_ms: ts, duration_ms: Some(50) }
    }

    #[test]
    fn finds_repeated_sequence() {
        let mut h = Vec::new();
        let mut t = 0;
        for _ in 0..5 {
            h.push(e("git add -A", t)); t += 1000;
            h.push(e("git commit -m wip", t)); t += 1000;
            h.push(e("git push", t)); t += 1000;
            h.push(e("ls", t)); t += 100_000_000; // break the sequence
        }
        h.reverse();
        let props = mine_aliases(&h, &[], 30.0);
        let seq = props.iter().find(|p| p.kind == AliasKind::Sequence).expect("sequence proposal");
        assert_eq!(seq.expansion, "git add -A && git commit -m wip && git push");
        assert_eq!(seq.count, 5);
        assert_eq!(seq.name, "gagcgp");
    }

    #[test]
    fn finds_long_repeated_command() {
        let h: Vec<HistoryEntry> = (0..6).map(|i| e("docker compose up -d --build", i * 1000)).collect();
        let props = mine_aliases(&h, &[], 30.0);
        let p = props.iter().find(|p| p.kind == AliasKind::Long).unwrap();
        assert_eq!(p.name, "dcud");
        assert!(p.chars_saved_per_month > 100);
    }

    #[test]
    fn skips_existing_aliases_and_noise() {
        let h: Vec<HistoryEntry> = (0..6).map(|i| e("docker compose up -d --build", i * 1000)).collect();
        assert!(mine_aliases(&h, &["dcud".into()], 30.0).is_empty());
        let noise: Vec<HistoryEntry> = (0..10).map(|i| e("ls -la --color=always", i * 1000)).collect();
        assert!(mine_aliases(&noise, &[], 30.0).is_empty());
    }

    #[test]
    fn short_names_avoid_real_programs() {
        assert_eq!(short_name("cargo build --release"), "cbr");
        assert_ne!(short_name("cd .."), "cd");
        assert_eq!(short_name("git push"), "gp");
    }

    #[test]
    fn stats_basics() {
        let mut h: Vec<HistoryEntry> = (0..10).map(|i| e("cargo test", i * 1000)).collect();
        h[0].exit_code = Some(101); h[1].exit_code = Some(101);
        h[2].duration_ms = Some(5000);
        let s = stats(&h, 2, 20_000, &Counters::default());
        assert_eq!(s.commands, 10);
        assert_eq!(s.failures, 2);
        assert_eq!(s.most_failing[0], ("cargo test".into(), 2, 10));
        assert_eq!(s.slowest[0].1, 5000);
        assert_eq!(s.time_waiting_ms, 5000);
    }
}
