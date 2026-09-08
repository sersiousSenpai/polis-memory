// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The `polis` command (feature `cli`, Session E1):
//! `init | serve | mcp | hook | capture | search | context | grep | tree |
//! stats | verify | doctor | restore | backup`. Reads go to a running daemon
//! when there is one and to the store file otherwise (`backend`); nothing
//! here needs a model, a key or a network.

pub mod backend;
pub mod doctor;
pub mod home;
pub mod install;
pub mod serve;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use polis_core::api::{CaptureRequest, ContextRequest, GrepRequest, Scope, SearchRequest, TreeRequest};
use polis_core::ledger::Origin;
use polis_core::types::GrepScope;
use polis_core::MemoryApi;
use polis_mcp::render;
use polis_server::hook::CaptureHookSpec;
use polis_store::PolisStore;

use backend::Backend;
use home::Home;

#[derive(Parser)]
#[command(name = "polis", version, about = "Polis Memory — a local-first memory for coding agents", long_about = None)]
struct Cli {
    /// Emit JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    /// A daemon to talk to instead of the store file (also `POLIS_REMOTE`).
    #[arg(long, global = true, value_name = "URL")]
    remote: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create $POLIS_HOME (~/.polis): the store, a private token, config.toml,
    /// and this device's identity — an Ed25519 key (`identity.key`, 0600)
    /// whose hash is your principal id, a device id derived from it and the
    /// device name, and a `principal_bind` event binding the key to the chain.
    /// Re-running adopts nothing twice.
    Init {
        /// The device name (default: the hostname). One key, many devices:
        /// each name is its own chain under the same human.
        #[arg(long)]
        device: Option<String>,
        /// Adopt an existing Redline install: point this home at its
        /// `redline.db` and share the key under `<dir>/polis/` (one device,
        /// one chain — a copy would fork it), alias every author it has
        /// recorded, and bind.
        #[arg(long, value_name = "REDLINE_DATA_DIR")]
        from_redline: Option<PathBuf>,
    },
    /// Run the daemon: HTTP routes, MCP at /mcp, the gardener, rotating backups.
    Serve {
        /// Bind address (default: config.toml `listen`, else 127.0.0.1:7677).
        #[arg(long)]
        listen: Option<String>,
        /// A token file for the writes (required for a non-loopback bind).
        #[arg(long)]
        token_file: Option<PathBuf>,
        /// Serve without running the gardener.
        #[arg(long)]
        no_gardener: bool,
        /// Gardener tick in seconds.
        #[arg(long, default_value_t = 30)]
        tick: u64,
    },
    /// Serve MCP over stdio (what `claude mcp add polis -- polis mcp` runs),
    /// or write a client's config.
    Mcp {
        #[command(subcommand)]
        cmd: Option<McpCmd>,
    },
    /// Install, remove or inspect the UserPromptSubmit capture hook.
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// The capture hook's command: reads the UserPromptSubmit payload on stdin,
    /// records the prompt (through the daemon, or locally), always exits 0.
    Capture,
    /// The answer pack for a question — START HERE.
    Search {
        q: Vec<String>,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        limit: Option<i64>,
        /// Also search the imported peer chains (E3); hits are labelled by source.
        #[arg(long)]
        shared: bool,
    },
    /// The answer pack rendered as one grounding block.
    Context {
        q: Vec<String>,
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        max_tokens: Option<usize>,
        /// Also include the imported peer chains, in a labelled SHARED section.
        #[arg(long)]
        shared: bool,
    },
    /// Literal / regex search over the record.
    Grep {
        literal: String,
        #[arg(long)]
        re: Option<String>,
        #[arg(long)]
        case_sensitive: bool,
        /// all | prompts | browse
        #[arg(long, default_value = "all")]
        kinds: String,
        #[arg(long)]
        limit: Option<i64>,
    },
    /// The class catalog.
    Tree {
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        project: Option<String>,
    },
    /// One organize pass now: file the new lake items — by class centroid,
    /// then a small model batch for the ambiguous ones (or `~inbox` with no
    /// model), and every fifth run the consolidation classifier.
    Organize,
    /// Counts over the record.
    Stats,
    /// Re-walk the hash chain.
    Verify,
    /// Check the install, the file, the chain, the hook, the clients, the backups.
    Doctor,
    /// Swap in a verifying snapshot (newest, or --from FILE). Stop `polis serve` first.
    Restore {
        #[arg(long)]
        from: Option<PathBuf>,
    },
    /// Write one snapshot now (verified, pruned to keep 7).
    Backup,
    /// Export this device's chain as a signed `polis.bundle/2` envelope (E2):
    /// the events from --from-seq to the head, prompt bodies at the policy's
    /// redaction (default: full only for org-visible user prompts, stubs
    /// otherwise), notes, principals, aliases and the bind — signed by your key.
    Export {
        /// Kept for the docs: exports are always signed.
        #[arg(long, default_value_t = true)]
        signed: bool,
        /// First seq to include (default 1, the whole chain).
        #[arg(long)]
        from_seq: Option<i64>,
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the full/gist/stub decision per prompt and export nothing.
        #[arg(long)]
        dry_run: bool,
        /// auto | full | gist | stub (default auto).
        #[arg(long)]
        bodies: Option<String>,
        /// Corpus roles whose bodies may ship, comma-separated (default: user).
        #[arg(long)]
        roles: Option<String>,
        /// Ship the local vectors beside every full body (a peer with the same
        /// embedder skips the re-embed; any other discards them).
        #[arg(long)]
        include_vectors: bool,
    },
    /// Import a peer's `polis.bundle/2` envelope (E3): verify everything —
    /// the key hashes to its principal, the device derives from it, the
    /// signature, the payload hash, the bind, every event hash and link —
    /// then trust, then continuity with what we already hold of that chain,
    /// then store its rows as FOREIGN (never re-chained). `--verify-only`
    /// reports and stores nothing.
    Import {
        file: PathBuf,
        /// Verify and report, never store.
        #[arg(long)]
        verify_only: bool,
        /// Trust an unknown key on first use (its fingerprint is printed).
        #[arg(long)]
        tofu: bool,
        /// Import even when no subscription matches the chain.
        #[arg(long)]
        force: bool,
    },
    /// Publish this device's new segments and import subscribed peers' —
    /// through a folder (`--folder DIR`, e.g. a synced drive) or a git
    /// remote (`--git URL`). Replicate, never federate: after a sync every
    /// question is answered from local rows.
    Sync {
        #[arg(long, value_name = "DIR")]
        folder: Option<PathBuf>,
        #[arg(long, value_name = "URL")]
        git: Option<String>,
        /// Only publish.
        #[arg(long)]
        publish_only: bool,
        /// Only fetch + import.
        #[arg(long)]
        fetch_only: bool,
        /// Trust unknown keys on first use.
        #[arg(long)]
        tofu: bool,
        /// Import chains no subscription matches.
        #[arg(long)]
        force: bool,
        /// Ship vectors beside full bodies.
        #[arg(long)]
        include_vectors: bool,
        /// auto | full | gist | stub (default auto).
        #[arg(long)]
        bodies: Option<String>,
    },
    /// What to import: by human (id, fingerprint prefix or display name),
    /// project root, or class. No subscriptions = import every chain a
    /// transport offers.
    Subscribe {
        #[command(subcommand)]
        cmd: SubscribeCmd,
    },
    /// The keys this install trusts (admin-distributed, or on first use).
    Trust {
        #[command(subcommand)]
        cmd: TrustCmd,
    },
    /// The peer chains held here: source, head, forked.
    Peers,
}

