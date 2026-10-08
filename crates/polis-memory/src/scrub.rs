// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Secret scrubbing at ingest.
//!
//! The ledger is append-only, hash-chained and syncs to peers, so a secret
//! that reaches it is expensive to take back: `forget` cleans up after the
//! fact, it does not guard. Every captured body (a typed prompt, an assistant
//! reply, an imported item, a remembered fact) passes through [`scrub`]
//! BEFORE it is hashed, so a matched credential never enters the chain, the
//! lexical index, the embeddings, a backup or a peer's copy.
//!
//! Pattern-based and deliberately conservative: provider key shapes, private
//! key blocks, JWTs, bearer headers, and `KEY|SECRET|TOKEN|PASSWORD`-style
//! assignments with a long value. Each match becomes `[redacted:<kind>]`; the
//! counts by kind are kept, never the value. A hash, a UUID or prose about
//! tokens is left alone.

use std::sync::OnceLock;

use regex_lite::{Captures, Regex};

/// The text with every match replaced, and how many of each kind were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed {
    pub text: String,
    /// `(kind, count)` in pattern order, only kinds that matched.
    pub redactions: Vec<(&'static str, usize)>,
}

impl Scrubbed {
    pub fn total(&self) -> usize {
        self.redactions.iter().map(|(_, n)| n).sum()
    }
}

/// What a redaction leaves in the text.
pub fn marker(kind: &str) -> String {
    format!("[redacted:{kind}]")
}

/// How a pattern rewrites its match: the whole match, or only the value
/// group (keeping the key or header that names it, which is not secret and
/// is what makes the record still read sensibly).
enum Keep {
    Nothing,
    Prefix,
}

struct Pattern {
    kind: &'static str,
    re: Regex,
    keep: Keep,
}

fn patterns() -> &'static [Pattern] {
    static PATTERNS: OnceLock<Vec<Pattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let p = |kind, re: &str, keep| Pattern { kind, re: Regex::new(re).expect("scrub pattern compiles"), keep };
        vec![
            // Most specific first: a later, broader pattern never sees what an
            // earlier one already replaced.
            p("private_key", r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----", Keep::Nothing),
            p("anthropic_key", r"\bsk-ant-[A-Za-z0-9_\-]{20,}", Keep::Nothing),
            p("openai_key", r"\bsk-(?:proj-)?[A-Za-z0-9_\-]{20,}", Keep::Nothing),
            p("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", Keep::Nothing),
            p("github_token", r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})", Keep::Nothing),
            p("slack_token", r"\bxox[abprs]-[A-Za-z0-9\-]{10,}", Keep::Nothing),
            p("jwt", r"\beyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}", Keep::Nothing),
            p("bearer", r"(?i)(\bbearer\s+)([A-Za-z0-9._~+/=\-]{16,})", Keep::Prefix),
            p(
                "assigned_secret",
                r#"(?i)(\b[A-Z0-9_\-]*(?:API_?KEY|SECRET|TOKEN|PASSWORD|PASSWD)[A-Z0-9_\-]*["']?\s*[:=]\s*["']?)([^\s"'`,;]{16,})"#,
                Keep::Prefix,
            ),
        ]
    })
}

