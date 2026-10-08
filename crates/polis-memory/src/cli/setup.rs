// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis setup` and `polis uninstall`: the whole install in one command, and
//! its exact reverse.
//!
//! Setup finds the agents on this machine and, after showing its plan and
//! asking:
//! - creates the home and its store, the same way `polis init` does;
//! - connects each agent's MCP config;
//! - installs capture hooks (Claude Code, Codex, Cursor) and the
//!   `polis-memory` skill for Claude Code;
//! - fetches the local embedding model;
//! - records an explicit gardener model (a model CLI the user already has,
//!   never an API key);
//! - keeps the daemon running as a per-user service.
//!
//! Every step is idempotent, so re-running setup refreshes the install.
//! Uninstall removes what setup added and keeps the record unless `--purge`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use polis_server::hook::StopHookSpec;
use serde::Serialize;

use super::agents::{self, HookClient};
use super::backend;
use super::home::{current_exe, home_dir, on_path, Home};
use super::install::{self, claude_settings_path, Client};
use super::{doctor, service};

/// An agent setup knows how to connect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeCode,
    Codex,
    Cursor,
    Windsurf,
    ClaudeDesktop,
}

impl Agent {
    pub const ALL: [Agent; 5] = [Agent::ClaudeCode, Agent::Codex, Agent::Cursor, Agent::Windsurf, Agent::ClaudeDesktop];

    pub fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude",
            Agent::Codex => "codex",
            Agent::Cursor => "cursor",
            Agent::Windsurf => "windsurf",
            Agent::ClaudeDesktop => "claude-desktop",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::Cursor => "Cursor",
            Agent::Windsurf => "Windsurf",
            Agent::ClaudeDesktop => "Claude Desktop",
        }
    }

    pub fn parse(s: &str) -> Option<Agent> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(Agent::ClaudeCode),
            "codex" => Some(Agent::Codex),
            "cursor" => Some(Agent::Cursor),
            "windsurf" => Some(Agent::Windsurf),
            "claude-desktop" | "desktop" => Some(Agent::ClaudeDesktop),
            _ => None,
        }
    }

    fn mcp_client(self) -> Client {
        match self {
            Agent::ClaudeCode => Client::Claude,
            Agent::Codex => Client::Codex,
            Agent::Cursor => Client::Cursor,
            Agent::Windsurf => Client::Windsurf,
            Agent::ClaudeDesktop => Client::ClaudeDesktop,
        }
    }

    /// The agent's capture hooks, when it has any to install.
    fn hooks(self) -> Option<HookClient> {
        match self {
            Agent::ClaudeCode => Some(HookClient::Claude),
            Agent::Codex => Some(HookClient::Codex),
            Agent::Cursor => Some(HookClient::Cursor),
            Agent::Windsurf | Agent::ClaudeDesktop => None,
        }
    }

    /// What setup gives this agent, in a phrase.
    fn offer(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "search tools + capture prompts and replies + skill",
            Agent::Codex => "search tools + capture prompts and replies",
            Agent::Cursor => "search tools + capture prompts and replies",
            Agent::Windsurf | Agent::ClaudeDesktop => "search tools (chats are not captured)",
        }
    }

    /// Is the agent installed for this user? A config directory or the CLI on
    /// PATH is the evidence.
    pub fn detected(self, user_home: &Path) -> bool {
        match self {
            Agent::ClaudeCode => on_path("claude").is_some() || user_home.join(".claude").is_dir(),
            Agent::Codex => on_path("codex").is_some() || agents::codex_hooks_path().and_then(|p| p.parent().map(Path::is_dir)).unwrap_or(false),
            Agent::Cursor => user_home.join(".cursor").is_dir(),
            Agent::Windsurf => user_home.join(".codeium").join("windsurf").is_dir(),
            Agent::ClaudeDesktop => install::claude_desktop_dir().is_some_and(|d| d.is_dir()),
        }
    }
}

/// Is this binary somewhere a package manager may delete or replace — the
/// npx cache, or inside any `node_modules`? Hooks, MCP configs and the
/// service must never point there.
pub fn is_transient(binary: &Path) -> bool {
    binary.components().any(|c| matches!(c.as_os_str().to_str(), Some("node_modules" | "_npx")))
}

/// Where setup keeps its own copy of the binary: `~/.local/bin/polis`
/// (unix) or `%LOCALAPPDATA%\Programs\polis\polis.exe` (Windows).
pub fn stable_binary_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Programs").join("polis").join("polis.exe"))
    } else {
        home_dir().map(|h| h.join(".local").join("bin").join("polis"))
    }
}

