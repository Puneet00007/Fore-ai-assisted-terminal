//! Cloud/local model client. Speaks the OpenAI-compatible `/chat/completions` API,
//! which means it works unchanged with: OpenAI, Ollama, LM Studio, llama.cpp server,
//! Groq, OpenRouter, Together, vLLM… Configure with three env vars:
//!
//!   FORE_LLM_BASE_URL   default http://localhost:11434/v1   (Ollama)
//!   FORE_LLM_MODEL      default qwen2.5-coder:1.5b
//!   FORE_LLM_API_KEY    optional
//!
//! Configuration comes from `config.rs` (file + env). HARD RULE: this module never sees raw text. Callers redact first. To make that
//! impossible to forget, the only way in is through `Redacted` — a newtype that can
//! only be constructed by the redactor.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A string that has passed through `Redactor::redact`. The private field means
/// no other module can construct one — they must go through `Redactor::wrap`.
#[derive(Debug, Clone)]
pub struct Redacted(pub(crate) String);

impl Redacted {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    /// Newer OpenAI models (gpt-5 / o-series) reject `max_tokens` and a non-default
    /// temperature; on that 400 we retry once in their shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: MessageOwned,
}
#[derive(Deserialize)]
struct MessageOwned {
    content: String,
}

pub struct Llm {
    cfg: LlmConfig,
    http: reqwest::Client,
}

impl Llm {
    pub fn new(cfg: LlmConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .expect("http client");
        Self { cfg, http }
    }

    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// One round trip. `system` and `user` must both be redacted — enforced by the type.
    pub async fn complete(&self, system: &Redacted, user: &Redacted, max_tokens: u32) -> Result<String, String> {
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let mut body = ChatRequest {
            model: &self.cfg.model,
            messages: vec![
                Message { role: "system", content: system.as_str() },
                Message { role: "user", content: user.as_str() },
            ],
            temperature: Some(0.1),
            max_tokens: Some(max_tokens),
            max_completion_tokens: None,
        };
        let (mut status, mut text) = self.post(&url, &body).await?;
        if status.as_u16() == 400 && wants_new_openai_shape(&text) {
            body.max_tokens = None;
            body.max_completion_tokens = Some(max_tokens);
            body.temperature = None;
            (status, text) = self.post(&url, &body).await?;
        }
        if !status.is_success() {
            return Err(format!("llm {status}: {}", text.chars().take(300).collect::<String>()));
        }
        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|e| format!("llm parse: {e}"))?;
        parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| "llm: empty response".into())
    }

    async fn post(&self, url: &str, body: &ChatRequest<'_>) -> Result<(reqwest::StatusCode, String), String> {
        let req = apply_auth(self.http.post(url).json(body), &self.cfg.base_url, self.cfg.api_key.as_deref());
        let resp = req.send().await.map_err(|e| format!("llm request: {}", error_chain(&e)))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("llm body: {}", error_chain(&e)))?;
        Ok((status, text))
    }
}

/// gpt-5 / o-series answer 400 with "Unsupported parameter: 'max_tokens' … use 'max_completion_tokens'"
/// or "'temperature' does not support 0.1". Same fix for both.
fn wants_new_openai_shape(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    b.contains("max_completion_tokens") || (b.contains("temperature") && b.contains("unsupported"))
}

/// Attach credentials the way each provider expects. Everyone takes `Authorization: Bearer`;
/// Anthropic's native endpoints (e.g. `/v1/models`) additionally want `x-api-key` + a version header.
pub fn apply_auth(req: reqwest::RequestBuilder, base_url: &str, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(k) if base_url.contains("api.anthropic.com") => req.bearer_auth(k).header("x-api-key", k).header("anthropic-version", "2023-06-01"),
        Some(k) => req.bearer_auth(k),
        None => req,
    }
}

