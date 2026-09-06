//! Safe rm + undo.
//!
//! `fore rm <args>` parses rm's flags, moves each target into a per-operation trash
//! directory, and writes a journal entry. `fore undo` moves the most recent operation
//! back. `fore trash list|purge` manages the store. The zsh plugin turns `rm` into
//! `fore rm` when [undo].safe_rm is on (an alias, so `command rm` / `\rm` bypass it).
//!
//! Layout:
//!   ~/.local/share/fore/trash/<op-id>/journal.json
//!   ~/.local/share/fore/trash/<op-id>/0/<original-name>
//!   ~/.local/share/fore/trash/<op-id>/1/<original-name>   (each target gets its own slot:
//!                                                          two files named `x` from different dirs)
//!
//! Cross-filesystem moves (e.g. /tmp on tmpfs → home) fall back to copy+delete, which is
//! slower but still reversible. Anything we can't move, we refuse to delete — we never
//! silently downgrade to a real rm.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
pub struct Journal {
    pub id: String,
    pub ts_ms: i64,
    pub cwd: String,
    pub cmdline: String,
    pub entries: Vec<Entry>,
    pub total_bytes: u64,
    pub total_files: usize,
    pub restored: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Absolute original path.
    pub from: PathBuf,
    /// Path inside the trash op dir.
    pub to: PathBuf,
    pub is_dir: bool,
    pub bytes: u64,
    pub files: usize,
}

pub struct RmPlan {
    pub recursive: bool,
    pub force: bool,
    pub targets: Vec<String>,
    pub interactive: bool,
    pub verbose: bool,
    pub dir_only: bool,
}

/// Parse rm's arguments the way GNU/BSD rm does (combined short flags, `--`, long flags).
pub fn parse_rm_args(args: &[String]) -> RmPlan {
    let mut p = RmPlan { recursive: false, force: false, targets: vec![], interactive: false, verbose: false, dir_only: false };
    let mut no_more_flags = false;
    for a in args {
        if no_more_flags || !a.starts_with('-') || a == "-" {
            p.targets.push(a.clone());
            continue;
        }
        if a == "--" { no_more_flags = true; continue; }
        match a.as_str() {
            "--recursive" => p.recursive = true,
            "--force" => p.force = true,
            "--interactive" | "--interactive=always" => p.interactive = true,
            "--verbose" => p.verbose = true,
            "--dir" => p.dir_only = true,
            _ if a.starts_with("--") => {} // --one-file-system, --preserve-root, …: irrelevant to us
            _ => {
                for c in a[1..].chars() {
                    match c {
                        'r' | 'R' => p.recursive = true,
                        'f' => p.force = true,
                        'i' | 'I' => p.interactive = true,
                        'v' => p.verbose = true,
                        'd' => p.dir_only = true,
                        _ => {}
                    }
                }
            }
        }
    }
    p
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn op_id() -> String {
    let t = now_ms();
    format!("{}-{:06}", t, std::process::id() % 1_000_000)
}

/// (files, bytes) under a path; a file counts as 1.
fn measure(p: &Path) -> (usize, u64) {
    let Ok(m) = fs::symlink_metadata(p) else { return (0, 0) };
    if !m.is_dir() || m.file_type().is_symlink() {
        return (1, m.len());
    }
    let (mut n, mut b) = (0usize, 0u64);
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() && !m.file_type().is_symlink() { stack.push(e.path()); } else { n += 1; b += m.len(); }
        }
    }
    (n, b)
}

/// Move with cross-device fallback.
fn move_path(from: &Path, to: &Path) -> std::io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) /* EXDEV */ => {
            copy_recursive(from, to)?;
            let m = fs::symlink_metadata(from)?;
            if m.is_dir() && !m.file_type().is_symlink() { fs::remove_dir_all(from) } else { fs::remove_file(from) }
        }
        Err(e) => Err(e),
    }
}

fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    let m = fs::symlink_metadata(from)?;
    if m.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to)?;
        return Ok(());
    }
    if m.is_dir() {
        fs::create_dir_all(to)?;
        for e in fs::read_dir(from)? {
            let e = e?;
            copy_recursive(&e.path(), &to.join(e.file_name()))?;
        }
        fs::set_permissions(to, m.permissions())?;
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

pub struct RmOutcome {
    pub journal: Option<Journal>,
    pub messages: Vec<String>,
    pub exit_code: i32,
}

/// Perform a safe rm. `max_bytes` → refuse (caller falls back to real rm) if exceeded.
pub fn safe_rm(trash: &Path, cwd: &Path, cmdline: &str, plan: &RmPlan, max_bytes: u64) -> RmOutcome {
    let mut messages = Vec::new();
    let mut exit_code = 0;

    if plan.targets.is_empty() {
        return RmOutcome { journal: None, messages: vec!["rm: missing operand".into()], exit_code: 1 };
    }

    // Validate targets first (so a partial failure doesn't leave a half-done op).
    struct Prepared { from: PathBuf, is_dir: bool, files: usize, bytes: u64 }
    let mut prepared = Vec::new();
    let mut total_bytes = 0u64;
    for t in &plan.targets {
        let from = if Path::new(t).is_absolute() { PathBuf::from(t) } else { cwd.join(t) };
        let from = normalize(&from);
        // Refuse the classic catastrophes outright, regardless of flags.
        let s = from.to_string_lossy();
        if s == "/" || s == crate::config::home().to_string_lossy() || s == "/home" || s == "/usr" || s == "/etc" || s == "/var" {
            return RmOutcome { journal: None, messages: vec![format!("rm: refusing to remove `{t}` (protected path)")], exit_code: 1 };
        }
        match fs::symlink_metadata(&from) {
            Err(_) => {
                if !plan.force { messages.push(format!("rm: cannot remove '{t}': No such file or directory")); exit_code = 1; }
            }
            Ok(m) => {
                let is_dir = m.is_dir() && !m.file_type().is_symlink();
                if is_dir && !plan.recursive {
                    if plan.dir_only && fs::read_dir(&from).map(|mut r| r.next().is_none()).unwrap_or(false) {
                        // rm -d on an empty dir is fine
                    } else {
                        messages.push(format!("rm: cannot remove '{t}': Is a directory"));
                        exit_code = 1;
                        continue;
                    }
                }
                let (files, bytes) = measure(&from);
                total_bytes += bytes;
                prepared.push(Prepared { from, is_dir, files, bytes });
            }
        }
    }
    if prepared.is_empty() {
        return RmOutcome { journal: None, messages, exit_code };
    }
    if total_bytes > max_bytes {
        messages.push(format!("fore: {} exceeds undo.max_bytes ({}); use `command rm` to delete for real", crate::preflight::fmt_bytes(total_bytes), crate::preflight::fmt_bytes(max_bytes)));
        return RmOutcome { journal: None, messages, exit_code: 3 };
    }

    // Trash the lot.
    let id = op_id();
    let op_dir = trash.join(&id);
    if let Err(e) = fs::create_dir_all(&op_dir) {
        return RmOutcome { journal: None, messages: vec![format!("fore: cannot create trash dir {}: {e}", op_dir.display())], exit_code: 1 };
    }
    let mut entries = Vec::new();
    let (mut tf, mut tb) = (0usize, 0u64);
    for (i, p) in prepared.into_iter().enumerate() {
        let slot = op_dir.join(i.to_string());
        let _ = fs::create_dir_all(&slot);
        let name = p.from.file_name().map(|s| s.to_os_string()).unwrap_or_else(|| "item".into());
        let to = slot.join(name);
        match move_path(&p.from, &to) {
            Ok(()) => {
                if plan.verbose { messages.push(format!("removed '{}'", p.from.display())); }
                tf += p.files; tb += p.bytes;
                entries.push(Entry { from: p.from, to, is_dir: p.is_dir, bytes: p.bytes, files: p.files });
            }
            Err(e) => {
                messages.push(format!("rm: cannot remove '{}': {e}", p.from.display()));
                exit_code = 1;
            }
        }
    }
    if entries.is_empty() {
        let _ = fs::remove_dir_all(&op_dir);
        return RmOutcome { journal: None, messages, exit_code };
    }
    let journal = Journal { id: id.clone(), ts_ms: now_ms(), cwd: cwd.to_string_lossy().into_owned(), cmdline: cmdline.to_string(), entries, total_bytes: tb, total_files: tf, restored: false };
    if let Err(e) = write_journal(&op_dir, &journal) {
        messages.push(format!("fore: journal write failed ({e}); files are in {}", op_dir.display()));
    }
    RmOutcome { journal: Some(journal), messages, exit_code }
}

fn write_journal(op_dir: &Path, j: &Journal) -> std::io::Result<()> {
    fs::write(op_dir.join("journal.json"), serde_json::to_vec_pretty(j).unwrap_or_default())
}

fn normalize(p: &Path) -> PathBuf {
    // Resolve `.` and `..` lexically (don't follow symlinks — rm doesn't either).
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => { out.pop(); }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// All journals, newest first.
pub fn list(trash: &Path) -> Vec<Journal> {
    let mut out: Vec<Journal> = fs::read_dir(trash).into_iter().flatten().flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("journal.json")).ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.ts_ms));
    out
}