/// The binary setup wires everything to: `polis` itself, unless it runs from
/// a package manager's directory — then a copy at [`stable_binary_path`]
/// (refreshed on every setup, so `npx polis-memory@latest setup` upgrades).
pub fn settle_binary(polis: PathBuf) -> Result<(PathBuf, Option<String>), String> {
    if !is_transient(&polis) {
        return Ok((polis, None));
    }
    let dest = stable_binary_path().ok_or("no place for a stable copy of polis (HOME / LOCALAPPDATA unset)")?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // Copy beside, then rename over: a running daemon keeps its old inode.
    let staging = dest.with_extension("new");
    std::fs::copy(&polis, &staging).map_err(|e| format!("copy {} → {}: {e}", polis.display(), staging.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&staging, &dest).map_err(|e| format!("install {}: {e}", dest.display()))?;
    let on_path = dest.parent().is_some_and(|dir| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d == dir)));
    let note = format!(
        "installed polis at {}{}",
        dest.display(),
        if on_path { String::new() } else { format!(" (add {} to your PATH to run `polis` directly)", dest.parent().map(|d| d.display().to_string()).unwrap_or_default()) }
    );
    Ok((dest, Some(note)))
}

/// Where `polis-memory`'s skill goes for Claude Code.
pub fn claude_skill_path() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join("skills").join("polis-memory").join("SKILL.md"))
}

/// Write the rendered `polis-memory` skill. Returns whether it changed.
pub fn install_skill(home: &Home, path: &Path) -> Result<bool, String> {
    let text = crate::skill::render_polis_memory_skill(&home.listen(), &home.token_path().to_string_lossy());
    if std::fs::read_to_string(path).ok().as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(true)
}

/// Remove the skill file (and its directory, when that leaves it empty).
pub fn uninstall_skill(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path).map_err(|e| format!("remove {}: {e}", path.display()))?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(true)
}

/// The gardener model setup records: a CLI the user already has (billed to
/// their own subscription), else none.
pub fn model_choice() -> (&'static str, String) {
    if let Some(bin) = backend::find_on_path("claude") {
        ("claude-cli", format!("claude CLI ({}) — your Claude subscription; no API key used", bin.display()))
    } else if let Some(bin) = backend::find_on_path("codex") {
        ("codex-cli", format!("codex CLI ({}) — your Codex subscription; no API key used", bin.display()))
    } else {
        ("none", "none found — the catalog files by rules only; install the claude or codex CLI and re-run setup".to_string())
    }
}

#[derive(Debug, Default)]
pub struct SetupOptions {
    pub yes: bool,
    pub dry_run: bool,
    /// Only these agents (names as `Agent::parse` reads them).
    pub clients: Option<Vec<String>>,
    pub no_service: bool,
    pub no_model: bool,
    pub no_skill: bool,
    pub polis: Option<PathBuf>,
}

/// What setup did, step by step (`--json` prints this).
#[derive(Debug, Default, Serialize)]
pub struct SetupReport {
    pub home: PathBuf,
    pub agents: Vec<Agent>,
    pub done: Vec<String>,
    pub skipped: Vec<String>,
    pub problems: Vec<String>,
    pub applied: bool,
}

/// Ask a yes/no question on the controlling terminal — which works under
/// `curl … | sh`, where stdin is the script. No terminal: no.
pub fn confirm(question: &str) -> bool {
    #[cfg(unix)]
    if let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") {
        let mut writer = &tty;
        let _ = write!(writer, "{question} [Y/n] ");
        let _ = writer.flush();
        let mut answer = String::new();
        let _ = std::io::BufReader::new(&tty).read_line(&mut answer);
        return matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes");
    }
    if std::io::stdin().is_terminal() {
        eprint!("{question} [Y/n] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().lock().read_line(&mut answer);
        return matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes");
    }
    false
}

/// Create the home, store, token, config and identity — `polis init`'s
/// plain path (no org, no Redline adoption). Returns what it created.
pub fn ensure_initialized(home: &Home) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    if !home.exists() {
        done.push(format!("created {}", home.root.display()));
    }
    home.ensure()?;
    if home.ensure_config()? {
        done.push(format!("wrote {}", home.config_path().display()));
    }
    let db = home.db_path();
    let created_db = !db.exists();
    let store = polis_store::PolisStore::open(&db).map_err(|e| format!("create {}: {e}", db.display()))?;
    if created_db {
        done.push(format!("created the store {}", db.display()));
    }
    home.ensure_token()?;
    let (identity, created_key) = crate::identity::Identity::load_or_create(&home.identity_dir(), home.device_name())?;
    if created_key {
        done.push(format!("created this device's key ({})", identity.fingerprint()));
    }
    crate::identity::adopt(&store, &identity, &crate::identity::login_name())?;
    let _ = std::fs::create_dir_all(home.models_dir());
    Ok(done)
}

