//! Secret redaction. Runs on EVERYTHING before it leaves the machine.
//!
//! Three layers, cheapest first:
//!   1. Known token formats   (AWS keys, GitHub PATs, OpenAI keys, JWTs, private keys…)
//!   2. Assignments           (`PASSWORD=…`, `--token …`, `Authorization: Bearer …`)
//!   3. Environment values    (any env var whose NAME looks secret and whose VALUE appears verbatim)
//!
//! Design rule: false positives are cheap (the model sees `<REDACTED>`), false negatives
//! are catastrophic. When in doubt, redact.
//!
//! No regex crate: hand-rolled matchers keep this dependency-free and — more importantly —
//! easy to audit line by line, which is exactly what you want from a security boundary.

pub const MASK: &str = "<REDACTED>";

/// Env var names (or name fragments) that mark a value as secret.
const SECRET_NAME_HINTS: &[&str] = &[
    "SECRET", "TOKEN", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "PRIVATE_KEY",
    "ACCESS_KEY", "AUTH", "CREDENTIAL", "CLIENT_SECRET", "DATABASE_URL", "DSN",
];

/// Words that, when followed by `=` / `:` / a space, introduce a secret value.
const ASSIGN_KEYS: &[&str] = &[
    "password", "passwd", "pwd", "secret", "token", "api_key", "apikey", "api-key",
    "access_key", "private_key", "client_secret", "auth", "authorization", "bearer",
];

pub struct Redactor {
    /// Concrete env values to scrub. Built once at daemon start, refreshed on demand.
    env_values: Vec<String>,
}

impl Redactor {
    pub fn from_env() -> Self {
        let mut vals: Vec<String> = std::env::vars()
            .filter(|(k, v)| looks_secret_name(k) && v.len() >= 8)
            .map(|(_, v)| v)
            .collect();
        // Longest first so a value that contains another is masked whole.
        vals.sort_by_key(|v| std::cmp::Reverse(v.len()));
        vals.dedup();
        Self { env_values: vals }
    }

    #[cfg(test)]
    pub fn with_values(vals: Vec<String>) -> Self {
        Self { env_values: vals }
    }

    /// The one public entry point. Idempotent.
    pub fn redact(&self, input: &str) -> String {
        let mut s = input.to_string();
        // Layer 3 first: exact env values are the highest-confidence match.
        for v in &self.env_values {
            if s.contains(v.as_str()) {
                s = s.replace(v.as_str(), MASK);
            }
        }
        s = redact_known_formats(&s);
        s = redact_assignments(&s);
        s
    }

    /// Redact and seal. This is the ONLY way to obtain a `Redacted`, so anything
    /// that reaches the LLM client provably went through redaction.
    pub fn wrap(&self, input: &str) -> crate::llm::Redacted {
        crate::llm::Redacted(self.redact(input))
    }
}

fn looks_secret_name(name: &str) -> bool {
    let up = name.to_ascii_uppercase();
    SECRET_NAME_HINTS.iter().any(|h| up.contains(h))
}

// ---------------------------------------------------------------------------
// Layer 1: known token formats
// ---------------------------------------------------------------------------

/// Token = maximal run of [A-Za-z0-9_\-./+:@] characters.
/// `=` deliberately splits tokens so `KEY=value` is seen as two, and `@` is kept
/// so `scheme://user:pass@host` stays whole.
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '+' | ':' | '@')
}

fn redact_known_formats(s: &str) -> String {
    // PEM private keys: mask the whole block.
    if s.contains("-----BEGIN") && s.contains("PRIVATE KEY-----")
        && let (Some(a), Some(b)) = (s.find("-----BEGIN"), s.rfind("PRIVATE KEY-----")) {
            let end = b + "PRIVATE KEY-----".len();
            if a < end {
                return format!("{}{}{}", &s[..a], MASK, &s[end..]);
            }
        }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        // find start of next token
        let start = match rest.find(is_token_char) {
            Some(i) => i,
            None => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !is_token_char(c)).unwrap_or(rest.len());
        let tok = &rest[..end];
        if is_known_secret_token(tok) {
            out.push_str(MASK);
        } else {
            out.push_str(tok);
        }
        rest = &rest[end..];
    }
    out
}

fn is_known_secret_token(t: &str) -> bool {
    let len = t.len();
    let alnum_tail = |prefix: &str| {
        t.starts_with(prefix)
            && t[prefix.len()..].chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };

    // AWS access key id: AKIA + 16 uppercase alnum
    if len == 20 && (t.starts_with("AKIA") || t.starts_with("ASIA"))
        && t.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return true;
    }
    // GitHub tokens
    if len >= 36 && (alnum_tail("ghp_") || alnum_tail("gho_") || alnum_tail("ghu_")
        || alnum_tail("ghs_") || alnum_tail("ghr_") || alnum_tail("github_pat_")) {
        return true;
    }
    // OpenAI / Anthropic / Stripe / Slack / Google
    if len >= 20 && (alnum_tail("sk-") || alnum_tail("sk_live_") || alnum_tail("sk_test_")
        || alnum_tail("rk_live_") || alnum_tail("xoxb-") || alnum_tail("xoxp-")
        || alnum_tail("xoxa-") || alnum_tail("AIza")) {
        return true;
    }
    // JWT: three base64url segments, first starts with eyJ
    if t.starts_with("eyJ") && t.matches('.').count() == 2 && len > 40 {
        return true;
    }
    // URL with embedded credentials: scheme://user:pass@host
    if let Some(i) = t.find("://") {
        let after = &t[i + 3..];
        if let Some(at) = after.find('@')
            && after[..at].contains(':') {
                return true;
            }
    }
    // Generic high-entropy blob: long, mixed classes, no dots (avoid paths/URLs/versions).
    if len >= 32 && !t.contains('.') && !t.contains('/') && shannon_bits(t) > 4.2 {
        return true;
    }
    false
}

