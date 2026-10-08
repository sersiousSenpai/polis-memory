// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Capture hooks for Codex and Cursor, beside Claude Code's (`polis_server::
//! hook`). The shapes are the ones Redline has run against both agents
//! (`codex_hook.rs`, `provider_hooks.rs`, its Cursor fixtures), minus
//! everything Redline-specific: no seat or launch headers, no plan review.
//!
//! | Agent | File | Events → command |
//! |---|---|---|
//! | Codex | `$CODEX_HOME/hooks.json` (`~/.codex`) | `UserPromptSubmit` → `capture --client codex` (async) · `Stop` → `capture --client codex --event stop` |
//! | Cursor | `~/.cursor/hooks.json` (version 1) | `beforeSubmitPrompt` → `capture --client cursor` · `afterAgentResponse` → `capture --client cursor --event response` |
//!
//! Every hook fails open and exits 0. Codex asks the user to review new
//! hooks (`/hooks` in Codex) before it runs them.

use std::path::{Path, PathBuf};

use clap::ValueEnum;
use serde_json::{json, Value};

use super::home::home_dir;

/// Which agent fired a capture hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum HookClient {
    Claude,
    Codex,
    Cursor,
}

impl HookClient {
    pub fn name(self) -> &'static str {
        match self {
            HookClient::Claude => "claude",
            HookClient::Codex => "codex",
            HookClient::Cursor => "cursor",
        }
    }
}

/// One agent's prompt payload in the shape the capture route reads
/// (`prompt`, `session_id`, `cwd`). Claude Code and Codex already send it;
/// Cursor names the session `conversation_id` and the project
/// `workspace_roots[0]`.
pub fn normalize_prompt(client: HookClient, payload: &Value) -> Value {
    match client {
        HookClient::Claude | HookClient::Codex => payload.clone(),
        HookClient::Cursor => json!({
            "prompt": payload.get("prompt").cloned().unwrap_or(Value::Null),
            "session_id": payload.get("conversation_id").or_else(|| payload.get("session_id")).cloned().unwrap_or(Value::Null),
            "cwd": payload.pointer("/workspace_roots/0").cloned().unwrap_or(Value::Null),
            "hook_event_name": "UserPromptSubmit",
        }),
    }
}

/// An agent's finished reply, when its payload carries one: `(session, cwd,
/// text)`. Codex's `Stop` carries `last_assistant_message`; Cursor's
/// `afterAgentResponse` carries `text`. (Claude Code's `Stop` names a
/// transcript instead — `capture_stop_payload`.)
pub fn reply_of(client: HookClient, payload: &Value) -> Option<(String, Option<String>, String)> {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let (session, cwd, text) = match client {
        HookClient::Codex => (s(payload.get("session_id")), s(payload.get("cwd")), s(payload.get("last_assistant_message"))),
        HookClient::Cursor => (
            s(payload.get("conversation_id")).or_else(|| s(payload.get("session_id"))),
            s(payload.pointer("/workspace_roots/0")),
            s(payload.get("text")),
        ),
        HookClient::Claude => return None,
    };
    Some((session?, cwd, text?))
}

pub fn codex_hooks_path() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| home_dir().map(|h| h.join(".codex"))).map(|d| d.join("hooks.json"))
}

pub fn cursor_hooks_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".cursor").join("hooks.json"))
}

/// `(event, command args, hook fields)` for an agent's hooks.
fn definitions(client: HookClient, polis: &Path) -> Vec<(&'static str, Value)> {
    let cmd = |args: &str| format!("\"{}\" capture --client {} {args}", polis.display(), client.name()).trim_end().to_string();
    match client {
        HookClient::Codex => vec![
            ("UserPromptSubmit", json!({ "type": "command", "command": cmd(""), "timeout": 5, "async": true })),
            ("Stop", json!({ "type": "command", "command": cmd("--event stop"), "timeout": 15 })),
        ],
        HookClient::Cursor => vec![
            ("beforeSubmitPrompt", json!({ "command": cmd(""), "timeout": 5 })),
            ("afterAgentResponse", json!({ "command": cmd("--event response"), "timeout": 15 })),
        ],
        HookClient::Claude => Vec::new(),
    }
}