fn chosen_agents(opts: &SetupOptions, user_home: &Path) -> Result<Vec<Agent>, String> {
    match &opts.clients {
        Some(names) => names
            .iter()
            .flat_map(|n| n.split(','))
            .filter(|n| !n.trim().is_empty())
            .map(|n| Agent::parse(n).ok_or_else(|| format!("unknown agent `{n}` (claude | codex | cursor | windsurf | claude-desktop)")))
            .collect(),
        None => Ok(Agent::ALL.into_iter().filter(|a| a.detected(user_home)).collect()),
    }
}

/// The plan, as the lines the prompt shows.
fn plan_lines(home: &Home, agents: &[Agent], opts: &SetupOptions, model: &str, daemon_up: bool) -> Vec<String> {
    let mut lines = vec![format!("  ✓ the record at {}{}", home.root.display(), if home.exists() { " (present; kept)" } else { "" })];
    if agents.is_empty() {
        lines.push("  · no supported agent found (claude, codex, cursor, windsurf, claude-desktop) — `--clients` names one".into());
    }
    for a in agents {
        lines.push(format!("  ✓ {:<14} {}", a.label(), a.offer()));
    }
    lines.push("  ✓ the local embedding model (semantic search; downloaded once)".into());
    lines.push(format!("  ✓ organizer model: {}", if opts.no_model { "none (--no-model)" } else { model }));
    let service = if opts.no_service {
        "no (--no-service) — run `polis serve` yourself".to_string()
    } else if daemon_up && service::installed().is_none() {
        "no — a daemon is already running; stop it, then `polis service install`".to_string()
    } else {
        match service::Manager::current() {
            Ok(service::Manager::Launchd) => "yes, at login (launchd)".into(),
            Ok(service::Manager::Systemd) => "yes, at login (systemd user unit)".into(),
            Err(_) => "no service manager supported here — run `polis serve` yourself".into(),
        }
    };
    lines.push(format!("  {} keep the daemon running: {service}", if service.starts_with("yes") { "✓" } else { "·" }));
    lines
}