/// Shannon entropy in bits per character.
fn shannon_bits(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    let bytes = s.as_bytes();
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts.iter().filter(|&&c| c > 0).map(|&c| {
        let p = c as f64 / n;
        -p * p.log2()
    }).sum()
}

// ---------------------------------------------------------------------------
// Layer 2: `key=value`, `--key value`, `Key: value`
// ---------------------------------------------------------------------------

fn redact_assignments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (li, line) in s.split('\n').enumerate() {
        if li > 0 {
            out.push('\n');
        }
        out.push_str(&redact_assignments_line(line));
    }
    out
}

fn redact_assignments_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    let mut cuts: Vec<(usize, usize)> = Vec::new(); // byte ranges to mask

    for key in ASSIGN_KEYS {
        let mut from = 0;
        while let Some(pos) = lower[from..].find(key) {
            let kstart = from + pos;
            let kend = kstart + key.len();
            from = kend;

            // Must be a whole word-ish: preceded by start/space/-/_/quote.
            let prev = line[..kstart].chars().next_back();
            let word_start = prev.is_none_or(|c| !c.is_ascii_alphanumeric());
            if !word_start {
                continue;
            }
            // Followed by optional spaces, then `=` or `:` or a space, then the value.
            let after = &line[kend..];
            let trimmed = after.trim_start();
            let sep_len = after.len() - trimmed.len();
            let (vstart_rel, ok) = if trimmed.starts_with('=') || trimmed.starts_with(':') {
                (sep_len + 1, true)
            } else if sep_len > 0 && (lower[kstart..kend] == **key) {
                // `--token abc` / `Bearer abc` style: only for flag-like or header-like keys
                (sep_len, key == &"bearer" || line[..kstart].ends_with("--") || line[..kstart].ends_with('-'))
            } else {
                (0, false)
            };
            if !ok {
                continue;
            }
            let vstart = kend + vstart_rel;
            let vslice = &line[vstart..];
            let vslice = vslice.trim_start();
            let vstart = vstart + (line[kend + vstart_rel..].len() - vslice.len());
            if vslice.is_empty() {
                continue;
            }
            // Value ends at whitespace, or at the closing quote if it opened with one.
            let (skip, term): (usize, Box<dyn Fn(char) -> bool>) = match vslice.chars().next() {
                Some(q @ ('"' | '\'')) => (1, Box::new(move |c| c == q)),
                _ => (0, Box::new(|c: char| c.is_whitespace() || matches!(c, '&' | ';' | '|' | '"' | '\''))),
            };
            let mut body = &vslice[skip..];
            let mut extra = 0;
            // `Authorization: Bearer <tok>` — keep the scheme word, mask the token.
            for scheme in ["Bearer ", "bearer ", "Basic ", "Token ", "token "] {
                if let Some(rest) = body.strip_prefix(scheme) {
                    let rest_trim = rest.trim_start();
                    extra = body.len() - rest_trim.len();
                    body = rest_trim;
                    break;
                }
            }
            let vstart = vstart + extra;
            let vlen = body.find(term).unwrap_or(body.len());
            if vlen == 0 {
                continue;
            }
            cuts.push((vstart + skip, vstart + skip + vlen));
        }
    }

    if cuts.is_empty() {
        return line.to_string();
    }
    cuts.sort();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    for (a, b) in cuts {
        if a < i {
            continue; // overlapping
        }
        out.push_str(&line[i..a]);
        out.push_str(MASK);
        i = b;
    }
    out.push_str(&line[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        Redactor::with_values(vec!["hunter2hunter2".into()]).redact(s)
    }

    #[test]
    fn aws_and_github() {
        assert_eq!(r("key AKIAIOSFODNN7EXAMPLE here"), "key <REDACTED> here");
        assert_eq!(r("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "<REDACTED>");
    }

    #[test]
    fn openai_key_and_jwt() {
        assert_eq!(r("export OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz123456"), "export OPENAI_API_KEY=<REDACTED>");
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        assert_eq!(r(&format!("Authorization: Bearer {jwt}")), "Authorization: Bearer <REDACTED>");
    }

    #[test]
    fn assignments_and_flags() {
        assert_eq!(r("mysql -u root --password=abc123 db"), "mysql -u root --password=<REDACTED> db");
        assert_eq!(r("curl -H 'X-Token: 12345' --token abc"), "curl -H 'X-Token: <REDACTED>' --token <REDACTED>");
        assert_eq!(r("PASSWORD=\"my pass\" ./run"), "PASSWORD=\"<REDACTED>\" ./run");
    }

    #[test]
    fn url_credentials_and_env_values() {
        assert_eq!(r("psql postgres://admin:s3cret@db.internal:5432/app"), "psql <REDACTED>");
        assert_eq!(r("echo hunter2hunter2 | login"), "echo <REDACTED> | login");
    }

    #[test]
    fn leaves_normal_commands_alone() {
        for s in ["cargo build --release", "git commit -m 'fix auth bug'", "ls -la /usr/local/bin",
                  "docker run -p 8080:8080 nginx:1.25.3", "grep -rn 'token' src/"] {
            assert_eq!(r(s), s, "should not touch: {s}");
        }
    }

    #[test]
    fn pem_block() {
        let s = "cat key\n-----BEGIN RSA PRIVATE KEY-----\nMIIE...\n-----END RSA PRIVATE KEY-----\ndone";
        assert_eq!(r(s), "cat key\n<REDACTED>\ndone");
    }
}