/// A `polis … capture --client <client>` command, at any binary path.
pub fn command_is_ours(command: &str, client: HookClient) -> bool {
    command.contains("polis") && command.contains(&format!(" capture --client {}", client.name()))
}

fn handler_is_ours(handler: &Value, client: HookClient) -> bool {
    handler.get("command").and_then(Value::as_str).is_some_and(|c| command_is_ours(c, client))
}

fn read(path: &Path) -> Result<Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            let root: Value = serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
            if !root.is_object() {
                return Err(format!("{} root is not a JSON object", path.display()));
            }
            Ok(root)
        }
        _ => Ok(json!({})),
    }
}

fn write(path: &Path, root: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text = format!("{}\n", serde_json::to_string_pretty(root).map_err(|e| e.to_string())?);
    if std::fs::read_to_string(path).ok().as_deref() != Some(text.as_str()) {
        std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

/// Remove our handlers from one event's entries (Codex nests handlers under
/// `hooks`; Cursor's entries are the handlers). Drops entries left empty.
fn strip(entries: &mut Vec<Value>, client: HookClient) {
    match client {
        HookClient::Codex => {
            for entry in entries.iter_mut() {
                if let Some(handlers) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                    handlers.retain(|h| !handler_is_ours(h, client));
                }
            }
            entries.retain(|e| e.get("hooks").and_then(Value::as_array).is_none_or(|h| !h.is_empty()));
        }
        _ => entries.retain(|h| !handler_is_ours(h, client)),
    }
}

fn hooks_object(root: &mut Value, client: HookClient) -> Result<&mut serde_json::Map<String, Value>, String> {
    let obj = root.as_object_mut().ok_or("hooks file root is not a JSON object")?;
    if client == HookClient::Cursor {
        if obj.get("version").is_some_and(|v| v != &json!(1)) {
            return Err("Cursor's hooks.json uses a schema version other than 1".into());
        }
        obj.entry("version").or_insert(json!(1));
    }
    obj.entry("hooks").or_insert_with(|| json!({})).as_object_mut().ok_or_else(|| "hooks is not a JSON object".into())
}

/// Install (or refresh) an agent's capture hooks, keeping every other hook.
pub fn install_at(client: HookClient, path: &Path, polis: &Path) -> Result<(), String> {
    let mut root = read(path)?;
    let hooks = hooks_object(&mut root, client)?;
    for (event, handler) in definitions(client, polis) {
        let entries = hooks.entry(event).or_insert_with(|| json!([])).as_array_mut().ok_or_else(|| format!("hooks.{event} is not a JSON array"))?;
        strip(entries, client);
        entries.push(if client == HookClient::Codex { json!({ "hooks": [handler] }) } else { handler });
    }
    write(path, &root)
}

/// Remove an agent's capture hooks; containers left empty go too.
pub fn uninstall_at(client: HookClient, path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut root = read(path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else { return Ok(()) };
    for entries in hooks.values_mut().filter_map(Value::as_array_mut) {
        strip(entries, client);
    }
    hooks.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    write(path, &root)
}

/// Are this agent's capture hooks present, and are they the commands this
/// binary would write?
pub fn status_at(client: HookClient, path: &Path, polis: &Path) -> (bool, bool) {
    let Ok(root) = read(path) else { return (false, false) };
    let Some(hooks) = root.get("hooks") else { return (false, false) };
    let handlers: Vec<&Value> = hooks
        .as_object()
        .into_iter()
        .flat_map(|o| o.values())
        .filter_map(Value::as_array)
        .flatten()
        .flat_map(|e| match e.get("hooks").and_then(Value::as_array) {
            Some(nested) => nested.iter().collect::<Vec<_>>(),
            None => vec![e],
        })
        .collect();
    let installed = handlers.iter().any(|h| handler_is_ours(h, client));
    let current = definitions(client, polis).iter().all(|(_, want)| handlers.iter().any(|h| h.get("command") == want.get("command")));
    (installed, installed && current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("polis-agents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("hooks.json")
    }

    #[test]
    fn cursor_payloads_normalize_to_the_capture_shape() {
        // Redline's recorded Cursor fixtures (2026-09-08), trimmed.
        let prompt = json!({"conversation_id":"c1","generation_id":"g0","prompt":"use port 9090","session_id":"c1","hook_event_name":"beforeSubmitPrompt","workspace_roots":["/p"]});
        assert_eq!(normalize_prompt(HookClient::Cursor, &prompt), json!({"prompt":"use port 9090","session_id":"c1","cwd":"/p","hook_event_name":"UserPromptSubmit"}));
        let codex = json!({"prompt":"hi","session_id":"s","cwd":"/p"});
        assert_eq!(normalize_prompt(HookClient::Codex, &codex), codex);
        let response = json!({"conversation_id":"c1","text":"Done: 9090.","workspace_roots":["/p"],"hook_event_name":"afterAgentResponse"});
        assert_eq!(reply_of(HookClient::Cursor, &response), Some(("c1".into(), Some("/p".into()), "Done: 9090.".into())));
        let stop = json!({"session_id":"s","turn_id":"t","cwd":"/p","last_assistant_message":"Fixed it."});
        assert_eq!(reply_of(HookClient::Codex, &stop), Some(("s".into(), Some("/p".into()), "Fixed it.".into())));
        assert_eq!(reply_of(HookClient::Codex, &json!({"session_id":"s","last_assistant_message":"  "})), None);
        assert_eq!(reply_of(HookClient::Claude, &stop), None);
    }

    #[test]
    fn codex_hooks_install_beside_foreign_ones_refresh_and_uninstall() {
        let path = tmp("codex");
        std::fs::write(&path, r#"{"hooks":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"plannotator"}]}]}}"#).unwrap();
        let polis = Path::new("/opt/polis");
        install_at(HookClient::Codex, &path, polis).unwrap();
        assert_eq!(status_at(HookClient::Codex, &path, polis), (true, true));
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "\"/opt/polis\" capture --client codex");
        assert_eq!(root["hooks"]["UserPromptSubmit"][0]["hooks"][0]["async"], true);
        assert_eq!(root["hooks"]["Stop"][1]["hooks"][0]["command"], "\"/opt/polis\" capture --client codex --event stop");
        assert_eq!(root["hooks"]["Stop"][0]["hooks"][0]["command"], "plannotator");
        // A moved binary is still ours and is replaced, not duplicated.
        let moved = Path::new("/usr/local/bin/polis");
        assert_eq!(status_at(HookClient::Codex, &path, moved), (true, false));
        install_at(HookClient::Codex, &path, moved).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hooks"]["Stop"].as_array().unwrap().len(), 2);
        uninstall_at(HookClient::Codex, &path).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root, json!({"hooks":{"Stop":[{"matcher":"*","hooks":[{"type":"command","command":"plannotator"}]}]}}));
        assert_eq!(status_at(HookClient::Codex, &path, moved), (false, false));
    }

    #[test]
    fn cursor_hooks_keep_version_one_and_foreign_handlers() {
        let path = tmp("cursor");
        std::fs::write(&path, r#"{"version":1,"hooks":{"beforeSubmitPrompt":[{"command":"other","timeout":3}]}}"#).unwrap();
        let polis = Path::new("/opt/polis");
        install_at(HookClient::Cursor, &path, polis).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["version"], 1);
        assert_eq!(root["hooks"]["beforeSubmitPrompt"].as_array().unwrap().len(), 2);
        assert_eq!(root["hooks"]["afterAgentResponse"][0]["command"], "\"/opt/polis\" capture --client cursor --event response");
        install_at(HookClient::Cursor, &path, polis).unwrap();
        let again: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(again, root, "idempotent");
        assert!(!command_is_ours("\"/opt/polis\" capture --client codex", HookClient::Cursor));
        uninstall_at(HookClient::Cursor, &path).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root, json!({"version":1,"hooks":{"beforeSubmitPrompt":[{"command":"other","timeout":3}]}}));
        std::fs::write(&path, r#"{"version":2}"#).unwrap();
        assert!(install_at(HookClient::Cursor, &path, polis).is_err(), "an unknown schema is not ours to rewrite");
    }
}