pub enum UndoResult {
    Restored { journal: Journal, conflicts: Vec<String> },
    NothingToUndo,
    Failed(String),
}

/// Restore the most recent non-restored op (or a specific id).
pub fn undo(trash: &Path, id: Option<&str>) -> UndoResult {
    let journals = list(trash);
    let Some(mut j) = journals.into_iter().find(|j| !j.restored && id.is_none_or(|i| j.id == i || j.id.starts_with(i))) else {
        return UndoResult::NothingToUndo;
    };
    let op_dir = trash.join(&j.id);
    let mut conflicts = Vec::new();
    let mut restored_any = false;
    for e in &j.entries {
        if e.from.exists() {
            // Something new lives there. Don't clobber it; restore next to it.
            let alt = alt_name(&e.from);
            match move_path(&e.to, &alt) {
                Ok(()) => { conflicts.push(format!("{} already existed — restored as {}", e.from.display(), alt.display())); restored_any = true; }
                Err(err) => conflicts.push(format!("could not restore {}: {err}", e.from.display())),
            }
            continue;
        }
        if let Some(parent) = e.from.parent() { let _ = fs::create_dir_all(parent); }
        match move_path(&e.to, &e.from) {
            Ok(()) => restored_any = true,
            Err(err) => conflicts.push(format!("could not restore {}: {err}", e.from.display())),
        }
    }
    if !restored_any && !conflicts.is_empty() {
        return UndoResult::Failed(conflicts.join("\n"));
    }
    j.restored = true;
    let _ = write_journal(&op_dir, &j);
    // Remove empty slot dirs; keep the journal as a record.
    for i in 0..j.entries.len() { let _ = fs::remove_dir(op_dir.join(i.to_string())); }
    UndoResult::Restored { journal: j, conflicts }
}

fn alt_name(p: &Path) -> PathBuf {
    let stem = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "restored".into());
    let parent = p.parent().unwrap_or(Path::new("."));
    for i in 1..1000 {
        let cand = parent.join(format!("{stem}.restored{}", if i == 1 { String::new() } else { format!("-{i}") }));
        if !cand.exists() { return cand; }
    }
    parent.join(format!("{stem}.restored-{}", now_ms()))
}

/// Delete ops older than `keep_days` (and restored ops older than 1 day). Returns (ops, bytes) purged.
pub fn purge(trash: &Path, keep_days: u64, all: bool) -> (usize, u64) {
    let cutoff = now_ms() - (keep_days as i64) * 86_400_000;
    let (mut n, mut b) = (0usize, 0u64);
    for j in list(trash) {
        let old = j.ts_ms < cutoff || (j.restored && j.ts_ms < now_ms() - 86_400_000);
        if (all || old)
            && fs::remove_dir_all(trash.join(&j.id)).is_ok() { n += 1; b += if j.restored { 0 } else { j.total_bytes }; }
    }
    (n, b)
}