pub fn run(home: &Home, opts: &SetupOptions) -> Result<SetupReport, String> {
    let user_home = home_dir().ok_or("HOME is not set")?;
    let agents = chosen_agents(opts, &user_home)?;
    let (model_key, model_label) = model_choice();
    let daemon_up = backend::daemon_alive(home).is_some();
    let mut report = SetupReport { home: home.root.clone(), agents: agents.clone(), ..Default::default() };

    eprintln!("Polis will set up:\n{}", plan_lines(home, &agents, opts, &model_label, daemon_up).join("\n"));
    if opts.dry_run {
        eprintln!("\n(dry run — nothing changed)");
        return Ok(report);
    }
    if !opts.yes && !confirm("\nProceed?") {
        return Err("setup cancelled — nothing changed (`polis setup --yes` skips the question)".into());
    }
    report.applied = true;
    let (polis, installed) = settle_binary(opts.polis.clone().unwrap_or_else(current_exe))?;
    report.done.extend(installed);

    report.done.extend(ensure_initialized(home)?);
    let semantic = backend::ensure_default_model(home);
    if semantic.ends_with("ready") {
        report.done.push(format!("semantic search: {semantic}"));
    } else {
        report.skipped.push(format!("semantic search: {semantic}"));
    }

    for agent in &agents {
        match install::install(agent.mcp_client(), None, Some(polis.clone())) {
            Ok((path, outcome)) => report.done.push(format!("{}: search tools {outcome} ({})", agent.label(), path.display())),
            Err(e) => report.problems.push(format!("{}: search tools: {e}", agent.label())),
        }
        match agent.hooks() {
            Some(HookClient::Claude) => match claude_settings_path() {
                Some(settings) => {
                    let capture = doctor::spec_for(home, Some(polis.clone()));
                    let stop = StopHookSpec::new(polis.clone());
                    match capture.install_at(&settings).and_then(|_| stop.install_at(&settings)) {
                        Ok(_) => report.done.push(format!("Claude Code: capture hooks ({})", settings.display())),
                        Err(e) => report.problems.push(format!("Claude Code: capture hooks: {e}")),
                    }
                    if !opts.no_skill {
                        match claude_skill_path().map(|p| install_skill(home, &p).map(|_| p)) {
                            Some(Ok(p)) => report.done.push(format!("Claude Code: polis-memory skill ({})", p.display())),
                            Some(Err(e)) => report.problems.push(format!("Claude Code: skill: {e}")),
                            None => {}
                        }
                    }
                }
                None => report.problems.push("Claude Code: no settings path (HOME)".into()),
            },
            Some(client) => {
                let path = if client == HookClient::Codex { agents::codex_hooks_path() } else { agents::cursor_hooks_path() };
                match path.ok_or_else(|| "no hooks path".to_string()).and_then(|p| agents::install_at(client, &p, &polis).map(|_| p)) {
                    Ok(p) => report.done.push(format!("{}: capture hooks ({})", agent.label(), p.display())),
                    Err(e) => report.problems.push(format!("{}: capture hooks: {e}", agent.label())),
                }
            }
            None => {}
        }
    }

    let model = if opts.no_model { "none" } else { model_key };
    home.config_set("gardener_model", model)?;
    report.done.push(format!("organizer model: gardener_model = \"{model}\""));

    if opts.no_service {
        report.skipped.push("daemon service (--no-service)".into());
    } else if daemon_up && service::installed().is_none() {
        report.skipped.push("daemon service: a daemon is already running — stop it, then `polis service install`".into());
    } else if service::Manager::current().is_ok() {
        match service::install(home, Some(polis.clone()), false) {
            Ok(r) => report.done.push(format!("daemon: {} service started ({})", r.manager, r.definition.display())),
            Err(e) => report.problems.push(format!("daemon service: {e}")),
        }
    } else {
        report.skipped.push("daemon service: no supported service manager — run `polis serve`".into());
    }
    Ok(report)
}

pub fn render(report: &SetupReport) -> String {
    let mut out = String::new();
    for line in &report.done {
        out.push_str(&format!("  ✓ {line}\n"));
    }
    for line in &report.skipped {
        out.push_str(&format!("  · {line}\n"));
    }
    for line in &report.problems {
        out.push_str(&format!("  ! {line}\n"));
    }
    if !report.applied {
        return out;
    }
    let mut next = Vec::new();
    if !report.agents.is_empty() {
        next.push("restart your agents so they load the Polis tools".to_string());
    }
    if report.agents.contains(&Agent::Codex) {
        next.push("in Codex, open `/hooks` and enable Polis's two hooks (Codex asks before running new hooks)".into());
    }
    next.push("`polis doctor` checks the install; `polis uninstall` removes it (your record stays unless `--purge`)".into());
    out.push_str(&format!("\nnext:\n{}", next.iter().map(|n| format!("  - {n}\n")).collect::<String>()));
    out
}

#[derive(Debug, Default, Serialize)]
pub struct UninstallReport {
    pub removed: Vec<String>,
    pub problems: Vec<String>,
    pub purged: bool,
}

/// Remove what setup adds — hooks, MCP entries, the skill, the service — and,
/// with `purge`, the home with its record.
pub fn uninstall(home: &Home, purge: bool) -> UninstallReport {
    let mut r = UninstallReport::default();
    if let Some(settings) = claude_settings_path() {
        let capture = super::hook_spec(home);
        let stop = StopHookSpec::new(current_exe());
        match (capture.uninstall_at(&settings), stop.uninstall_at(&settings)) {
            (Ok(_), Ok(_)) => r.removed.push(format!("Claude Code capture hooks ({})", settings.display())),
            (Err(e), _) | (_, Err(e)) => r.problems.push(format!("Claude Code hooks: {e}")),
        }
    }
    for (client, path) in [(HookClient::Codex, agents::codex_hooks_path()), (HookClient::Cursor, agents::cursor_hooks_path())] {
        let Some(path) = path.filter(|p| p.exists()) else { continue };
        if agents::status_at(client, &path, &current_exe()).0 {
            match agents::uninstall_at(client, &path) {
                Ok(()) => r.removed.push(format!("{} capture hooks ({})", client.name(), path.display())),
                Err(e) => r.problems.push(format!("{} hooks: {e}", client.name())),
            }
        }
    }
    // Setup never writes a project's `.mcp.json`; uninstall leaves it alone.
    for client in Client::ALL.into_iter().filter(|c| *c != Client::Project) {
        match install::uninstall(client) {
            Ok((path, true)) => r.removed.push(format!("{} search tools ({})", client.name(), path.display())),
            Ok(_) => {}
            Err(e) => r.problems.push(format!("{}: {e}", client.name())),
        }
    }
    if let Some(path) = claude_skill_path() {
        match uninstall_skill(&path) {
            Ok(true) => r.removed.push(format!("polis-memory skill ({})", path.display())),
            Ok(false) => {}
            Err(e) => r.problems.push(e),
        }
    }
    if service::installed().is_some() {
        match service::uninstall() {
            Ok(s) => r.removed.push(format!("{} service ({})", s.manager, s.definition.display())),
            Err(e) => r.problems.push(format!("service: {e}")),
        }
    }
    if purge && home.root.exists() {
        match std::fs::remove_dir_all(&home.root) {
            Ok(()) => {
                r.removed.push(format!("the record and its backups ({})", home.root.display()));
                r.purged = true;
            }
            Err(e) => r.problems.push(format!("remove {}: {e}", home.root.display())),
        }
    }
    r
}