#[derive(Subcommand)]
enum SubscribeCmd {
    Add {
        #[arg(long)]
        principal: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        class: Option<String>,
    },
    List,
    Rm {
        id: i64,
        /// Also delete every row imported from chains this subscription
        /// covered (and clear their fork marks).
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
enum TrustCmd {
    List,
    /// Trust a human's key: 64 hex chars, or `@FILE` holding them.
    Add {
        pubkey: String,
        #[arg(long)]
        name: Option<String>,
    },
    Rm {
        principal: String,
    },
    /// This install's human fingerprint and public key — what a peer adds.
    Fingerprint,
}

#[derive(Subcommand)]
enum McpCmd {
    /// Write the MCP server entry into a client's config (merge, never overwrite).
    Install {
        #[arg(long, value_enum)]
        client: ClientArg,
        /// The config file (default: the client's usual place).
        #[arg(long)]
        path: Option<PathBuf>,
        /// The polis binary to point at (default: this one).
        #[arg(long)]
        polis: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ClientArg {
    Claude,
    Codex,
    Project,
    Cursor,
    Windsurf,
    ClaudeDesktop,
}

#[derive(Subcommand)]
enum HookCmd {
    Install {
        /// Claude Code's global settings (default ~/.claude/settings.json).
        #[arg(long)]
        settings: Option<PathBuf>,
        /// The polis binary the hook runs (default: this one).
        #[arg(long)]
        polis: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    Status {
        #[arg(long)]
        settings: Option<PathBuf>,
    },
}

/// The spec `polis hook` installs for this home.
pub fn hook_spec(home: &Home) -> CaptureHookSpec {
    doctor::spec_for(home, None)
}

fn init_logging() {
    let level = match std::env::var("POLIS_LOG").ok().as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("trace") => tracing::Level::TRACE,
        Some("debug") => tracing::Level::DEBUG,
        Some("info") => tracing::Level::INFO,
        Some("error") => tracing::Level::ERROR,
        _ => tracing::Level::WARN,
    };
    let _ = tracing_subscriber::fmt().with_max_level(level).with_writer(std::io::stderr).with_ansi(false).try_init();
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().map_err(|e| format!("runtime: {e}"))
}

fn words(v: Vec<String>) -> Option<String> {
    let s = v.join(" ").trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn emit<T: serde::Serialize>(json: bool, value: &T, text: impl FnOnce() -> String) {
    if json {
        println!("{}", serde_json::to_string_pretty(value).unwrap_or_default());
    } else {
        println!("{}", text());
    }
}

/// The entry point: exit code.
pub fn main() -> i32 {
    init_logging();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("polis: {e}");
            1
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let home = Home::resolve()?;
    let json = cli.json;
    match cli.cmd {
        Cmd::Init { device, from_redline } => {
            let fresh = !home.exists();
            home.ensure()?;
            let wrote_config = home.ensure_config()?;
            if let Some(dir) = &from_redline {
                let redline_db = dir.join("redline.db");
                if !redline_db.exists() {
                    return Err(format!("{} has no redline.db", dir.display()));
                }
                // Point, never copy: two writers on one chain would be two
                // heads under one id, which is exactly what devices exist to
                // prevent. Redline and `polis` are the same device here.
                home.config_set("db", &redline_db.to_string_lossy())?;
                home.config_set("identity_dir", &dir.join("polis").to_string_lossy())?;
            }
            if let Some(name) = &device {
                home.config_set("device", name)?;
            }
            let db = home.db_path();
            let created_db = !db.exists();
            let store = PolisStore::open(&db).map_err(|e| format!("create {}: {e}", db.display()))?;
            home.ensure_token()?;
            let identity_dir = home.identity_dir();
            let (identity, created_key) = crate::identity::Identity::load_or_create(&identity_dir, home.device_name())?;
            let login = crate::identity::login_name();
            let report = crate::identity::adopt(&store, &identity, &login)?;
            emit(
                json,
                &serde_json::json!({
                    "home": home.root, "db": db, "createdHome": fresh, "createdDb": created_db, "wroteConfig": wrote_config,
                    "identity": { "dir": identity_dir, "createdKey": created_key, "principal": identity.principal_id(), "fingerprint": identity.fingerprint(), "device": identity.device_id(), "deviceName": identity.device_name },
                    "adopt": report,
                }),
                || {
                    format!(
                        "home     {}{}\nstore    {}{}\ntoken    {}\nconfig   {}{}\nidentity {} ({}; key {})\n         principal {}  device {} ({})\n         bind #{}{}; aliases +{}, stamped {} rows{}\n\nnext: `polis hook install` (capture prompts), `polis mcp install --client claude` (answer them), `polis serve` (a daemon with the gardener and backups).",
                        home.root.display(),
                        if fresh { " (created)" } else { "" },
                        db.display(),
                        if created_db { " (created)" } else { " (present)" },
                        home.token_path().display(),
                        home.config_path().display(),
                        if wrote_config { " (written)" } else { "" },
                        identity_dir.display(),
                        crate::identity::KEY_FILE,
                        if created_key { "created" } else { "present" },
                        identity.fingerprint(),
                        polis_core::identity::fingerprint(&identity.device_id()),
                        identity.device_name,
                        report.bind_seq.unwrap_or(0),
                        if report.already_bound { " (already bound)" } else { " (appended)" },
                        report.aliases_seeded.len(),
                        report.stamped,
                        {
                            let u = &report.unscoped;
                            let left = u.prompts + u.browse_events + u.user_notes + u.class_nodes + u.class_observations;
                            if left > 0 { format!(", {left} still unscoped") } else { String::new() }
                        }
                    )
                },
            );
            Ok(())
        }
        Cmd::Serve { listen, token_file, no_gardener, tick } => {
            let rt = runtime()?;
            rt.block_on(serve::run(&home, serve::ServeOptions { listen, token_file, no_gardener, tick_secs: tick }))
        }
        Cmd::Mcp { cmd: None } => {
            let backend = backend::choose(&home, cli.remote);
            tracing::info!(backend = %backend.describe(), "polis mcp");
            let api = backend::open(&home, &backend)?;
            let rt = runtime()?;
            rt.block_on(polis_mcp::serve_stdio(api)).map_err(|e| format!("mcp: {e}"))
        }
        Cmd::Mcp { cmd: Some(McpCmd::Install { client, path, polis }) } => {
            let client = match client {
                ClientArg::Claude => install::Client::Claude,
                ClientArg::Codex => install::Client::Codex,
                ClientArg::Project => install::Client::Project,
                ClientArg::Cursor => install::Client::Cursor,
                ClientArg::Windsurf => install::Client::Windsurf,
                ClientArg::ClaudeDesktop => install::Client::ClaudeDesktop,
            };
            let (path, outcome) = install::install(client, path, polis)?;
            emit(json, &serde_json::json!({ "path": path, "outcome": outcome }), || format!("{}: polis MCP server {outcome}", path.display()));
            Ok(())
        }
        Cmd::Hook { cmd } => {
            let settings = |s: Option<PathBuf>| s.or_else(install::claude_settings_path).ok_or_else(|| "no settings path (set HOME or pass --settings)".to_string());
            match cmd {
                HookCmd::Install { settings: s, polis } => {
                    let path = settings(s)?;
                    let spec = doctor::spec_for(&home, polis);
                    let installed = spec.install_at(&path)?;
                    emit(json, &serde_json::json!({ "settings": path, "installed": installed, "command": spec.command() }), || format!("{}: capture hook {} → {}", path.display(), if installed { "installed" } else { "NOT installed" }, spec.command()));
                    Ok(())
                }
                HookCmd::Uninstall { settings: s } => {
                    let path = settings(s)?;
                    let still = hook_spec(&home).uninstall_at(&path)?;
                    emit(json, &serde_json::json!({ "settings": path, "installed": still }), || format!("{}: capture hook {}", path.display(), if still { "still present" } else { "removed" }));
                    Ok(())
                }
                HookCmd::Status { settings: s } => {
                    let path = settings(s)?;
                    let spec = hook_spec(&home);
                    let (installed, current) = (spec.installed_at(&path), spec.current_at(&path));
                    emit(json, &serde_json::json!({ "settings": path, "installed": installed, "current": current, "command": spec.command() }), || {
                        format!("{}: {}{}\ncommand: {}", path.display(), if installed { "installed" } else { "not installed" }, if installed && !current { " (stale — run `polis hook install`)" } else { "" }, spec.command())
                    });
                    Ok(())
                }
            }
        }
        Cmd::Capture => {
            capture(&home);
            Ok(())
        }
        Cmd::Search { q, node, limit, shared } => {
            let api = open(&home, cli.remote)?;
            let req = SearchRequest { q: words(q), node, limit, scope: Scope { include_shared: shared, ..Default::default() } };
            let pack = api.search(&req).map_err(|e| e.to_string())?;
            emit(json, &pack, || render::pack(&pack));
            Ok(())
        }
        Cmd::Context { q, node, max_tokens, shared } => {
            let api = open(&home, cli.remote)?;
            let q = words(q).ok_or("a question is required")?;
            let block = api.context(&ContextRequest { q, node, max_tokens, scope: Scope { include_shared: shared, ..Default::default() } }).map_err(|e| e.to_string())?;
            emit(json, &block, || render::context(&block));
            Ok(())
        }
        Cmd::Grep { literal, re, case_sensitive, kinds, limit } => {
            let api = open(&home, cli.remote)?;
            let hits = api
                .grep(&GrepRequest { literal, regex: re, case_sensitive, kinds: GrepScope::parse(Some(&kinds)), limit: Some(limit.unwrap_or(30)), scope: Scope::default() })
                .map_err(|e| e.to_string())?;
            emit(json, &serde_json::json!({ "hits": hits }), || render::grep(&hits));
            Ok(())
        }
        Cmd::Tree { root, project } => {
            let api = open(&home, cli.remote)?;
            let nodes = api.tree(&TreeRequest { root, project, scope: Scope::default() }).map_err(|e| e.to_string())?;
            emit(json, &serde_json::json!({ "nodes": nodes }), || render::tree(&nodes));
            Ok(())
        }
        Cmd::Organize => {
            let api = open(&home, cli.remote)?;
            let rt = runtime()?;
            let r = rt.block_on(api.organize(&Scope::default())).map_err(|e| e.to_string())?;
            emit(json, &r, || {
                if r.ran {
                    format!("organized · {} · seq {}..{}", r.summary, r.seq_from, r.seq_to)
                } else {
                    format!("nothing to organize · {}", r.summary)
                }
            });
            Ok(())
        }
        Cmd::Stats => {
            let api = open(&home, cli.remote)?;
            let s = api.stats(&Scope::default()).map_err(|e| e.to_string())?;
            emit(json, &s, || render::stats(&s));
            Ok(())
        }
        Cmd::Verify => {
            let api = open(&home, cli.remote)?;
            let v = api.verify().map_err(|e| e.to_string())?;
            emit(json, &v, || render::verdict(&v));
            if v.ok {
                Ok(())
            } else {
                Err(format!("the chain does not verify (first bad seq {:?}) — `polis doctor` for the restore offer", v.first_bad_seq))
            }
        }
        Cmd::Doctor => {
            let d = doctor::run(&home);
            emit(json, &d, || doctor::render(&d));
            if d.problems.is_empty() {
                Ok(())
            } else {
                Err(format!("{} problem(s)", d.problems.len()))
            }
        }
        Cmd::Restore { from } => {
            if let Some(base) = backend::daemon_alive(&home) {
                return Err(format!("a daemon holds the store ({base}) — stop `polis serve` before restoring"));
            }
            let r = crate::backup::restore(&home.db_path(), from.as_deref(), &home.backups_dir())?;
            emit(json, &serde_json::json!({ "from": r.from, "keptAs": r.kept_as, "checked": r.verdict.checked, "ok": r.verdict.ok }), || {
                format!("restored {} → {}\n  snapshot chain: {} events, ok\n  previous file kept as {}", r.from.display(), home.db_path().display(), r.verdict.checked, r.kept_as.as_deref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none)".into()))
            });
            Ok(())
        }
        Cmd::Export { signed: _, from_seq, out, dry_run, bodies, roles, include_vectors } => {
            let db = home.db_path();
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            let mut policy = crate::envelope::Policy::default();
            if let Some(b) = bodies.as_deref() {
                policy.bodies = crate::envelope::Bodies::parse(b).ok_or_else(|| format!("--bodies must be auto|full|gist|stub, not `{b}`"))?;
            }
            if let Some(r) = roles.as_deref() {
                policy.roles = r.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
            let opts = crate::envelope::BuildOptions { from_seq, policy, org_id: None, include_vectors };
            if dry_run {
                let d = crate::envelope::decisions(&store, &opts)?;
                emit(json, &d, || {
                    let mut s = String::new();
                    for x in &d {
                        s.push_str(&format!("#{:<6} prompt {:<6} {:<6} {:<8} {:?} ({} bytes)\n", x.seq, x.prompt_id, x.role, x.visibility, x.redaction, x.bytes));
                    }
                    let count = |r: crate::envelope::Redaction| d.iter().filter(|x| x.redaction == r).count();
                    s.push_str(&format!("{} prompts; full {} · gist {} · stub {}", d.len(), count(crate::envelope::Redaction::Full), count(crate::envelope::Redaction::Gist), count(crate::envelope::Redaction::Stub)));
                    s
                });
                return Ok(());
            }
            let identity = crate::identity::Identity::load(&home.identity_dir(), home.device_name())?
                .ok_or_else(|| "no identity — run `polis init` first".to_string())?;
            let env = crate::envelope::build(&store, &identity, &crate::identity::login_name(), &opts)?;
            let text = serde_json::to_string_pretty(&env).map_err(|e| e.to_string())?;
            match out {
                Some(path) => {
                    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
                    let s = &env.header.segment;
                    emit(json, &serde_json::json!({ "out": path, "chainId": env.header.chain_id, "fromSeq": s.from_seq, "toSeq": s.to_seq, "headHash": s.head_hash, "events": env.payload.events.len(), "prompts": env.payload.prompts.len() }), || {
                        format!("wrote {} · chain {} · seq {}..{} · {} events, {} prompts", path.display(), polis_core::identity::fingerprint(&env.header.chain_id), s.from_seq, s.to_seq, env.payload.events.len(), env.payload.prompts.len())
                    });
                }
                None => println!("{text}"),
            }
            Ok(())
        }
        Cmd::Import { file, verify_only, tofu, force } => {
            let text = std::fs::read_to_string(&file).map_err(|e| format!("read {}: {e}", file.display()))?;
            let env: crate::envelope::Envelope = serde_json::from_str(&text).map_err(|e| format!("{} is not a polis.bundle/2 envelope: {e}", file.display()))?;
            let db = home.db_path();
            if !verify_only {
                let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
                let opts = crate::sharing::ImportOptions { tofu, force, local_model: None };
                return match crate::sharing::import(&store, &env, &opts) {
                    Ok(r) => {
                        emit(json, &r, || render_import(&r));
                        Ok(())
                    }
                    Err(e) => {
                        emit(json, &serde_json::json!({ "ok": false, "error": e }), || format!("REFUSED · {e}"));
                        Err(format!("import refused: {e}"))
                    }
                };
            }
            let result = if db.exists() {
                let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
                crate::envelope::verify_against(&store, &env)
            } else {
                crate::envelope::verify(&env)
            };
            match result {
                Ok(v) => {
                    emit(json, &serde_json::json!({ "ok": true, "verified": v }), || {
                        format!(
                            "ok · chain {} ({}) of human {} · seq {}..{} · {} events, {} prompts ({} full bodies), {} bind(s)",
                            polis_core::identity::fingerprint(&v.chain_id), v.device_name, polis_core::identity::fingerprint(&v.human), v.from_seq, v.to_seq, v.events, v.prompts, v.full_bodies, v.binds
                        )
                    });
                    Ok(())
                }
                Err(e) => {
                    emit(json, &serde_json::json!({ "ok": false, "error": e }), || format!("REFUSED · {e}"));
                    Err(format!("envelope refused: {e}"))
                }
            }
        }
        Cmd::Sync { folder, git, publish_only, fetch_only, tofu, force, include_vectors, bodies } => {
            let transport: Box<dyn crate::transport::SegmentTransport> = match (folder, git) {
                (Some(dir), None) => Box::new(crate::transport::FolderTransport::new(dir)),
                (None, Some(url)) => Box::new(crate::transport::GitTransport::new(url.clone(), home.sync_dir().join("git").join(short_hash(&url)))),
                _ => return Err("pass exactly one of --folder DIR or --git URL".into()),
            };
            let db = home.db_path();
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            let identity = crate::identity::Identity::load(&home.identity_dir(), home.device_name())?
                .ok_or_else(|| "no identity — run `polis init` first".to_string())?;
            let mut policy = crate::envelope::Policy::default();
            if let Some(b) = bodies.as_deref() {
                policy.bodies = crate::envelope::Bodies::parse(b).ok_or_else(|| format!("--bodies must be auto|full|gist|stub, not `{b}`"))?;
            }
            let opts = crate::sync::SyncOptions { publish: !fetch_only, fetch: !publish_only, tofu, force, policy, include_vectors, local_model: None };
            let rt = runtime()?;
            let report = rt.block_on(crate::sync::sync(&store, &identity, &crate::identity::login_name(), transport.as_ref(), &opts));
            emit(json, &report, || render_sync(&report));
            if report.errors.is_empty() {
                Ok(())
            } else {
                Err(format!("{} error(s) during sync", report.errors.len()))
            }
        }
        Cmd::Subscribe { cmd } => {
            let db = home.db_path();
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            match cmd {
                SubscribeCmd::Add { principal, project, class } => {
                    if principal.is_none() && project.is_none() && class.is_none() {
                        return Err("give at least one of --principal, --project, --class".into());
                    }
                    let id = store.subscribe(principal.as_deref(), project.as_deref(), class.as_deref()).map_err(|e| e.to_string())?;
                    emit(json, &serde_json::json!({ "id": id }), || format!("subscription #{id} added"));
                    Ok(())
                }
                SubscribeCmd::List => {
                    let rows = store.subscriptions().map_err(|e| e.to_string())?;
                    emit(json, &rows, || {
                        if rows.is_empty() {
                            "no subscriptions — every chain a transport offers is imported".to_string()
                        } else {
                            rows.iter().map(|r| format!("#{} principal {} · project {} · class {}", r.id, r.principal.as_deref().unwrap_or("*"), r.project.as_deref().unwrap_or("*"), r.class.as_deref().unwrap_or("*"))).collect::<Vec<_>>().join("\n")
                        }
                    });
                    Ok(())
                }
                SubscribeCmd::Rm { id, purge } => {
                    let rows = store.subscriptions().map_err(|e| e.to_string())?;
                    let Some(row) = rows.iter().find(|r| r.id == id) else { return Err(format!("no subscription #{id}")) };
                    let mut purged = 0usize;
                    if purge {
                        for c in store.list_foreign_chains().map_err(|e| e.to_string())? {
                            let matches = row.principal.as_deref().is_none_or(|p| {
                                let p_l = p.to_ascii_lowercase();
                                c.human_id == p_l || c.human_id.starts_with(&p_l) || polis_core::identity::fingerprint(&c.human_id) == p_l || c.display_name.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(p))
                            });
                            if matches {
                                purged += store.forget_foreign_chain(&c.chain_id).map_err(|e| e.to_string())?;
                            }
                        }
                    }
                    store.unsubscribe(id).map_err(|e| e.to_string())?;
                    emit(json, &serde_json::json!({ "removed": id, "purgedRows": purged }), || format!("subscription #{id} removed{}", if purge { format!(" · {purged} foreign row(s) purged") } else { String::new() }));
                    Ok(())
                }
            }
        }
        Cmd::Trust { cmd } => {
            let db = home.db_path();
            match cmd {
                TrustCmd::Fingerprint => {
                    let identity = crate::identity::Identity::load(&home.identity_dir(), home.device_name())?
                        .ok_or_else(|| "no identity — run `polis init` first".to_string())?;
                    emit(json, &serde_json::json!({ "principal": identity.principal_id(), "fingerprint": identity.fingerprint(), "pubkey": identity.pubkey_hex() }), || {
                        format!("human {} · fingerprint {} · pubkey {}", identity.principal_id(), identity.fingerprint(), identity.pubkey_hex())
                    });
                    Ok(())
                }
                TrustCmd::List => {
                    let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
                    let rows = store.trust_list().map_err(|e| e.to_string())?;
                    emit(json, &rows, || {
                        if rows.is_empty() {
                            "no trusted keys".to_string()
                        } else {
                            rows.iter().map(|t| format!("{} ({}) · {}{}", t.fingerprint, t.source, t.principal_id, t.display_name.as_deref().map(|n| format!(" · {n}")).unwrap_or_default())).collect::<Vec<_>>().join("\n")
                        }
                    });
                    Ok(())
                }
                TrustCmd::Add { pubkey, name } => {
                    let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
                    let hex = match pubkey.strip_prefix('@') {
                        Some(path) => std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?.trim().to_string(),
                        None => pubkey.trim().to_string(),
                    };
                    let bytes = crate::identity::hex_decode(&hex).filter(|b| b.len() == 32).ok_or("a public key is 64 hex chars")?;
                    let principal = polis_core::identity::principal_id(&bytes);
                    let entry = polis_store::foreign::TrustEntry { principal_id: principal.clone(), pubkey: hex, fingerprint: polis_core::identity::fingerprint(&principal), source: "admin".into(), display_name: name, added_at: polis_core::ledger::now_millis() };
                    let added = store.trust_set(&entry).map_err(|e| e.to_string())?;
                    emit(json, &serde_json::json!({ "principal": principal, "added": added }), || format!("{} {}", entry.fingerprint, if added { "trusted (admin)" } else { "already trusted — `polis trust rm` first to replace" }));
                    Ok(())
                }
                TrustCmd::Rm { principal } => {
                    let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
                    let rows = store.trust_list().map_err(|e| e.to_string())?;
                    let target = rows.iter().find(|t| t.principal_id == principal || t.fingerprint == principal).map(|t| t.principal_id.clone()).ok_or_else(|| format!("no trusted key {principal}"))?;
                    store.trust_remove(&target).map_err(|e| e.to_string())?;
                    emit(json, &serde_json::json!({ "removed": target }), || "removed".to_string());
                    Ok(())
                }
            }
        }
        Cmd::Peers => {
            let db = home.db_path();
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            let chains = store.list_foreign_chains().map_err(|e| e.to_string())?;
            emit(json, &chains, || {
                if chains.is_empty() {
                    "no peer chains held".to_string()
                } else {
                    chains.iter().map(|c| format!("{} ({}) · head #{} · {}{}", polis_core::identity::fingerprint(&c.chain_id), c.display_name.as_deref().unwrap_or("unnamed"), c.head_seq, c.device_name.as_deref().unwrap_or("?"), if c.forked { " · FORKED" } else { "" })).collect::<Vec<_>>().join("\n")
                }
            });
            Ok(())
        }
        Cmd::Backup => {
            let db = home.db_path();
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            let policy = crate::backup::BackupPolicy::default();
            let r = crate::backup::backup_verify_prune(&store, &home.backups_dir(), policy.keep)?;
            emit(json, &serde_json::json!({ "path": r.path, "ok": r.verdict.ok, "checked": r.verdict.checked, "pruned": r.pruned }), || {
                format!("{} · chain {} ({} events) · pruned {}", r.path.display(), if r.verdict.ok { "ok" } else { "BROKEN" }, r.verdict.checked, r.pruned)
            });
            if r.verdict.ok {
                Ok(())
            } else {
                Err("the snapshot was written but its chain does not verify".into())
            }
        }
    }
}

fn open(home: &Home, remote: Option<String>) -> Result<Arc<dyn MemoryApi>, String> {
    let backend = backend::choose(home, remote);
    backend::open(home, &backend)
}

/// The hook's whole life: never block prompt submission. Every branch ends
/// in exit 0; anything printed to stdout is the daemon's own hook answer
/// (`hookSpecificOutput`), which the harness hands the model as context.
fn capture(home: &Home) {
    use std::io::Read;
    let started = std::time::Instant::now();
    let budget = Duration::from_millis(1000);
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() || raw.trim().is_empty() {
        return;
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let prompt = polis_server::ingest_prompt_text(&payload);
    if prompt.is_empty() {
        return;
    }
    let session = payload.get("session_id").and_then(serde_json::Value::as_str).map(str::to_string);
    let cwd = payload.get("cwd").and_then(serde_json::Value::as_str).map(str::to_string);

    // A daemon: hand it the payload as the hook would, and relay only a
    // body carrying hookSpecificOutput.
    if let Some(info) = home.read_serve() {
        let remaining = budget.saturating_sub(started.elapsed()).max(Duration::from_millis(150));
        let api = polis_mcp::remote::RemoteApi::new(info.base_url(), home.read_token()).with_timeout(remaining);
        match api.capture_raw(&payload) {
            Ok(body) => {
                if body.get("hookSpecificOutput").is_some() {
                    print!("{body}");
                }
                return;
            }
            Err(e) => tracing::debug!(error = %e, "daemon capture failed; recording locally"),
        }
    }
    // No daemon (or it did not answer in time): the store, here.
    let db = home.db_path();
    if !db.exists() {
        tracing::warn!(db = %db.display(), "capture: no store — run `polis init`");
        return;
    }
    match backend::open(home, &Backend::Local) {
        Ok(api) => {
            let req = CaptureRequest { body: prompt, origin: Origin::External, surface: "external".into(), session, project: cwd };
            if let Err(e) = api.capture(&req) {
                tracing::warn!(error = %e, "capture: local record failed");
            }
        }
        Err(e) => tracing::warn!(error = %e, "capture: could not open the store"),
    }
}

fn short_hash(s: &str) -> String {
    polis_core::ledger::sha256_hex(s.as_bytes())[..16].to_string()
}

fn render_import(r: &crate::sharing::ImportReport) -> String {
    let what = match &r.outcome {
        crate::sharing::ImportOutcome::Appended { appended } => format!("{appended} new event(s)"),
        crate::sharing::ImportOutcome::NoOp => "already held, unchanged".to_string(),
        crate::sharing::ImportOutcome::OwnChain => "our own chain — continuity ok, nothing stored".to_string(),
    };
    format!(
        "ok · {} · chain {} of human {} · seq {}..{} · {} prompts, {} notes, {} tombstoned · vectors kept {} / discarded {}{}",
        what,
        polis_core::identity::fingerprint(&r.chain_id),
        polis_core::identity::fingerprint(&r.human),
        r.from_seq,
        r.to_seq,
        r.rows.prompts,
        r.rows.notes,
        r.rows.tombstoned,
        r.vectors_kept,
        r.vectors_discarded,
        r.trusted_now.as_deref().map(|f| format!(" · TRUSTED ON FIRST USE: {f} (verify this fingerprint with the peer)")).unwrap_or_default()
    )
}

fn render_sync(r: &crate::sync::SyncReport) -> String {
    let mut s = String::new();
    match &r.published {
        Some(p) => s.push_str(&format!("published seq {}..{} → {}\n", p.from_seq, p.to_seq, p.locator)),
        None => s.push_str("published nothing new\n"),
    }
    for i in &r.imported {
        s.push_str(&render_import(i));
        s.push('\n');
    }
    for (c, why) in &r.skipped {
        s.push_str(&format!("skipped {}: {why}\n", polis_core::identity::fingerprint(c)));
    }
    for (c, e) in &r.errors {
        s.push_str(&format!("ERROR {}: {e}\n", if c.len() == 64 { polis_core::identity::fingerprint(c) } else { c.clone() }));
    }
    s.trim_end().to_string()
}
