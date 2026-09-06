//! Tier 0 predictor: pure in-memory scoring, no I/O, no model.
//!
//! Given what the user has typed so far (`prefix`) and where they are (`cwd`),
//! pick the single best full command from history.
//!
//! Every candidate is scored on four signals:
//!   1. directory match    (commands are local to projects)
//!   2. recency            (exponential decay, half-life 7 days)
//!   3. frequency          (log-scaled so one command can't dominate)
//!   4. outcome            (penalize commands that failed the last time)
//!
//! This whole thing runs on a Vec in memory; the hot path touches no disk.

use crate::store::HistoryEntry;
use std::collections::HashMap;

/// How fast old commands lose relevance. After this many ms, recency score halves.
const HALF_LIFE_MS: f64 = 7.0 * 24.0 * 3600.0 * 1000.0;

/// Ignore trivially short prefixes — a suggestion for "l" is noise.
const MIN_PREFIX_LEN: usize = 1;

pub struct Predictor {
    /// Newest first. Bounded by `capacity`.
    history: Vec<HistoryEntry>,
    capacity: usize,
}

#[derive(Debug)]
struct Candidate {
    cmd: String,
    score: f64,
}

impl Predictor {
    pub fn new(mut history: Vec<HistoryEntry>, capacity: usize) -> Self {
        history.truncate(capacity);
        Self { history, capacity }
    }

    /// Called on every `Exec`: push to the front, drop the oldest if over capacity.
    pub fn push(&mut self, entry: HistoryEntry) {
        self.history.insert(0, entry);
        if self.history.len() > self.capacity {
            self.history.pop();
        }
    }

    /// Called on every `Done`: attach the exit code to the newest entry for this cwd-less
    /// context. (We match on "most recent without an exit code", same rule as the store.)
    pub fn mark_done(&mut self, exit_code: i32, duration_ms: u64) {
        if let Some(e) = self.history.iter_mut().find(|e| e.exit_code.is_none()) {
            e.exit_code = Some(exit_code);
            e.duration_ms = Some(duration_ms);
        }
    }

    /// Replace everything (after an import).
    pub fn replace(&mut self, mut history: Vec<HistoryEntry>) {
        history.truncate(self.capacity);
        self.history = history;
    }

    /// Read-only view for stats / alias mining.
    pub fn all(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// The last `n` commands run in (or under) `cwd`, newest first. This is the
    /// context we hand to the model: small, local, relevant.
    pub fn recent_in(&self, cwd: &str, n: usize) -> Vec<HistoryEntry> {
        let mut out: Vec<HistoryEntry> = self
            .history
            .iter()
            .filter(|e| e.cwd == cwd || e.cwd.starts_with(cwd))
            .take(n)
            .cloned()
            .collect();
        // If the directory is new, fall back to global recent so the model still
        // learns the user's tool preferences.
        if out.len() < 3 {
            out = self.history.iter().take(n).cloned().collect();
        }
        out
    }

    /// The hot path.
    pub fn suggest(&self, prefix: &str, cwd: &str, now_ms: i64) -> Option<String> {
        let prefix = prefix.trim_start();
        if prefix.len() < MIN_PREFIX_LEN {
            return None;
        }

        // Aggregate per distinct command text so frequency can be counted.
        let mut agg: HashMap<&str, Candidate> = HashMap::new();
        let mut counts: HashMap<&str, u32> = HashMap::new();

        for e in &self.history {
            // Must extend what the user typed, and must add something beyond it.
            if !e.cmd.starts_with(prefix) || e.cmd.len() == prefix.len() {
                continue;
            }

            let mut s = 0.0;

            // 1. Directory affinity
            s += dir_affinity(&e.cwd, cwd);

            // 2. Recency: 2.0 * 0.5^(age / half_life)
            let age = (now_ms - e.ts_ms).max(0) as f64;
            s += 2.0 * (0.5f64).powf(age / HALF_LIFE_MS);

            // 4. Outcome (applied per occurrence; failures drag the total down)
            if let Some(code) = e.exit_code
                && code != 0 {
                    s -= 2.0;
                }

            *counts.entry(e.cmd.as_str()).or_insert(0) += 1;
            agg.entry(e.cmd.as_str())
                .and_modify(|c| c.score += s)
                .or_insert(Candidate { cmd: e.cmd.clone(), score: s });
        }

        // 3. Frequency bonus, log-scaled. Applied once per distinct command.
        for (cmd, c) in agg.iter_mut() {
            let n = counts.get(cmd).copied().unwrap_or(1) as f64;
            c.score += n.log2();
        }

        agg.into_values()
            .filter(|c| c.score > 0.0)
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .map(|c| c.cmd)
    }
}

/// How related are two directories?
///   same dir            → 3.0
///   one contains other  → 1.5   (e.g. repo root vs repo/src)
///   unrelated           → 0.0
fn dir_affinity(a: &str, b: &str) -> f64 {
    if a == b {
        3.0
    } else if a.starts_with(b) || b.starts_with(a) {
        1.5
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(cmd: &str, cwd: &str, exit: Option<i32>, age_ms: i64) -> HistoryEntry {
        HistoryEntry { cmd: cmd.into(), cwd: cwd.into(), exit_code: exit, ts_ms: 1_000_000_000 - age_ms, duration_ms: None }
    }

    #[test]
    fn prefers_same_directory() {
        let p = Predictor::new(
            vec![
                entry("npm test", "/proj/js", Some(0), 1000),
                entry("cargo test", "/proj/rs", Some(0), 1000),
            ],
            100,
        );
        // Both commands share no prefix, so use a one-char prefix each and check cwd wins
        // when there IS ambiguity: add a cross-directory decoy for each.
        let p2 = Predictor::new(
            vec![
                entry("cargo test", "/proj/rs", Some(0), 1000),
                entry("cargo build", "/proj/js", Some(0), 500), // more recent, wrong dir
            ],
            100,
        );
        assert_eq!(p2.suggest("c", "/proj/rs", 1_000_000_000).as_deref(), Some("cargo test"));
        assert_eq!(p2.suggest("c", "/proj/js", 1_000_000_000).as_deref(), Some("cargo build"));
        assert_eq!(p.suggest("n", "/proj/js", 1_000_000_000).as_deref(), Some("npm test"));
    }

    #[test]
    fn penalizes_failures() {
        let p = Predictor::new(
            vec![
                entry("git psuh", "/p", Some(1), 500),   // typo, failed, more recent
                entry("git push", "/p", Some(0), 5000),
            ],
            100,
        );
        assert_eq!(p.suggest("git p", "/p", 1_000_000_000).as_deref(), Some("git push"));
    }

    #[test]
    fn frequency_beats_single_recent() {
        let mut h = vec![entry("ls -la", "/p", Some(0), 100)];
        for i in 0..5 {
            h.push(entry("ls -lh", "/p", Some(0), 1000 + i));
        }
        let p = Predictor::new(h, 100);
        assert_eq!(p.suggest("ls", "/p", 1_000_000_000).as_deref(), Some("ls -lh"));
    }

    #[test]
    fn nothing_for_exact_match() {
        let p = Predictor::new(vec![entry("ls", "/p", Some(0), 100)], 100);
        assert_eq!(p.suggest("ls", "/p", 1_000_000_000), None);
    }
}
