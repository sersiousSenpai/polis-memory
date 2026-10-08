// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Assistant turns from a Claude Code transcript (the `Stop` hook's
//! `transcript_path`, JSON lines).
//!
//! The capture hook records what the user typed; this is the other half of
//! the conversation. A turn is the assistant's text from one user message to
//! the next, with a one-line note of the files it edited. Tool inputs and
//! results stay out: they are bulky, mostly machine text, and the place a
//! pasted secret or a whole file would otherwise ride in. Subagent
//! (sidechain) traffic is skipped.

/// One assistant turn, ready to ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub body: String,
    /// Source time in unix milliseconds (the turn's last entry), when the
    /// transcript carried a parseable timestamp.
    pub ts: Option<i64>,
}

/// A turn longer than this is cut (with an ellipsis): the record keeps what
/// was said, not a dump.
pub const MAX_TURN_CHARS: usize = 16_000;

/// The tools whose `file_path` / `notebook_path` input names an edit.
const EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];

/// The length of the prefix made of complete lines: a transcript being
/// written may end mid-line, and that tail is read next time.
pub fn complete_prefix(bytes: &[u8]) -> usize {
    bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)
}

/// `2026-10-07T05:29:01.123Z` (or with an offset) → unix milliseconds.
pub fn parse_timestamp_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, day): (i64, i64, i64) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (time, offset_ms) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':').unwrap_or((&off[1..], "0"));
        (t, sign * (oh.parse::<i64>().ok()? * 3_600_000 + om.parse::<i64>().ok()? * 60_000))
    } else {
        (rest, 0)
    };
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':');
    let (h, mi, sec): (i64, i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let ms = format!("{frac:0<3}").get(..3)?.parse::<i64>().ok()?;
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3_600 + mi * 60 + sec) * 1000 + ms) - offset_ms)
}

#[derive(Default)]
struct Building {
    texts: Vec<String>,
    edited: Vec<String>,
    ts: Option<i64>,
}

impl Building {
    fn flush(&mut self, out: &mut Vec<Turn>) {
        let text = self.texts.join("\n\n").trim().to_string();
        if !text.is_empty() {
            let mut body = if text.chars().count() > MAX_TURN_CHARS {
                format!("{}…", text.chars().take(MAX_TURN_CHARS).collect::<String>())
            } else {
                text
            };
            if !self.edited.is_empty() {
                body.push_str(&format!("\n\n[edited: {}]", self.edited.join(", ")));
            }
            out.push(Turn { body, ts: self.ts });
        }
        *self = Building::default();
    }
}

/// The assistant turns in a run of transcript lines. File paths under `cwd`
/// are shown relative to it.
pub fn assistant_turns(jsonl: &str, cwd: Option<&str>) -> Vec<Turn> {
    let mut out = Vec::new();
    let mut turn = Building::default();
    for line in jsonl.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }
        let content = v.pointer("/message/content");
        match v.get("type").and_then(serde_json::Value::as_str) {
            Some("user") => {
                // A typed message starts the next turn; a tool result is the
                // same turn continuing.
                let typed = match content {
                    Some(serde_json::Value::String(_)) => true,
                    Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| b.get("type").and_then(|t| t.as_str()) == Some("text")),
                    _ => false,
                };
                if typed {
                    turn.flush(&mut out);
                }
            }
            Some("assistant") => {
                if let Some(ts) = v.get("timestamp").and_then(serde_json::Value::as_str).and_then(parse_timestamp_ms) {
                    turn.ts = Some(ts);
                }
                let Some(serde_json::Value::Array(blocks)) = content else { continue };
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()).filter(|t| !t.trim().is_empty()) {
                                turn.texts.push(text.trim().to_string());
                            }
                        }
                        Some("tool_use") if block.get("name").and_then(|n| n.as_str()).is_some_and(|n| EDIT_TOOLS.contains(&n)) => {
                            let input = block.get("input");
                            let path = input
                                .and_then(|i| i.get("file_path").or_else(|| i.get("notebook_path")))
                                .and_then(|p| p.as_str());
                            if let Some(path) = path {
                                let shown = cwd
                                    .and_then(|c| path.strip_prefix(c.trim_end_matches('/')))
                                    .map(|p| p.trim_start_matches('/'))
                                    .unwrap_or(path)
                                    .to_string();
                                if !turn.edited.contains(&shown) {
                                    turn.edited.push(shown);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    turn.flush(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: serde_json::Value) -> String {
        format!("{v}\n")
    }

    fn fixture() -> String {
        [
            line(serde_json::json!({"type":"user","message":{"role":"user","content":"which port should the daemon use?"},"timestamp":"2026-10-07T05:29:00.000Z"})),
            line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Use 7677; 7676 is taken."}]},"timestamp":"2026-10-07T05:29:01.500Z"})),
            line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/src/main.rs","old_string":"a","new_string":"b"}}]},"timestamp":"2026-10-07T05:29:02.000Z"})),
            line(serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}})),
            line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Updated the default."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]},"timestamp":"2026-10-07T05:29:03.000Z"})),
            line(serde_json::json!({"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent chatter"}]}})),
            line(serde_json::json!({"type":"user","message":{"role":"user","content":"thanks, now the docs"}})),
            line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Write","input":{"file_path":"/repo/README.md","content":"…"}}]}})),
            line(serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Docs updated."}]},"timestamp":"2026-10-07T05:30:00Z"})),
        ]
        .concat()
    }

    #[test]
    fn turns_group_text_and_edits_between_typed_messages() {
        let turns = assistant_turns(&fixture(), Some("/repo"));
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].body, "Use 7677; 7676 is taken.\n\nUpdated the default.\n\n[edited: src/main.rs]");
        assert_eq!(turns[0].ts, parse_timestamp_ms("2026-10-07T05:29:03.000Z"));
        assert_eq!(turns[1].body, "Docs updated.\n\n[edited: README.md]");
        assert!(!turns.iter().any(|t| t.body.contains("subagent") || t.body.contains("cargo test")));
    }

    #[test]
    fn only_complete_lines_are_consumed() {
        let all = fixture();
        let cut = &all.as_bytes()[..all.len() - 10];
        let n = complete_prefix(cut);
        assert!(n < cut.len() && cut[n - 1] == b'\n');
        // The cut line was the second turn's only text: what is complete is
        // the first turn, and an edit-only remainder records nothing.
        assert_eq!(assistant_turns(std::str::from_utf8(&cut[..n]).unwrap(), Some("/repo")).len(), 1);
        assert_eq!(complete_prefix(b"no newline"), 0);
    }

    #[test]
    fn timestamps_parse_to_unix_millis() {
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_timestamp_ms("2026-10-07T05:29:01.123Z"), Some(1_791_350_941_123));
        assert_eq!(parse_timestamp_ms("2026-10-07T07:29:01.123+02:00"), Some(1_791_350_941_123));
        assert_eq!(parse_timestamp_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(parse_timestamp_ms("yesterday"), None);
    }
}