pub fn render_uninstall(r: &UninstallReport, home: &Home) -> String {
    let mut out = String::new();
    if r.removed.is_empty() {
        out.push_str("nothing of Polis's was installed\n");
    }
    for line in &r.removed {
        out.push_str(&format!("  ✓ removed {line}\n"));
    }
    for line in &r.problems {
        out.push_str(&format!("  ! {line}\n"));
    }
    if !r.purged && home.root.exists() {
        out.push_str(&format!("\nyour record is kept at {} — `polis uninstall --purge` deletes it\n", home.root.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_round_trip_and_reject_strangers() {
        for a in Agent::ALL {
            assert_eq!(Agent::parse(a.name()), Some(a));
        }
        assert_eq!(Agent::parse("Claude-Code"), Some(Agent::ClaudeCode));
        assert_eq!(Agent::parse("vim"), None);
        let opts = SetupOptions { clients: Some(vec!["codex,cursor".into()]), ..Default::default() };
        assert_eq!(chosen_agents(&opts, Path::new("/nowhere")).unwrap(), vec![Agent::Codex, Agent::Cursor]);
        let bad = SetupOptions { clients: Some(vec!["emacs".into()]), ..Default::default() };
        assert!(chosen_agents(&bad, Path::new("/nowhere")).is_err());
    }

    #[test]
    fn a_package_manager_path_is_transient() {
        assert!(is_transient(Path::new("/Users/a/.npm/_npx/3f2a/node_modules/@polis-memory/darwin-arm64/bin/polis")));
        assert!(is_transient(Path::new("/usr/local/lib/node_modules/polis-memory/node_modules/@polis-memory/linux-x64/bin/polis")));
        assert!(!is_transient(Path::new("/Users/a/.local/bin/polis")));
        assert!(!is_transient(Path::new("/opt/homebrew/bin/polis")));
        let (same, note) = settle_binary(PathBuf::from("/Users/a/.local/bin/polis")).unwrap();
        assert_eq!((same, note), (PathBuf::from("/Users/a/.local/bin/polis"), None));
    }

    #[test]
    fn only_agents_with_hooks_get_capture() {
        assert_eq!(Agent::ClaudeCode.hooks(), Some(HookClient::Claude));
        assert_eq!(Agent::ClaudeDesktop.hooks(), None);
        assert!(Agent::ClaudeDesktop.offer().contains("not captured"));
    }

    #[test]
    fn initialization_creates_the_home_once_and_the_skill_round_trips() {
        let root = std::env::temp_dir().join(format!("polis-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = Home { root: root.join("home") };
        let first = ensure_initialized(&home).unwrap();
        assert!(first.iter().any(|l| l.contains("created the store")), "{first:?}");
        assert!(home.db_path().exists() && home.token_path().exists());
        assert!(ensure_initialized(&home).unwrap().is_empty(), "idempotent: {:?}", ensure_initialized(&home));
        let skill = root.join("skills").join("polis-memory").join("SKILL.md");
        assert!(install_skill(&home, &skill).unwrap());
        assert!(!install_skill(&home, &skill).unwrap(), "unchanged");
        let text = std::fs::read_to_string(&skill).unwrap();
        assert!(text.contains(&home.token_path().to_string_lossy().to_string()) && !text.contains("{{"));
        assert!(uninstall_skill(&skill).unwrap() && !skill.parent().unwrap().exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
