// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Client configs (`polis mcp install`) and the capture hook (`polis hook`).
//! Merge, never overwrite: a JSON file keeps every other key it had; the
//! codex TOML gains one section only if it has none for `polis`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::home::{current_exe, home_dir};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Client {
    /// Claude Code, user scope: `~/.claude.json` → `mcpServers.polis`.
    Claude,
    /// Codex CLI: `~/.codex/config.toml` → `[mcp_servers.polis]`.
    Codex,
    /// Any client reading a project's `.mcp.json` (cwd).
    Project,
}

impl Client {
    pub fn parse(s: &str) -> Option<Client> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(Client::Claude),
            "codex" => Some(Client::Codex),
            "project" => Some(Client::Project),
            _ => None,
        }
    }

    pub fn default_path(self) -> Option<PathBuf> {
        match self {
            Client::Claude => home_dir().map(|h| h.join(".claude.json")),
            Client::Codex => home_dir().map(|h| h.join(".codex").join("config.toml")),
            Client::Project => std::env::current_dir().ok().map(|d| d.join(".mcp.json")),
        }
    }
}

/// The MCP server entry every JSON client understands.
pub fn stdio_entry(polis: &Path) -> Value {
    json!({ "command": polis.to_string_lossy(), "args": ["mcp"] })
}

/// Merge `mcpServers.polis` into a JSON config file. Returns what changed.
pub fn install_json(path: &Path, polis: &Path) -> Result<&'static str, String> {
    let mut root: Value = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?,
        _ => json!({}),
    };
    let obj = root.as_object_mut().ok_or_else(|| format!("{} root is not a JSON object", path.display()))?;
    let servers = obj.entry("mcpServers".to_string()).or_insert_with(|| json!({}));
    let servers = servers.as_object_mut().ok_or_else(|| "mcpServers is not an object".to_string())?;
    let entry = stdio_entry(polis);
    let outcome = match servers.get("polis") {
        Some(existing) if existing == &entry => "unchanged",
        Some(_) => "updated",
        None => "added",
    };
    servers.insert("polis".into(), entry);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?)).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(outcome)
}

/// Codex reads TOML; without a TOML crate the rule is conservative: add a
/// `[mcp_servers.polis]` section when there is none, leave the file alone
/// when there is one (and say so).
pub fn install_codex(path: &Path, polis: &Path) -> Result<&'static str, String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains("[mcp_servers.polis]") {
        return Ok("present (left as is)");
    }
    let section = format!(
        "\n[mcp_servers.polis]\ncommand = \"{}\"\nargs = [\"mcp\"]\n",
        polis.to_string_lossy().replace('\\', "\\\\")
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&section);
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok("added")
}

pub fn install(client: Client, path: Option<PathBuf>, polis: Option<PathBuf>) -> Result<(PathBuf, &'static str), String> {
    let path = path.or_else(|| client.default_path()).ok_or("cannot resolve the client's config path (no HOME)")?;
    let polis = polis.unwrap_or_else(current_exe);
    let outcome = match client {
        Client::Claude | Client::Project => install_json(&path, &polis)?,
        Client::Codex => install_codex(&path, &polis)?,
    };
    Ok((path, outcome))
}

/// Is `polis` configured in a JSON client file?
pub fn json_has_polis(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("mcpServers")?.get("polis").cloned())
        .is_some()
}

/// The Claude Code global settings file the capture hook lives in.
pub fn claude_settings_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join("settings.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_install_merges_and_reports_the_change() {
        let dir = std::env::temp_dir().join(format!("polis-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".claude.json");
        std::fs::write(&path, r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let polis = Path::new("/opt/polis");
        assert_eq!(install_json(&path, polis).unwrap(), "added");
        assert_eq!(install_json(&path, polis).unwrap(), "unchanged");
        assert_eq!(install_json(&path, Path::new("/usr/local/bin/polis")).unwrap(), "updated");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark", "foreign keys survive");
        assert_eq!(v["mcpServers"]["other"]["command"], "x", "foreign servers survive");
        assert_eq!(v["mcpServers"]["polis"]["args"][0], "mcp");
        assert!(json_has_polis(&path));
        let codex = dir.join("config.toml");
        std::fs::write(&codex, "model = \"x\"").unwrap();
        assert_eq!(install_codex(&codex, polis).unwrap(), "added");
        assert_eq!(install_codex(&codex, polis).unwrap(), "present (left as is)");
        let text = std::fs::read_to_string(&codex).unwrap();
        assert!(text.starts_with("model = \"x\"\n") && text.contains("[mcp_servers.polis]"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
