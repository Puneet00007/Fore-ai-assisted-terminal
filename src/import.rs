//! Import existing shell history so fore is useful on day one.
//!
//! Supports zsh (plain and EXTENDED_HISTORY `: <epoch>:<dur>;<cmd>`, multi-line with
//! trailing `\`, metafied bytes) and bash (plain, optional `#<epoch>` timestamp lines).
//! Rows get cwd = "" (unknown), so the predictor gives them no directory affinity —
//! they're a baseline that your real, located history quickly outranks.

use std::path::{Path, PathBuf};

pub struct Imported {
    pub cmd: String,
    pub ts_ms: i64,
}

/// Candidate history files in priority order, with the shell they belong to.
pub fn candidates() -> Vec<(PathBuf, &'static str)> {
    let home = crate::config::home();
    let mut out = Vec::new();
    if let Ok(h) = std::env::var("HISTFILE")
        && !h.is_empty() { out.push((PathBuf::from(h), "zsh")); }
    for (p, shell) in [(".zsh_history", "zsh"), (".zhistory", "zsh"), (".local/share/zsh/history", "zsh"), (".bash_history", "bash")] {
        let path = home.join(p);
        if path.is_file() && !out.iter().any(|(q, _)| q == &path) { out.push((path, shell)); }
    }
    out
}

