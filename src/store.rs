//! Persistent storage: one SQLite row per command executed.
//!
//! Design notes:
//! - SQLite in WAL mode: writes don't block reads, and a crash can't corrupt it.
//! - `exit_code` is NULL until the `Done` event arrives. A NULL exit code that
//!   never gets filled means the shell died mid-command (or the user hit Ctrl-C
//!   in a way we didn't see) — still useful signal.
//! - We keep the schema boring on purpose. Clever comes later, on top.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A command as loaded back out of the database.
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub cmd: String,
    pub cwd: String,
    pub exit_code: Option<i32>,
    /// Unix timestamp in milliseconds.
    pub ts_ms: i64,
    pub duration_ms: Option<u64>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS history (
                id           INTEGER PRIMARY KEY,
                session      TEXT    NOT NULL,
                cmd          TEXT    NOT NULL,
                cwd          TEXT    NOT NULL,
                git_branch   TEXT,
                ts_ms        INTEGER NOT NULL,
                exit_code    INTEGER,
                duration_ms  INTEGER
            );

            -- The suggest hot path filters by cwd and orders by recency.
            CREATE INDEX IF NOT EXISTS idx_history_cwd_ts ON history(cwd, ts_ms DESC);
            CREATE INDEX IF NOT EXISTS idx_history_ts     ON history(ts_ms DESC);

            -- Product metrics. One row per counter; incremented in place.
            CREATE TABLE IF NOT EXISTS counters (
                name  TEXT PRIMARY KEY,
                value INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        Ok(Self { conn })
    }

    /// Record that a command is starting. Returns the new row id.
    pub fn record_exec(
        &self,
        session: &str,
        cmd: &str,
        cwd: &str,
        git_branch: Option<&str>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO history (session, cmd, cwd, git_branch, ts_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session, cmd, cwd, git_branch, now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Fill in the outcome of the most recent unfinished command in this session.
    pub fn record_done(&self, session: &str, exit_code: i32, duration_ms: u64) -> rusqlite::Result<bool> {
        // Find the latest row for this session that has no exit code yet.
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM history WHERE session = ?1 AND exit_code IS NULL ORDER BY id DESC LIMIT 1",
                params![session],
                |r| r.get(0),
            )
            .optional()?;

        match id {
            Some(id) => {
                self.conn.execute(
                    "UPDATE history SET exit_code = ?1, duration_ms = ?2 WHERE id = ?3",
                    params![exit_code, duration_ms as i64, id],
                )?;
                Ok(true)
            }
            // A `Done` with no matching `Exec` happens on the very first prompt of a
            // shell session (precmd fires before any command ran). Harmless.
            None => Ok(false),
        }
    }

    /// Load the most recent `limit` entries, newest first.
    /// The predictor works on this in-memory slice — we never run a query per keystroke.
    pub fn recent(&self, limit: usize) -> rusqlite::Result<Vec<HistoryEntry>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT cmd, cwd, exit_code, ts_ms, duration_ms FROM history ORDER BY ts_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(HistoryEntry {
                cmd: r.get(0)?,
                cwd: r.get(1)?,
                exit_code: r.get(2)?,
                ts_ms: r.get(3)?,
                duration_ms: r.get::<_, Option<i64>>(4)?.map(|d| d.max(0) as u64),
            })
        })?;
        rows.collect()
    }

    /// Bulk-insert imported history in one transaction. Skips (cmd, ts_ms) pairs that
    /// already exist so re-running an import is idempotent. Returns rows inserted.
    pub fn import(&mut self, rows: &[(String, i64)]) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let mut n = 0;
        {
            let mut exists = tx.prepare("SELECT 1 FROM history WHERE cmd = ?1 AND ts_ms = ?2 AND session = 'import' LIMIT 1")?;
            let mut ins = tx.prepare("INSERT INTO history (session, cmd, cwd, ts_ms, exit_code) VALUES ('import', ?1, '', ?2, 0)")?;
            for (cmd, ts) in rows {
                if exists.exists(params![cmd, ts])? { continue; }
                ins.execute(params![cmd, ts])?;
                n += 1;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn session_count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(DISTINCT session) FROM history WHERE session != 'import'", [], |r| r.get(0))
    }

    pub fn bump_counter(&self, name: &str, by: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO counters (name, value) VALUES (?1, ?2) ON CONFLICT(name) DO UPDATE SET value = value + ?2",
            params![name, by as i64],
        )?;
        Ok(())
    }

    pub fn counter(&self, name: &str) -> rusqlite::Result<u64> {
        Ok(self
            .conn
            .query_row("SELECT value FROM counters WHERE name = ?1", params![name], |r| r.get::<_, i64>(0))
            .optional()?
            .unwrap_or(0)
            .max(0) as u64)
    }

    #[allow(dead_code)] // used by `fore stats` (Milestone 1)
    pub fn count(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
