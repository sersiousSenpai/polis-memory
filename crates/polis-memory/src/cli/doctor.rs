// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis doctor`: the install, the file, the chain, the hook, the clients,
//! the model binaries, the backups, full-disk encryption — and the offer to
//! restore when the chain is red or `quick_check` fails.

use std::path::PathBuf;

use polis_core::MemoryApi;
use polis_server::hook::CaptureHookSpec;
use polis_store::PolisStore;
use serde::Serialize;

use super::backend::{daemon_alive, PROBE_TIMEOUT};
use super::home::{current_exe, on_path, Home};
use super::install::{claude_settings_path, json_has_polis, Client};
use crate::backup;

#[derive(Debug, Default, Serialize)]
pub struct Doctor {
    pub home: String,
    pub home_exists: bool,
    pub db: String,
    pub db_exists: bool,
    pub db_bytes: u64,
    pub token_present: bool,
    pub config_present: bool,
    pub daemon: Option<String>,
    pub chain_ok: Option<bool>,
    pub chain_checked: i64,
    pub chain_first_bad_seq: Option<i64>,
    pub quick_check: Option<Result<(), String>>,
    pub head_seq: Option<i64>,
    pub total_prompts: Option<i64>,
    pub model: Option<String>,
    pub embedder: Option<String>,
    pub hook_installed: bool,
    pub hook_current: bool,
    pub hook_settings: Option<String>,
    pub client_claude: bool,
    pub client_codex: bool,
    pub client_project: bool,
    pub binaries: Vec<(String, bool)>,
    pub backups: usize,
    pub newest_verifying_backup: Option<String>,
    pub fde: String,
    pub identity: String,
    /// The file could not even be opened as a store (malformed, wrong schema).
    pub store_unreadable: bool,
    pub offer_restore: bool,
    pub problems: Vec<String>,
}

fn fde_status() -> String {
    if cfg!(target_os = "macos") {
        match std::process::Command::new("fdesetup").arg("status").output() {
            Ok(o) => {
                let s = String::from_utf8_lossy(&o.stdout);
                if s.contains("FileVault is On") {
                    "on (FileVault)".into()
                } else if s.contains("FileVault is Off") {
                    "OFF (FileVault) — the store is plain SQLite; turn disk encryption on".into()
                } else {
                    "unknown".into()
                }
            }
            Err(_) => "unknown".into(),
        }
    } else {
        "unknown (checked on macOS only for now)".into()
    }
}

pub fn run(home: &Home) -> Doctor {
    let mut d = Doctor { home: home.root.display().to_string(), home_exists: home.exists(), ..Default::default() };
    let db = home.db_path();
    d.db = db.display().to_string();
    d.db_exists = db.is_file();
    d.db_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    d.token_present = home.read_token().is_some();
    d.config_present = home.config_path().is_file();
    d.daemon = daemon_alive(home);
    d.identity = "keys and device chains land in E2 (identity); the author is the local user name today".into();
    d.fde = fde_status();
    for name in ["claude", "codex", "ollama"] {
        d.binaries.push((name.into(), on_path(name).is_some()));
    }

    // The chain and the file: through the daemon when one is up (its
    // verify walks the same rows), else the file here.
    if d.db_exists {
        match &d.daemon {
            Some(base) => {
                let api = polis_mcp::remote::RemoteApi::new(base.clone(), home.read_token()).with_timeout(PROBE_TIMEOUT * 10);
                match api.health() {
                    Ok(h) => {
                        d.chain_ok = Some(h.ok);
                        d.chain_checked = h.chain.checked;
                        d.chain_first_bad_seq = h.chain.first_bad_seq;
                        d.head_seq = Some(h.head_seq);
                        d.total_prompts = Some(h.total_prompts);
                        d.model = h.model;
                        d.embedder = Some(h.embedder);
                    }
                    Err(e) => d.problems.push(format!("daemon health: {e}")),
                }
            }
            None => match PolisStore::open(&db) {
                Ok(store) => {
                    match store.verify_ledger_chain() {
                        Ok(v) => {
                            d.chain_ok = Some(v.ok);
                            d.chain_checked = v.checked;
                            d.chain_first_bad_seq = v.first_bad_seq;
                        }
                        Err(e) => d.problems.push(format!("chain verify: {e}")),
                    }
                    match store.quick_check() {
                        Ok(r) => d.quick_check = Some(r),
                        Err(e) => d.problems.push(format!("quick_check: {e}")),
                    }
                    d.head_seq = store.max_ledger_seq().ok();
                    d.total_prompts = store.prompt_counts_by_surface().ok().map(|v| v.iter().map(|(_, c)| c).sum());
                }
                Err(e) => {
                    d.store_unreadable = true;
                    d.problems.push(format!("open {}: {e}", db.display()));
                }
            },
        }
    } else {
        d.problems.push("no store — run `polis init`".into());
    }

    // Backups.
    let backups = home.backups_dir();
    d.backups = backup::list_snapshots(&backups).len();
    d.newest_verifying_backup = backup::newest_verifying(&backups).map(|p| p.display().to_string());

    // The hook and the clients.
    if let Some(settings) = claude_settings_path() {
        d.hook_settings = Some(settings.display().to_string());
        let spec = super::hook_spec(home);
        d.hook_installed = spec.installed_at(&settings);
        d.hook_current = spec.current_at(&settings);
        // Another capture hook (a host's, e.g. Redline's curl to :7676) beside ours?
        if let Ok(text) = std::fs::read_to_string(&settings) {
            if text.contains("/v1/prompts/ingest") && !text.contains(&spec.ingest_url) {
                d.problems.push("another capture hook (a host's) is installed in the same settings — both will record each prompt into their own store".into());
            }
        }
    }
    d.client_claude = Client::Claude.default_path().is_some_and(|p| json_has_polis(&p));
    d.client_project = Client::Project.default_path().is_some_and(|p| json_has_polis(&p));
    d.client_codex = Client::Codex.default_path().is_some_and(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("[mcp_servers.polis]")));

    let red = d.store_unreadable || d.chain_ok == Some(false) || matches!(d.quick_check, Some(Err(_)));
    if red {
        match (&d.chain_ok, &d.quick_check) {
            (Some(false), _) => d.problems.push(format!("the chain does not verify (first bad seq {:?})", d.chain_first_bad_seq)),
            (_, Some(Err(c))) => d.problems.push(format!("quick_check: {c}")),
            _ => {}
        }
        d.offer_restore = d.newest_verifying_backup.is_some();
    }
    d
}