/// Scrub one body. Returns the input unchanged (and no redactions) when
/// nothing matched.
pub fn scrub(text: &str) -> Scrubbed {
    let mut out = text.to_string();
    let mut redactions = Vec::new();
    for pattern in patterns() {
        let mut n = 0usize;
        let next = pattern.re.replace_all(&out, |c: &Captures<'_>| match pattern.keep {
            Keep::Nothing => {
                n += 1;
                marker(pattern.kind)
            }
            Keep::Prefix => {
                let value = c.get(2).map_or("", |m| m.as_str());
                // An earlier pattern's marker is not a secret to redact again.
                if value.starts_with("[redacted:") {
                    return c[0].to_string();
                }
                n += 1;
                format!("{}{}", &c[1], marker(pattern.kind))
            }
        });
        if n > 0 {
            out = next.into_owned();
            redactions.push((pattern.kind, n));
        }
    }
    Scrubbed { text: out, redactions }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &Scrubbed) -> Vec<&'static str> {
        s.redactions.iter().map(|(k, _)| *k).collect()
    }

    /// One fixture per kind: the value is gone, the marker names the kind,
    /// and the words around it survive.
    #[test]
    fn every_kind_is_redacted_and_its_value_is_gone() {
        let cases: &[(&str, &str, &str)] = &[
            ("anthropic_key", "export ANTHROPIC = sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz012345", "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz012345"),
            ("openai_key", "the key sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123 works", "sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123"),
            ("aws_access_key", "aws id AKIAIOSFODNN7EXAMPLE in the config", "AKIAIOSFODNN7EXAMPLE"),
            ("github_token", "push with ghp_abcdefghijklmnopqrstuvwxyz0123456789AB now", "ghp_abcdefghijklmnopqrstuvwxyz0123456789AB"),
            ("github_token", "fine-grained github_pat_11ABCDEFG0123456789_abcdefghijklmnop", "github_pat_11ABCDEFG0123456789_abcdefghijklmnop"),
            ("slack_token", "bot xoxb-123456789012-abcdefghijkl posted", "xoxb-123456789012-abcdefghijkl"),
            ("jwt", "cookie eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U set", "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"),
            ("bearer", "curl -H 'Authorization: Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0' http://x", "9f8e7d6c5b4a39281706f5e4d3c2b1a0"),
            ("assigned_secret", "DATABASE_PASSWORD=hunter2hunter2hunter2 in .env", "hunter2hunter2hunter2"),
            ("assigned_secret", r#"{"client_secret": "Zm9vYmFyYmF6cXV4cXV1eA"}"#, "Zm9vYmFyYmF6cXV4cXV1eA"),
            ("private_key", "key:\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\ndone", "b3BlbnNzaC1rZXktdjEAAAAA"),
        ];
        for (kind, text, secret) in cases {
            let s = scrub(text);
            assert!(!s.text.contains(secret), "{kind}: {}", s.text);
            assert!(s.text.contains(&marker(kind)), "{kind}: {}", s.text);
            assert_eq!(kinds(&s), vec![*kind], "{text}");
        }
        let kept = scrub("DATABASE_PASSWORD=hunter2hunter2hunter2 in .env");
        assert_eq!(kept.text, "DATABASE_PASSWORD=[redacted:assigned_secret] in .env");
        let bearer = scrub("Authorization: Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0");
        assert_eq!(bearer.text, "Authorization: Bearer [redacted:bearer]");
    }

    /// Hashes, UUIDs, base64 test data, short config values and ordinary
    /// prose about tokens are not secrets.
    #[test]
    fn ordinary_text_is_untouched() {
        for text in [
            "commit 4367ef1a9b2c3d4e5f60718293a4b5c6d7e8f901 fixed the chain",
            "sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "session fbf661e8-3152-4f0d-bc43-e1bc07008f5a resumed",
            "fixture body aGVsbG8gd29ybGQgdGhpcyBpcyBiYXNlNjQgdGVzdCBkYXRh",
            "max_tokens = 4096 and the token budget is 12,000 bytes",
            "the password field must be at least 12 characters",
            "We rotate the API key monthly; ask ops for the new token.",
            "use --frozen and port 9090 for the API",
            "skip the sk- prefix check for short ids like sk-12",
        ] {
            let s = scrub(text);
            assert_eq!(s.text, text, "{:?}", s.redactions);
            assert_eq!(s.total(), 0);
        }
    }

    /// A specific match inside an assignment is redacted once, under its own
    /// kind, not re-redacted by the broader assignment rule.
    #[test]
    fn a_marker_is_not_redacted_twice() {
        let s = scrub("OPENAI_API_KEY=sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123");
        assert_eq!(s.text, "OPENAI_API_KEY=[redacted:openai_key]");
        assert_eq!(s.redactions, vec![("openai_key", 1)]);
    }
}