pub fn trash_size(trash: &Path) -> (usize, u64) {
    let js = list(trash);
    let live: Vec<&Journal> = js.iter().filter(|j| !j.restored).collect();
    (live.len(), live.iter().map(|j| j.total_bytes).sum())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("fore-undo-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let work = root.join("work"); let trash = root.join("trash");
        fs::create_dir_all(&work).unwrap(); fs::create_dir_all(&trash).unwrap();
        (work, trash)
    }

    #[test]
    fn parses_rm_flags_like_rm() {
        let p = parse_rm_args(&["-rf".into(), "a".into(), "--".into(), "-weird".into()]);
        assert!(p.recursive && p.force);
        assert_eq!(p.targets, vec!["a", "-weird"]);
        let p = parse_rm_args(&["--recursive".into(), "-v".into(), "b".into()]);
        assert!(p.recursive && p.verbose && !p.force);
    }

    #[test]
    fn rm_then_undo_roundtrip() {
        let (work, trash) = sandbox("roundtrip");
        fs::create_dir_all(work.join("build/sub")).unwrap();
        fs::write(work.join("build/a.o"), b"aaaa").unwrap();
        fs::write(work.join("build/sub/b.o"), b"bb").unwrap();
        fs::write(work.join("note.txt"), b"n").unwrap();
        let plan = parse_rm_args(&["-rf".into(), "build".into(), "note.txt".into()]);
        let out = safe_rm(&trash, &work, "rm -rf build note.txt", &plan, u64::MAX);
        assert_eq!(out.exit_code, 0, "{:?}", out.messages);
        assert!(!work.join("build").exists() && !work.join("note.txt").exists());
        let j = out.journal.unwrap();
        assert_eq!(j.total_files, 3);
        assert_eq!(j.total_bytes, 7);

        match undo(&trash, None) {
            UndoResult::Restored { journal, conflicts } => {
                assert!(conflicts.is_empty(), "{conflicts:?}");
                assert_eq!(journal.id, j.id);
            }
            _ => panic!("expected restore"),
        }
        assert_eq!(fs::read(work.join("build/sub/b.o")).unwrap(), b"bb");
        assert_eq!(fs::read(work.join("note.txt")).unwrap(), b"n");
        assert!(matches!(undo(&trash, None), UndoResult::NothingToUndo));
    }

    #[test]
    fn refuses_dir_without_r_and_missing_without_f() {
        let (work, trash) = sandbox("refuse");
        fs::create_dir_all(work.join("d")).unwrap();
        let out = safe_rm(&trash, &work, "rm d", &parse_rm_args(&["d".into()]), u64::MAX);
        assert_eq!(out.exit_code, 1);
        assert!(work.join("d").exists());
        let out = safe_rm(&trash, &work, "rm nope", &parse_rm_args(&["nope".into()]), u64::MAX);
        assert_eq!(out.exit_code, 1);
        let out = safe_rm(&trash, &work, "rm -f nope", &parse_rm_args(&["-f".into(), "nope".into()]), u64::MAX);
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn undo_does_not_clobber_newer_file() {
        let (work, trash) = sandbox("clobber");
        fs::write(work.join("x"), b"old").unwrap();
        safe_rm(&trash, &work, "rm x", &parse_rm_args(&["x".into()]), u64::MAX);
        fs::write(work.join("x"), b"new").unwrap();
        match undo(&trash, None) {
            UndoResult::Restored { conflicts, .. } => assert_eq!(conflicts.len(), 1),
            _ => panic!(),
        }
        assert_eq!(fs::read(work.join("x")).unwrap(), b"new");
        assert_eq!(fs::read(work.join("x.restored")).unwrap(), b"old");
    }

    #[test]
    fn max_bytes_refuses() {
        let (work, trash) = sandbox("max");
        fs::write(work.join("big"), vec![0u8; 5000]).unwrap();
        let out = safe_rm(&trash, &work, "rm big", &parse_rm_args(&["big".into()]), 1000);
        assert_eq!(out.exit_code, 3);
        assert!(work.join("big").exists());
    }

    #[test]
    fn protected_paths() {
        let (work, trash) = sandbox("prot");
        let out = safe_rm(&trash, &work, "rm -rf /", &parse_rm_args(&["-rf".into(), "/".into()]), u64::MAX);
        assert_eq!(out.exit_code, 1);
        assert!(out.messages[0].contains("protected"));
    }

    #[test]
    fn purge_by_age() {
        let (work, trash) = sandbox("purge");
        fs::write(work.join("f"), b"1").unwrap();
        safe_rm(&trash, &work, "rm f", &parse_rm_args(&["f".into()]), u64::MAX);
        assert_eq!(purge(&trash, 14, false).0, 0);
        assert_eq!(purge(&trash, 14, true).0, 1);
        assert_eq!(list(&trash).len(), 0);
    }
}