pub fn render(d: &Doctor) -> String {
    let yn = |b: bool| if b { "yes" } else { "no" };
    let mut out = String::new();
    out.push_str(&format!("home        {} ({})\n", d.home, if d.home_exists { "present" } else { "MISSING — run `polis init`" }));
    out.push_str(&format!("store       {} ({}{})\n", d.db, if d.db_exists { "present" } else { "missing" }, if d.db_exists { format!(", {} bytes", d.db_bytes) } else { String::new() }));
    out.push_str(&format!("token       {} · config {}\n", yn(d.token_present), yn(d.config_present)));
    out.push_str(&format!("daemon      {}\n", d.daemon.as_deref().unwrap_or("not running (commands open the store locally)")));
    match d.chain_ok {
        Some(true) => out.push_str(&format!("chain       ok · {} events · head #{}\n", d.chain_checked, d.head_seq.unwrap_or(0))),
        Some(false) => out.push_str(&format!("chain       BROKEN · first bad seq {:?}\n", d.chain_first_bad_seq)),
        None => out.push_str("chain       not checked\n"),
    }
    if let Some(q) = &d.quick_check {
        out.push_str(&format!("quick_check {}\n", match q { Ok(()) => "ok".to_string(), Err(c) => format!("FAILED: {c}") }));
    }
    if let Some(n) = d.total_prompts {
        out.push_str(&format!("prompts     {}\n", n));
    }
    out.push_str(&format!("model       {} · embedder {}\n", d.model.as_deref().unwrap_or("none (capture, retrieval and filing still work)"), d.embedder.as_deref().unwrap_or("absent")));
    out.push_str(&format!("hook        {}{}{}\n", if d.hook_installed { "installed" } else { "not installed (`polis hook install`)" }, if d.hook_installed && !d.hook_current { " · STALE — run `polis hook install` again" } else { "" }, d.hook_settings.as_deref().map(|s| format!(" · {s}")).unwrap_or_default()));
    out.push_str(&format!("clients     claude {} · codex {} · project {}\n", yn(d.client_claude), yn(d.client_codex), yn(d.client_project)));
    out.push_str(&format!("binaries    {}\n", d.binaries.iter().map(|(n, b)| format!("{n} {}", if *b { "found" } else { "absent" })).collect::<Vec<_>>().join(" · ")));
    out.push_str(&format!("backups     {} · newest verifying: {}\n", d.backups, d.newest_verifying_backup.as_deref().unwrap_or("none")));
    out.push_str(&format!("disk crypt  {}\n", d.fde));
    out.push_str(&format!("identity    {}\n", d.identity));
    if d.problems.is_empty() {
        out.push_str("\nno problems found\n");
    } else {
        out.push_str("\nproblems:\n");
        for p in &d.problems {
            out.push_str(&format!("  ! {p}\n"));
        }
    }
    if d.offer_restore {
        out.push_str(&format!(
            "\nRESTORE AVAILABLE: `polis restore` swaps in the newest verifying snapshot\n  ({})\n  and keeps the current file beside it as polis.db.bad. Stop `polis serve` first.\n",
            d.newest_verifying_backup.as_deref().unwrap_or("")
        ));
    }
    out
}

/// The spec `polis hook` installs: the binary form, aimed at this home's
/// daemon address (used only to recognize an install; the binary finds the
/// daemon itself).
pub fn spec_for(home: &Home, polis: Option<PathBuf>) -> CaptureHookSpec {
    CaptureHookSpec::new(format!("http://{}/v1/prompts/ingest", home.listen())).with_binary(polis.unwrap_or_else(current_exe))
}