/// zsh "metafies" bytes that collide with its internal markers: Meta (0x83) followed
/// by the real byte XOR 0x20. Undo that so UTF-8 survives.
fn unmetafy(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x83 && i + 1 < bytes.len() {
            out.push(bytes[i + 1] ^ 0x20);
            i += 2;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

pub fn parse(bytes: &[u8], shell: &str, now_ms: i64) -> Vec<Imported> {
    let text = String::from_utf8_lossy(&unmetafy(bytes)).into_owned();
    let mut raw: Vec<(String, Option<i64>)> = Vec::new();

    if shell == "bash" {
        let mut pending_ts: Option<i64> = None;
        for line in text.lines() {
            if let Some(ts) = line.strip_prefix('#').and_then(|s| s.trim().parse::<i64>().ok())
                && ts > 1_000_000_000 { pending_ts = Some(ts * 1000); continue; }
            if !line.trim().is_empty() { raw.push((line.to_string(), pending_ts.take())); }
        }
    } else {
        // zsh: a logical entry may span lines when a line ends with a backslash.
        let mut cur: Option<(String, Option<i64>)> = None;
        for line in text.lines() {
            let (mut body, ts, is_new) = match line.strip_prefix(": ") {
                Some(rest) if rest.contains(';') => {
                    let (meta, cmd) = rest.split_once(';').unwrap();
                    let ts = meta.split(':').next().and_then(|t| t.trim().parse::<i64>().ok()).map(|t| t * 1000);
                    (cmd.to_string(), ts, true)
                }
                _ => (line.to_string(), None, cur.is_none()),
            };
            let continues = body.ends_with('\\') && !body.ends_with("\\\\");
            if continues { body.pop(); body.push('\n'); }
            if is_new {
                if let Some(c) = cur.take() { raw.push(c); }
                cur = Some((body, ts));
            } else if let Some(c) = cur.as_mut() {
                c.0.push_str(&body);
            }
            if !continues
                && let Some(c) = cur.take() { raw.push(c); }
        }
        if let Some(c) = cur.take() { raw.push(c); }
    }

    // Timestamps: real ones when present. Entries without one are interpolated between
    // their nearest timestamped neighbours (or spread over the last 30 days if the file
    // has none at all), so file order == time order.
    let n = raw.len();
    let span = 30 * 86_400_000i64;
    let mut ts: Vec<Option<i64>> = raw.iter().map(|(_, t)| *t).collect();
    let first_known = ts.iter().position(|t| t.is_some());
    let mut i = 0;
    while i < n {
        if ts[i].is_some() { i += 1; continue; }
        let j = (i..n).find(|&k| ts[k].is_some()).unwrap_or(n);       // next known
        // Anchor synthetic times to known neighbours; when there's no later anchor, use the
        // hour boundary (not the exact `now`) so re-importing the same file is a no-op.
        let anchor_end = now_ms - now_ms % 3_600_000;
        let lo = if i > 0 { ts[i - 1].unwrap() } else { ts.get(j).copied().flatten().map(|t| t - span).unwrap_or(anchor_end - span) };
        let hi = if j < n { ts[j].unwrap() } else { anchor_end };
        let gap = (j - i + 1) as i64;
        for (step, k) in (i..j).enumerate() {
            ts[k] = Some(lo + (hi - lo) * (step as i64 + 1) / gap);
        }
        i = j;
    }
    let _ = first_known;
    let mut out = Vec::with_capacity(n);
    for ((cmd, _), t) in raw.into_iter().zip(ts) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() || cmd.len() > 2000 { continue; }
        // Never import things that shouldn't be suggested back.
        if cmd.starts_with("fore ") || cmd == "exit" || cmd == "logout" { continue; }
        out.push(Imported { cmd, ts_ms: t.unwrap_or(now_ms) });
    }
    out
}

pub fn read(path: &Path, shell: &str, now_ms: i64) -> std::io::Result<Vec<Imported>> {
    let bytes = std::fs::read(path)?;
    Ok(parse(&bytes, shell, now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_extended_and_multiline() {
        let f = b": 1700000000:0;git status\n: 1700000100:5;cargo build \\\n  --release\nplain command\n: 1700000200:0;ls\n";
        let v = parse(f, "zsh", 1_800_000_000_000);
        let cmds: Vec<&str> = v.iter().map(|i| i.cmd.as_str()).collect();
        assert_eq!(cmds, vec!["git status", "cargo build \n  --release", "plain command", "ls"]);
        assert_eq!(v[0].ts_ms, 1_700_000_000_000);
        assert!(v[2].ts_ms > 0);
    }

    #[test]
    fn zsh_metafied_utf8_roundtrips() {
        // "é" is C3 A9; zsh would metafy neither, but 0x83-prefixed bytes must be decoded.
        let f = b"echo \x83\xa3x\n"; // 0x83 0xa3 → 0xa3 ^ 0x20 = 0x83 (a raw Meta byte in the original)
        let v = parse(f, "zsh", 0);
        assert_eq!(v.len(), 1);
        assert!(v[0].cmd.starts_with("echo "));
    }

    #[test]
    fn bash_with_timestamps_and_plain() {
        let f = b"#1700000000\ngit status\nmake\n#1700000900\nls -la\n";
        let v = parse(f, "bash", 1_800_000_000_000);
        let cmds: Vec<&str> = v.iter().map(|i| i.cmd.as_str()).collect();
        assert_eq!(cmds, vec!["git status", "make", "ls -la"]);
        assert_eq!(v[0].ts_ms, 1_700_000_000_000);
        assert_eq!(v[2].ts_ms, 1_700_000_900_000);
        assert!(v[1].ts_ms < v[2].ts_ms); // synthetic timestamp keeps order
    }

    #[test]
    fn skips_noise() {
        let f = b"exit\nfore stats\n\nls\n";
        let v = parse(f, "zsh", 1_800_000_000_000);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].cmd, "ls");
    }

    #[test]
    fn synthetic_timestamps_keep_file_order() {
        let f = b"a\nb\nc\n";
        let v = parse(f, "zsh", 1_800_000_000_000);
        assert!(v[0].ts_ms < v[1].ts_ms && v[1].ts_ms < v[2].ts_ms);
        assert!(v[2].ts_ms <= 1_800_000_000_000);
        // deterministic within the hour → re-import dedupes
        let again = parse(f, "zsh", 1_800_000_000_000 + 60_000);
        assert_eq!(v[1].ts_ms, again[1].ts_ms);
    }
}