// ---------------------------------------------------------------------------
// Probing a server: `GET {base}/models`. Used by `fore doctor` and `fore model`.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ProbeError {
    /// TCP/DNS/TLS level: nothing answered.
    Unreachable(String),
    /// The server answered, but not 2xx (401 = bad key, 404 = no list endpoint, …).
    Http(u16),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self { ProbeError::Unreachable(e) => write!(f, "{e}"), ProbeError::Http(s) => write!(f, "HTTP {s}") }
    }
}

/// Blocking. Returns model ids (empty if the server answered in an unexpected format).
pub fn probe_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<String>, ProbeError> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| ProbeError::Unreachable(e.to_string()))?;
    let base = base_url.to_string();
    let key = api_key.map(str::to_string);
    let v: serde_json::Value = rt.block_on(async move {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(6)).build().map_err(|e| ProbeError::Unreachable(e.to_string()))?;
        let req = apply_auth(client.get(&url), &base, key.as_deref());
        let resp = req.send().await.map_err(|e| ProbeError::Unreachable(short_err(&error_chain(&e))))?;
        if !resp.status().is_success() { return Err(ProbeError::Http(resp.status().as_u16())); }
        Ok::<_, ProbeError>(resp.json().await.unwrap_or(serde_json::Value::Null))
    })?;
    // OpenAI/Ollama/LM Studio/Groq/Gemini: {"data":[{"id":..}]} · a few servers: {"models":[{"name"|"id":..}]}
    let from = |arr: Option<&Vec<serde_json::Value>>| -> Vec<String> {
        arr.map(|a| a.iter().filter_map(|m| m.get("id").or(m.get("name")).and_then(|i| i.as_str()).map(String::from)).collect()).unwrap_or_default()
    };
    let mut ids = from(v.get("data").and_then(|d| d.as_array()));
    if ids.is_empty() { ids = from(v.get("models").and_then(|d| d.as_array())); }
    Ok(ids)
}

/// Does the server's list contain our model? Ollama lists `name:tag` (we may have `name`),
/// Gemini lists `models/gemini-…`.
pub fn model_listed(models: &[String], model: &str) -> bool {
    models.iter().any(|m| m == model || m.starts_with(&format!("{model}:")) || m.ends_with(&format!("/{model}")))
}

pub fn short_err(e: &str) -> String {
    let low = e.to_ascii_lowercase();
    if low.contains("connection refused") { "connection refused".into() }
    else if low.contains("dns") || low.contains("resolve") { "DNS lookup failed".into() }
    else if low.contains("timed out") || low.contains("timeout") { "timed out".into() }
    else if low.contains("certificate") || low.contains("tls") { "TLS error".into() }
    else { e.chars().take(80).collect() }
}

// ---------------------------------------------------------------------------
// Prompts. Kept here, in one place, so they're easy to iterate on.
// ---------------------------------------------------------------------------

pub const SYSTEM_FIX: &str = "\
You are a shell expert embedded in a terminal. The user ran a command that failed.
Reply in EXACTLY this format, nothing else:

WHY: <one sentence: the actual cause>
FIX: <one shell command that fixes or correctly retries it; no prose, no backticks>

Rules: prefer the smallest change. Never invent flags. If a package is missing, use the
package manager evident from the context. If no single command can fix it, put the most
useful diagnostic command in FIX.";

pub const SYSTEM_NL: &str = "\
You translate natural-language requests into a single shell command for the user's shell.
Reply in EXACTLY this format, nothing else:

CMD: <the command; no prose, no backticks>
NOTE: <at most 12 words: anything non-obvious the user should know, or '-'>

Rules: prefer portable POSIX/GNU tools already visible in the context. Never use
destructive flags unless the request clearly asks for deletion. Quote paths.";

/// reqwest's Display is "error sending request for url (…)"; the useful part
/// ("Connection refused", "dns error") is buried in the source chain. Flatten it.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        parts.push(s.to_string());
        cur = s.source();
    }
    parts.dedup();
    parts.join(": ")
}
