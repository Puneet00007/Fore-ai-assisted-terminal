//! The wire protocol between the shell plugin and the daemon.
//!
//! Newline-delimited JSON over a Unix domain socket. One request, one response.
//! This file is the *contract*: the shell plugin and the daemon can each be
//! rewritten freely as long as they both speak this.

use serde::{Deserialize, Serialize};

/// Everything the shell can send us.
///
/// `#[serde(tag = "type")]` makes the JSON look like `{"type":"exec", ...}`
/// instead of `{"Exec": {...}}` — friendlier to write from a shell script.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// A command is about to run (sent from zsh's `preexec` hook).
    Exec {
        cmd: String,
        cwd: String,
        session: String,
        /// Git branch if we're inside a repo. The shell computes this because it's
        /// already there; the daemon shouldn't shell out for it.
        #[serde(default)]
        git_branch: Option<String>,
    },

    /// The last command finished (sent from zsh's `precmd` hook).
    Done {
        session: String,
        exit_code: i32,
        duration_ms: u64,
        /// Last lines of stderr, if the shell captured them. On failure the daemon
        /// starts computing a fix immediately — before the user asks.
        #[serde(default)]
        stderr_tail: Option<String>,
    },

    /// Ctrl+/ — "why did that fail and what should I run?"
    /// Returns the precomputed fix if ready, otherwise computes it now.
    Fix {
        session: String,
    },

    /// Ctrl+Space — English → command.
    Nl {
        session: String,
        cwd: String,
        request: String,
        #[serde(default)]
        git_branch: Option<String>,
    },

    /// Classify a command line without running anything (also used by `fore check`).
    Assess {
        cmd: String,
    },

    /// Enter was pressed: guards + previews + insights, before the command runs.
    Preflight {
        session: String,
        cmd: String,
        cwd: String,
        #[serde(default)]
        env: crate::preflight::EnvSnapshot,
    },

    /// The user accepted a ghost-text suggestion (for metrics).
    Accepted {
        session: String,
        /// Characters the user did not have to type.
        chars: u64,
    },

    /// Usage statistics.
    Stats,

    /// Re-read history from the database (after `fore import`).
    Reload,

    /// Alias proposals mined from history.
    Aliases {
        #[serde(default)]
        existing: Vec<String>,
    },

    /// User is typing; give us the best completion for `prefix` in this context.
    /// This is the hot path — it must answer in single-digit milliseconds.
    Suggest {
        prefix: String,
        cwd: String,
        session: String,
    },

    /// Health check / latency probe.
    Ping,

    /// Ask the daemon to shut down cleanly.
    Shutdown,
}

/// Everything the daemon can answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    /// Present only for `Suggest`. `None` means "nothing good to offer" —
    /// the shell should show nothing rather than a bad guess.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Human-readable detail (errors, pong, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Present for `Fix` and `Nl`: the proposed command with its safety assessment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proposal: Option<crate::assist::Proposal>,
    /// Present for `Assess`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assessment: Option<crate::safety::Assessment>,
    /// Present for `Preflight`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight: Option<crate::preflight::Preflight>,
    /// Present for `Stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<crate::insights::Stats>,
    /// Present for `Aliases`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<crate::insights::AliasProposal>>,
    /// How long the daemon spent on this request, in microseconds.
    /// We measure latency from day one — it's a product feature, not a debug detail.
    pub took_us: u64,
}

impl Response {
    fn base(took_us: u64) -> Self {
        Self { ok: true, suggestion: None, message: None, proposal: None, assessment: None, preflight: None, stats: None, aliases: None, took_us }
    }

    pub fn ok(took_us: u64) -> Self {
        Self::base(took_us)
    }

    pub fn suggestion(s: Option<String>, took_us: u64) -> Self {
        Self { suggestion: s, ..Self::base(took_us) }
    }

    pub fn message(msg: impl Into<String>, took_us: u64) -> Self {
        Self { message: Some(msg.into()), ..Self::base(took_us) }
    }

    pub fn error(msg: impl Into<String>, took_us: u64) -> Self {
        Self { ok: false, message: Some(msg.into()), ..Self::base(took_us) }
    }

    pub fn proposal(p: crate::assist::Proposal, took_us: u64) -> Self {
        Self { proposal: Some(p), ..Self::base(took_us) }
    }

    pub fn assessment(a: crate::safety::Assessment, took_us: u64) -> Self {
        Self { assessment: Some(a), ..Self::base(took_us) }
    }

    pub fn preflight(p: crate::preflight::Preflight, took_us: u64) -> Self {
        Self { preflight: Some(p), ..Self::base(took_us) }
    }

    pub fn stats(s: crate::insights::Stats, took_us: u64) -> Self {
        Self { stats: Some(s), ..Self::base(took_us) }
    }

    pub fn aliases(a: Vec<crate::insights::AliasProposal>, took_us: u64) -> Self {
        Self { aliases: Some(a), ..Self::base(took_us) }
    }
}
