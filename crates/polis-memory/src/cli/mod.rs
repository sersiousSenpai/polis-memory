// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The `polis` command (feature `cli`, Session E1):
//! `setup | uninstall | init | service | serve | mcp | hook | skill | capture |
//! search | context | grep | tree | stats | verify | doctor | restore |
//! backup`. Reads go to a running daemon
//! when there is one and to the store file otherwise (`backend`); nothing
//! here needs a model, a key or a network.

pub mod agents;
pub mod backend;
pub mod doctor;
pub mod home;
pub mod install;
pub mod inspect;
pub mod serve;
pub mod service;
pub mod setup;

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
        /// Make this home an ORG NODE (E4): its identity is the org's
        /// principal, its device chain the firm's, and `polis serve --org`
        /// relays segments between the peers under it.
        #[arg(long, value_name = "NAME", conflicts_with = "from_redline")]
        org: Option<String>,
    },
    /// Set Polis up in one step: create the record, connect every agent found
    /// (search tools, capture hooks, the skill), fetch the embedding model,
    /// choose the organizer model, and keep the daemon running. Shows the
    /// plan and asks first; safe to re-run.
    Setup {
        /// Don't ask; apply the plan.
        #[arg(long, short = 'y')]
        yes: bool,
        /// Show the plan and change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Only these agents, comma-separated (claude, codex, cursor,
        /// windsurf, claude-desktop). Default: every one found.
        #[arg(long, value_name = "LIST")]
        clients: Option<String>,
        /// Don't install the daemon service.
        #[arg(long)]
        no_service: bool,
        /// No organizer model: the catalog files by rules only.
        #[arg(long)]
        no_model: bool,
        /// Don't install the polis-memory skill for Claude Code.
        #[arg(long)]
        no_skill: bool,
        /// The polis binary the hooks, MCP configs and service run.
        #[arg(long)]
        polis: Option<PathBuf>,
    },
    /// Undo `polis setup`: remove the hooks, MCP entries, skill and service.
    /// The record stays unless `--purge`.
    Uninstall {
        /// Also delete $POLIS_HOME: the record, its backups and this
        /// device's key. Asks first unless `--yes`.
        #[arg(long)]
        purge: bool,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Install or remove the `polis-memory` skill (how an agent searches,
    /// cites and writes the record) for Claude Code.
    Skill {
        #[command(subcommand)]
        cmd: SkillCmd,
    },
    /// Keep `polis serve` running as a per-user service (launchd on macOS,
    /// systemd on Linux): semantic indexing, filing and backups need it.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
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
        /// Run as an org node (E4): relay signed segments over /v1/sync/*,
        /// publish this node's chain and catalog, keep the union. A
        /// non-loopback bind then needs `--token-file`.
        #[arg(long)]
        org: bool,
        /// Org node: trust a peer's key on first use (the fingerprint is
        /// logged); otherwise unknown keys are refused until `polis trust add`.
        #[arg(long, requires = "org")]
        tofu: bool,
    },
    /// Serve MCP over stdio (what `claude mcp add polis -- polis mcp` runs),
    /// or write a client's config.
    Mcp {
        #[command(subcommand)]
        cmd: Option<McpCmd>,
    },
    /// Install, remove or inspect the capture hooks: UserPromptSubmit (typed
    /// prompts) and Stop (the assistant's replies).
    Hook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// The capture hooks' command: reads the hook payload on stdin and records
    /// it (through the daemon, or locally); always exits 0. `--event prompt`
    /// records the typed prompt; `--event stop` the assistant's reply (Claude
    /// Code: the turns its transcript gained; Codex: its last message);
    /// `--event response` Cursor's finished reply.
    Capture {
        #[arg(long, value_enum, default_value_t = CaptureEvent::Prompt)]
        event: CaptureEvent,
        /// The agent whose hook fired: its payload shape (`claude` = Claude
        /// Code, the default).
        #[arg(long, value_enum, default_value_t = agents::HookClient::Claude)]
        client: agents::HookClient,
    },
    /// Inspect local retrieval traces, source citations, and gardener history.
    Inspect {
        #[arg(long)] id: Option<String>,
        #[arg(long, default_value_t=50)] limit: usize,
        #[arg(long)] seq: Option<i64>,
        /// Write a standalone, offline browser inspector.
        #[arg(long)] html: Option<PathBuf>,
        /// Export portable diagnostic JSON.
        #[arg(long)] export: Option<PathBuf>,
    },
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
    /// Embed the backlog (C2). `--model` names a provider — apple |
    /// model2vec | fastembed | remote, or a row model id — and may fetch its
    /// files on first use (verified against their pinned hashes); `--all`
    /// drains the whole backlog instead of one bounded pass; `--prune`
    /// drops every OTHER model's rows afterwards (by default they stay: a
    /// switch back is then free, at one byte per dimension per chunk).
    Reindex {
        #[arg(long, value_name = "PROVIDER")]
        model: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        prune: bool,
    },
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
        /// An org node's base URL (E4), e.g. http://node.example:7677. Needs
        /// the node's token: `--token-file`, else `POLIS_ORG_TOKEN`.
        #[arg(long, value_name = "URL")]
        org: Option<String>,
        /// The org node's bearer token, in a file.
        #[arg(long, value_name = "FILE", requires = "org")]
        token_file: Option<PathBuf>,
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
    Peers {
        /// E4: clear a chain's FORKED mark (a chain id or fingerprint prefix)
        /// after the operator has looked — the held history stays, newer
        /// segments that link onto it land again. Nothing is deleted.
        #[arg(long, value_name = "CHAIN")]
        unfork: Option<String>,
    },
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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureEvent {
    Prompt,
    Stop,
    Response,
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
enum SkillCmd {
    Install {
        /// Where to write it (default ~/.claude/skills/polis-memory/SKILL.md).
        #[arg(long)]
        path: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Write the service definition and start it.
    Install {
        /// The polis binary the service runs (default: this one).
        #[arg(long)]
        polis: Option<PathBuf>,
        /// Write the definition without loading or starting it.
        #[arg(long)]
        no_start: bool,
    },
    /// Stop the service and remove its definition.
    Uninstall,
    /// Whether a definition is installed, and whether a daemon is answering.
    Status,
}

#[derive(Subcommand)]
enum HookCmd {
    Install {
        /// The agent to capture from: `claude` (Claude Code, the default),
        /// `codex` (~/.codex/hooks.json) or `cursor` (~/.cursor/hooks.json).
        #[arg(long, value_enum, default_value_t = agents::HookClient::Claude)]
        client: agents::HookClient,
        /// The agent's hook file (default: its usual global one).
        #[arg(long)]
        settings: Option<PathBuf>,
        /// The polis binary the hook runs (default: this one).
        #[arg(long)]
        polis: Option<PathBuf>,
        /// Capture typed prompts only: skip (or remove) the Stop hook that
        /// records the assistant's replies.
        #[arg(long)]
        no_replies: bool,
    },
    Uninstall {
        #[arg(long, value_enum, default_value_t = agents::HookClient::Claude)]
        client: agents::HookClient,
        #[arg(long)]
        settings: Option<PathBuf>,
    },
    Status {
        #[arg(long, value_enum, default_value_t = agents::HookClient::Claude)]
        client: agents::HookClient,
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
        Cmd::Init { device, from_redline, org } => {
            let fresh = !home.exists();
            home.ensure()?;
            let wrote_config = home.ensure_config()?;
            if let Some(name) = &org {
                // The node is just another principal: its own key, its own
                // device chain named for the org, its card carrying the name.
                home.config_set("org", name)?;
                if device.is_none() {
                    home.config_set("device", &format!("org:{name}"))?;
                }
            }
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
            // An org node's human card carries the org's name, not a login.
            let login = home.config_get("org").unwrap_or_else(crate::identity::login_name);
            let report = crate::identity::adopt(&store, &identity, &login)?;
            let _ = std::fs::create_dir_all(home.models_dir());
            // Said out loud, not only logged: whether semantic search will
            // work is the first thing a new install wants to know.
            let semantic = backend::ensure_default_model(&home);
            tracing::info!(model = %semantic, "default embedding model");
            tracing::info!(assets = backend::request_apple_assets_if_allowed(), "apple contextual embedding assets");
            emit(
                json,
                &serde_json::json!({
                    "home": home.root, "db": db, "createdHome": fresh, "createdDb": created_db, "wroteConfig": wrote_config, "semantic": semantic,
                    "identity": { "dir": identity_dir, "createdKey": created_key, "principal": identity.principal_id(), "fingerprint": identity.fingerprint(), "device": identity.device_id(), "deviceName": identity.device_name },
                    "adopt": report,
                }),
                || {
                    format!(
                        "home     {}{}\nstore    {}{}\ntoken    {}\nconfig   {}{}\nidentity {} ({}; key {})\n         principal {}  device {} ({})\n         bind #{}{}; aliases +{}, stamped {} rows{}\nsemantic {}\n\nnext: `polis setup` connects your agents and keeps the daemon running (or step by step: `polis hook install`, `polis mcp install --client claude`, `polis service install`).",
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
                        },
                        semantic
                    )
                },
            );
            Ok(())
        }
        Cmd::Setup { yes, dry_run, clients, no_service, no_model, no_skill, polis } => {
            let opts = setup::SetupOptions { yes, dry_run, clients: clients.map(|c| vec![c]), no_service, no_model, no_skill, polis };
            let report = setup::run(&home, &opts)?;
            emit(json, &serde_json::to_value(&report).map_err(|e| e.to_string())?, || setup::render(&report));
            if report.problems.is_empty() { Ok(()) } else { Err(format!("{} step(s) need attention (above)", report.problems.len())) }
        }
        Cmd::Uninstall { purge, yes } => {
            if purge && !yes && !setup::confirm(&format!("Delete {} — the record, its backups and this device's key?", home.root.display())) {
                return Err("uninstall cancelled — nothing changed".into());
            }
            let report = setup::uninstall(&home, purge);
            emit(json, &serde_json::to_value(&report).map_err(|e| e.to_string())?, || setup::render_uninstall(&report, &home));
            if report.problems.is_empty() { Ok(()) } else { Err(format!("{} step(s) failed (above)", report.problems.len())) }
        }
        Cmd::Skill { cmd } => {
            let default = || setup::claude_skill_path().ok_or_else(|| "no skill path (set HOME or pass --path)".to_string());
            match cmd {
                SkillCmd::Install { path } => {
                    let path = path.map(Ok).unwrap_or_else(default)?;
                    let changed = setup::install_skill(&home, &path)?;
                    emit(json, &serde_json::json!({ "path": path, "changed": changed }), || format!("{}: polis-memory skill {}", path.display(), if changed { "installed" } else { "unchanged" }));
                    Ok(())
                }
                SkillCmd::Uninstall { path } => {
                    let path = path.map(Ok).unwrap_or_else(default)?;
                    let removed = setup::uninstall_skill(&path)?;
                    emit(json, &serde_json::json!({ "path": path, "removed": removed }), || format!("{}: polis-memory skill {}", path.display(), if removed { "removed" } else { "not installed" }));
                    Ok(())
                }
            }
        }
        Cmd::Service { cmd } => match cmd {
            ServiceCmd::Install { polis, no_start } => {
                home.ensure()?;
                let r = service::install(&home, polis, no_start)?;
                emit(json, &serde_json::to_value(&r).map_err(|e| e.to_string())?, || {
                    format!("{} service {} · {}", r.manager, if r.started { "installed and started" } else { "written (not started)" }, r.definition.display())
                });
                Ok(())
            }
            ServiceCmd::Uninstall => {
                let r = service::uninstall()?;
                emit(json, &serde_json::to_value(&r).map_err(|e| e.to_string())?, || {
                    format!("{} service removed{} · {}", r.manager, if r.stopped { " (stopped)" } else { "" }, r.definition.display())
                });
                Ok(())
            }
            ServiceCmd::Status => {
                let definition = service::installed();
                let daemon = backend::daemon_alive(&home);
                emit(json, &serde_json::json!({ "definition": definition, "daemon": daemon }), || {
                    format!(
                        "service  {}\ndaemon   {}",
                        definition.as_ref().map(|p| format!("installed · {}", p.display())).unwrap_or_else(|| "not installed (`polis service install`)".into()),
                        daemon.as_deref().unwrap_or("not answering")
                    )
                });
                Ok(())
            }
        },
        Cmd::Serve { listen, token_file, no_gardener, tick, org, tofu } => {
            let rt = runtime()?;
            rt.block_on(serve::run(&home, serve::ServeOptions { listen, token_file, no_gardener, tick_secs: tick, org, tofu }))
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
            // Codex and Cursor: their own hook files, the same two hooks.
            let other = |client: agents::HookClient, s: Option<PathBuf>| {
                s.or_else(|| if client == agents::HookClient::Codex { agents::codex_hooks_path() } else { agents::cursor_hooks_path() })
                    .ok_or_else(|| "no hooks path (set HOME or pass --settings)".to_string())
            };
            match cmd {
                HookCmd::Install { client, settings: s, polis, .. } if client != agents::HookClient::Claude => {
                    let path = other(client, s)?;
                    let polis = polis.unwrap_or_else(home::current_exe);
                    agents::install_at(client, &path, &polis)?;
                    emit(json, &serde_json::json!({ "client": client.name(), "hooks": path, "installed": true }), || {
                        format!(
                            "{}: {} capture hooks installed (prompts and replies){}",
                            path.display(),
                            client.name(),
                            if client == agents::HookClient::Codex { "\nCodex asks you to review new hooks: open `/hooks` in Codex and enable them." } else { "" }
                        )
                    });
                    Ok(())
                }
                HookCmd::Uninstall { client, settings: s } if client != agents::HookClient::Claude => {
                    let path = other(client, s)?;
                    agents::uninstall_at(client, &path)?;
                    emit(json, &serde_json::json!({ "client": client.name(), "hooks": path, "installed": false }), || format!("{}: {} capture hooks removed", path.display(), client.name()));
                    Ok(())
                }
                HookCmd::Status { client, settings: s } if client != agents::HookClient::Claude => {
                    let path = other(client, s)?;
                    let (installed, current) = agents::status_at(client, &path, &home::current_exe());
                    emit(json, &serde_json::json!({ "client": client.name(), "hooks": path, "installed": installed, "current": current }), || {
                        format!("{}: {} capture hooks {}", path.display(), client.name(), if !installed { "not installed" } else if current { "installed" } else { "installed (stale — run `polis hook install` again)" })
                    });
                    Ok(())
                }
                HookCmd::Install { settings: s, polis, no_replies, .. } => {
                    let path = settings(s)?;
                    let spec = doctor::spec_for(&home, polis.clone());
                    let installed = spec.install_at(&path)?;
                    let stop = doctor::stop_spec_for(polis);
                    let replies = if no_replies { stop.uninstall_at(&path)? } else { stop.install_at(&path)? };
                    emit(json, &serde_json::json!({ "settings": path, "installed": installed, "command": spec.command(), "replies": replies, "stopCommand": stop.command() }), || {
                        format!(
                            "{}: capture hook {} → {}\nreply capture (Stop hook) {}",
                            path.display(),
                            if installed { "installed" } else { "NOT installed" },
                            spec.command(),
                            if replies { format!("installed → {}", stop.command()) } else { "off".to_string() }
                        )
                    });
                    Ok(())
                }
                HookCmd::Uninstall { settings: s, .. } => {
                    let path = settings(s)?;
                    let still = hook_spec(&home).uninstall_at(&path)?;
                    let stop_still = doctor::stop_spec_for(None).uninstall_at(&path)?;
                    emit(json, &serde_json::json!({ "settings": path, "installed": still, "replies": stop_still }), || {
                        format!("{}: capture hook {} · reply capture {}", path.display(), if still { "still present" } else { "removed" }, if stop_still { "still present" } else { "removed" })
                    });
                    Ok(())
                }
                HookCmd::Status { settings: s, .. } => {
                    let path = settings(s)?;
                    let spec = hook_spec(&home);
                    let stop = doctor::stop_spec_for(None);
                    let (installed, current) = (spec.installed_at(&path), spec.current_at(&path));
                    let (replies, replies_current) = (stop.installed_at(&path), stop.current_at(&path));
                    emit(json, &serde_json::json!({ "settings": path, "installed": installed, "current": current, "command": spec.command(), "replies": replies, "repliesCurrent": replies_current, "stopCommand": stop.command() }), || {
                        format!(
                            "{}: {}{}\ncommand: {}\nreply capture: {}{}",
                            path.display(),
                            if installed { "installed" } else { "not installed" },
                            if installed && !current { " (stale — run `polis hook install`)" } else { "" },
                            spec.command(),
                            if replies { "installed" } else { "not installed" },
                            if replies && !replies_current { " (stale — run `polis hook install`)" } else { "" }
                        )
                    });
                    Ok(())
                }
            }
        }
        Cmd::Capture { event, client } => {
            capture(&home, client, event);
            Ok(())
        }
        Cmd::Inspect { id, limit, seq, html, export } => {
            let local_diagnostics=cli.remote.is_none() && std::env::var("POLIS_REMOTE").ok().is_none_or(|v|v.trim().is_empty());
            let api=open(&home,cli.remote)?;
            let mut data=inspect::export(api.as_ref(),id,limit.min(200),seq)?;
            if seq.is_none() && local_diagnostics {
                if let Ok(store)=PolisStore::open(&home.db_path()) {
                    let conn=store.conn();
                    let jobs=conn.prepare("SELECT kind,status,attempts,lease_until,error FROM background_jobs ORDER BY updated_at DESC LIMIT 50").and_then(|mut q|q.query_map([],|r|Ok(serde_json::json!({"kind":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"attempts":r.get::<_,i64>(2)?,"deadline":r.get::<_,Option<i64>>(3)?,"error":r.get::<_,Option<String>>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>());
                    data["jobs"]=match jobs {Ok(rows)=>serde_json::json!(rows),Err(e)=>serde_json::json!({"unavailable":e.to_string()})};
                    let usage=conn.query_row("SELECT COUNT(*),COALESCE(SUM(input_tokens),0),COALESCE(SUM(output_tokens),0),COALESCE(SUM(NOT usage_reported),0) FROM model_usage",[],|r|Ok(serde_json::json!({"calls":r.get::<_,i64>(0)?,"inputTokens":r.get::<_,i64>(1)?,"outputTokens":r.get::<_,i64>(2)?,"unknownUsage":r.get::<_,i64>(3)?})));
                    data["usage"]=match usage {Ok(v)=>v,Err(e)=>serde_json::json!({"unavailable":e.to_string()})};
                }
            }
            if let Some(path)=html {inspect::write_html(&path,&data)?;eprintln!("inspector: {}",path.display());}
            if let Some(path)=export {std::fs::write(&path,serde_json::to_vec_pretty(&data).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;}
            emit(json,&data,||serde_json::to_string_pretty(&data).unwrap_or_default());Ok(())
        }
        Cmd::Search { q, node, limit, shared } => {
            let api = open(&home, cli.remote)?;
            let req = SearchRequest { q: words(q), node, limit, scope: Scope { include_shared: shared, ..Default::default() }, ..Default::default() };
            let pack = api.search(&req).map_err(|e| e.to_string())?;
            emit(json, &pack, || render::pack(&pack));
            Ok(())
        }
        Cmd::Context { q, node, max_tokens, shared } => {
            let api = open(&home, cli.remote)?;
            let q = words(q).ok_or("a question is required")?;
            let block = api.context(&ContextRequest { q, node, max_tokens, scope: Scope { include_shared: shared, ..Default::default() }, ..Default::default() }).map_err(|e| e.to_string())?;
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
        Cmd::Reindex { model, all, prune } => {
            if model.is_some() && (cli.remote.is_some() || backend::daemon_alive(&home).is_some()) {
                return Err("`reindex --model` switches this process's provider; stop `polis serve` first (a daemon reindexes under its own)".into());
            }
            let embedder: Option<Arc<dyn polis_embed::Embedder>> = match &model {
                Some(name) => Some(backend::embedder_named(&home, name)?),
                None => None,
            };
            let api: Arc<dyn polis_core::MemoryApi> = match embedder {
                Some(e) => {
                    let db = home.db_path();
                    if !db.exists() {
                        return Err(format!("no store at {} — run `polis init` first", db.display()));
                    }
                    let store = Arc::new(PolisStore::open(&db).map_err(|err| format!("open {}: {err}", db.display()))?);
                    let identity = crate::identity::Identity::load(&home.identity_dir(), home.device_name())?.map(Arc::new);
                    Arc::new(
                        crate::PolisHandle::new(store, backend::agent_for_home(&home), Arc::new(polis_core::host::NoHost), Arc::new(polis_llm::NoopSink))
                            .with_identity(identity)
                            .with_embedder(Some(e)),
                    )
                }
                None => open(&home, cli.remote)?,
            };
            let mut embedded = 0usize;
            let mut passes = 0usize;
            let provider = loop {
                let r = api.reindex(&Scope::default()).map_err(|e| e.to_string())?;
                embedded += r.embedded;
                passes += 1;
                if !all || r.embedded == 0 {
                    break r.provider;
                }
            };
            let mut pruned: Vec<(String, usize)> = Vec::new();
            let index: Vec<(String, i64, i64)> = {
                let db = home.db_path();
                match PolisStore::open(&db) {
                    Ok(store) => {
                        if prune {
                            let keep = model
                                .as_deref()
                                .map(|n| backend::embedder_named(&home, n).map(|e| e.model_id()))
                                .transpose()?;
                            for (m, _, _) in store.models_in_index().unwrap_or_default() {
                                if keep.as_deref().is_some_and(|k| k != m) {
                                    let n = store.delete_embeddings_for_model(&m).unwrap_or(0);
                                    pruned.push((m, n));
                                }
                            }
                        }
                        store.models_in_index().unwrap_or_default()
                    }
                    Err(_) => Vec::new(),
                }
            };
            emit(
                json,
                &serde_json::json!({ "provider": provider, "embedded": embedded, "passes": passes, "pruned": pruned, "index": index.iter().map(|(m, d, n)| serde_json::json!({"model": m, "dim": d, "rows": n})).collect::<Vec<_>>() }),
                || {
                    let mut s = format!("reindex · provider {provider} · embedded {embedded} target(s) in {passes} pass(es)");
                    for (m, d, n) in &index {
                        s.push_str(&format!("\n  {m} · {d}-dim · {n} rows"));
                    }
                    for (m, n) in &pruned {
                        s.push_str(&format!("\n  pruned {m}: {n} rows"));
                    }
                    s
                },
            );
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
        Cmd::Sync { folder, git, org, token_file, publish_only, fetch_only, tofu, force, include_vectors, bodies } => {
            let transport: Box<dyn crate::transport::SegmentTransport> = match (folder, git, org) {
                (Some(dir), None, None) => Box::new(crate::transport::FolderTransport::new(dir)),
                (None, Some(url), None) => Box::new(crate::transport::GitTransport::new(url.clone(), home.sync_dir().join("git").join(short_hash(&url)))),
                (None, None, Some(url)) => {
                    // The node's token, never this home's: `--token-file`,
                    // else POLIS_ORG_TOKEN.
                    let token = match &token_file {
                        Some(p) => Some(std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?.trim().to_string()),
                        None => std::env::var("POLIS_ORG_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
                    };
                    if token.is_none() {
                        return Err("an org node requires its token: pass --token-file FILE or set POLIS_ORG_TOKEN".into());
                    }
                    Box::new(crate::transport::OrgNodeTransport::new(url, token))
                }
                _ => return Err("pass exactly one of --folder DIR, --git URL or --org URL".into()),
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
            let login = home.config_get("org").unwrap_or_else(crate::identity::login_name);
            let rt = runtime()?;
            let report = rt.block_on(crate::sync::sync(&store, &identity, &login, transport.as_ref(), &opts));
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
        Cmd::Peers { unfork } => {
            let db = home.db_path();
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            if let Some(prefix) = unfork {
                let p = prefix.trim().to_ascii_lowercase();
                let chains = store.list_foreign_chains().map_err(|e| e.to_string())?;
                let matches: Vec<_> = chains.iter().filter(|c| c.chain_id == p || c.chain_id.starts_with(&p) || polis_core::identity::fingerprint(&c.chain_id) == p).collect();
                let Some(c) = matches.first() else { return Err(format!("no held chain matches `{prefix}`")) };
                if matches.len() > 1 {
                    return Err(format!("`{prefix}` matches {} chains — give more of the id", matches.len()));
                }
                let cleared = store.clear_foreign_fork(&c.chain_id).map_err(|e| e.to_string())?;
                emit(json, &serde_json::json!({ "chain": c.chain_id, "cleared": cleared }), || {
                    format!("{} ({}) · fork mark {}", polis_core::identity::fingerprint(&c.chain_id), c.display_name.as_deref().unwrap_or("unnamed"), if cleared { "cleared — segments linking onto the held head land again" } else { "was not set" })
                });
                return Ok(());
            }
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

/// The hooks' whole life: never block the agent. Every branch ends in exit
/// 0; anything printed to stdout is for the agent (Claude Code: the daemon's
/// `hookSpecificOutput`, which the model reads as context; Cursor: its
/// required `{"continue": true}`).
fn capture(home: &Home, client: agents::HookClient, event: CaptureEvent) {
    use agents::HookClient;
    use std::io::Read;
    let mut raw = String::new();
    let payload = std::io::stdin()
        .read_to_string(&mut raw)
        .ok()
        .and_then(|_| serde_json::from_str::<serde_json::Value>(&raw).ok());
    match (client, event) {
        (_, CaptureEvent::Prompt) => {
            let answer = payload.as_ref().and_then(|p| capture_prompt_payload(home, &agents::normalize_prompt(client, p)));
            match client {
                // Only Claude Code reads hidden context from this hook.
                HookClient::Claude => {
                    if let Some(answer) = answer {
                        print!("{answer}");
                    }
                }
                HookClient::Cursor => print!("{}", serde_json::json!({ "continue": true })),
                HookClient::Codex => {}
            }
        }
        (HookClient::Claude, CaptureEvent::Stop) => {
            if let Some(p) = &payload {
                if let Err(e) = capture_stop_payload(home, p) {
                    tracing::warn!(error = %e, "capture --event stop: the turns are read again next time");
                }
            }
        }
        (HookClient::Codex, CaptureEvent::Stop) | (HookClient::Cursor, CaptureEvent::Response) => {
            if let Some((session, cwd, text)) = payload.as_ref().and_then(|p| agents::reply_of(client, p)) {
                let item = polis_core::api::IngestItem {
                    body: text.chars().take(crate::transcript::MAX_TURN_CHARS).collect(),
                    ts: None,
                    role: Some("assistant".into()),
                    session: Some(session.clone()),
                    run: Some(session),
                    project: cwd.clone(),
                };
                if let Err(e) = ingest_assistant(home, cwd, vec![vec![item]], client.name()) {
                    tracing::warn!(error = %e, "capture: reply not recorded");
                }
            }
        }
        _ => {}
    }
}

/// Record one normalized prompt payload (`prompt`, `session_id`, `cwd`) and
/// return the hook answer to relay, if any: through the daemon when one is
/// up, else in this process.
pub fn capture_prompt_payload(home: &Home, payload: &serde_json::Value) -> Option<serde_json::Value> {
    let started = std::time::Instant::now();
    let budget = Duration::from_millis(1000);
    let prompt = polis_server::ingest_prompt_text(payload);
    if prompt.is_empty() {
        return None;
    }
    let session = payload.get("session_id").and_then(serde_json::Value::as_str).map(str::to_string);
    let cwd = payload.get("cwd").and_then(serde_json::Value::as_str).map(str::to_string);

    // A daemon: hand it the payload as the hook would, and relay only a
    // body carrying hookSpecificOutput.
    if let Some(info) = home.read_serve() {
        let remaining = budget.saturating_sub(started.elapsed()).max(Duration::from_millis(150));
        let api = polis_mcp::remote::RemoteApi::new(info.base_url(), home.read_token()).with_timeout(remaining);
        match api.capture_raw(payload) {
            Ok(body) => return body.get("hookSpecificOutput").is_some().then_some(body),
            Err(e) => tracing::debug!(error = %e, "daemon capture failed; recording locally"),
        }
    }
    // No daemon (or it did not answer in time): the store, here.
    let db = home.db_path();
    if !db.exists() {
        tracing::warn!(db = %db.display(), "capture: no store — run `polis setup`");
        return None;
    }
    let arrived = polis_core::ledger::now_millis();
    let handle = match backend::open_local(home) {
        Ok(handle) => handle,
        Err(e) => {
            tracing::warn!(error = %e, "capture: could not open the store");
            return None;
        }
    };
    let req = CaptureRequest { body: prompt.clone(), origin: Origin::External, surface: "external".into(), session: session.clone(), project: cwd.clone() };
    if let Err(e) = handle.capture(&req) {
        tracing::warn!(error = %e, "capture: local record failed");
    }
    // The daemon's `annotate`, answered here when there is no daemon.
    if !home.inject_enabled() {
        return None;
    }
    let site = crate::inject::PromptSite { session: session.as_deref().filter(|s| !s.is_empty()), project: cwd.as_deref(), before: Some(arrived) };
    crate::inject::answer_prompt(&handle, &prompt, &site, &Default::default())
}

/// Store assistant items (in request-sized batches) as the named agent,
/// through the daemon when one is up, else in this process.
fn ingest_assistant(home: &Home, cwd: Option<String>, batches: Vec<Vec<polis_core::api::IngestItem>>, agent: &str) -> Result<(), String> {
    if batches.iter().all(Vec::is_empty) {
        return Ok(());
    }
    let api: Arc<dyn MemoryApi> = match home.read_serve() {
        Some(info) => Arc::new(polis_mcp::remote::RemoteApi::new(info.base_url(), home.read_token()).with_timeout(Duration::from_secs(5))),
        None => backend::open_local(home)?,
    };
    let agent = if agent == "claude" { "claude-code" } else { agent };
    let scope = Scope { agent: Some(agent.into()), project: cwd, ..Scope::default() };
    for items in batches.into_iter().filter(|b| !b.is_empty()) {
        api.ingest(&polis_core::api::IngestRequest { items, scope: scope.clone() }).map_err(|e| format!("ingest: {e}"))?;
    }
    Ok(())
}

/// Read at most this much new transcript per Stop fire; the rest is read on
/// the next one.
const STOP_READ_LIMIT: u64 = 4 * 1024 * 1024;
/// Keep each ingest request well under the server's 2 MB body limit.
const STOP_BATCH_BYTES: usize = 512 * 1024;

/// Where a session's transcript was last read up to.
fn transcript_offset_path(home: &Home, session: &str) -> PathBuf {
    home.root.join("transcripts").join(format!("{}.offset", short_hash(session)))
}

/// Claude Code's Stop hook: the assistant turns written since the last
/// fire, from the transcript the payload names, recorded as `role=assistant`
/// (scrubbed, and minus echoes of injected memory, by `ingest`). Returns how
/// many turns were sent; the transcript offset moves only once every batch
/// was stored, so a failed fire is read again next time.
pub fn capture_stop_payload(home: &Home, payload: &serde_json::Value) -> Result<usize, String> {
    use std::io::{Read, Seek, SeekFrom};
    let field = |k: &str| payload.get(k).and_then(serde_json::Value::as_str).map(str::to_string).filter(|s| !s.is_empty());
    let (Some(session), Some(path)) = (field("session_id"), field("transcript_path")) else { return Ok(0) };
    let cwd = field("cwd");
    let offset_path = transcript_offset_path(home, &session);
    let mut offset = std::fs::read_to_string(&offset_path).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let mut file = std::fs::File::open(&path).map_err(|e| format!("open {path}: {e}"))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if offset > len {
        offset = 0; // the transcript was replaced; read it again (ingest is idempotent)
    }
    let mut chunk = Vec::new();
    file.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    file.take(STOP_READ_LIMIT).read_to_end(&mut chunk).map_err(|e| e.to_string())?;
    let complete = crate::transcript::complete_prefix(&chunk);
    if complete == 0 {
        return Ok(0);
    }
    let text = String::from_utf8_lossy(&chunk[..complete]);
    let items: Vec<polis_core::api::IngestItem> = crate::transcript::assistant_turns(&text, cwd.as_deref())
        .into_iter()
        .map(|t| polis_core::api::IngestItem {
            body: t.body,
            ts: t.ts,
            role: Some("assistant".into()),
            session: Some(session.clone()),
            run: Some(session.clone()),
            project: cwd.clone(),
        })
        .collect();
    let sent = items.len();
    let mut batches: Vec<Vec<polis_core::api::IngestItem>> = vec![Vec::new()];
    let mut size = 0usize;
    for item in items {
        if size + item.body.len() > STOP_BATCH_BYTES && !batches.last().is_some_and(Vec::is_empty) {
            batches.push(Vec::new());
            size = 0;
        }
        size += item.body.len();
        batches.last_mut().expect("one batch").push(item);
    }
    ingest_assistant(home, cwd, batches, "claude")?;
    if let Some(parent) = offset_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    home::write_private(&offset_path, (offset + complete as u64).to_string().as_bytes())?;
    Ok(sent)
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

#[cfg(test)]
mod stop_tests {
    use super::*;

    fn temp_home(tag: &str) -> Home {
        let root = std::env::temp_dir().join(format!("polis-stop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        PolisStore::open(&root.join("polis.db")).unwrap();
        Home { root }
    }

    fn turn(user: &str, reply: &str) -> String {
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"user","message":{"role":"user","content":user}}),
            serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":reply}]},"timestamp":"2026-10-07T05:29:01.000Z"})
        )
    }

    fn assistant_bodies(home: &Home) -> Vec<String> {
        let store = PolisStore::open(&home.db_path()).unwrap();
        let conn = store.conn();
        let mut stmt = conn.prepare("SELECT body FROM prompts WHERE role = 'assistant' ORDER BY id").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<Vec<String>>>().unwrap()
    }

    /// Each fire records only what the transcript gained since the last
    /// one; a replayed fire records nothing; a replaced transcript is read
    /// again without duplicating what was stored.
    #[test]
    fn stop_capture_reads_each_turn_once_and_scrubs_it() {
        let home = temp_home("once");
        let transcript = home.root.join("t.jsonl");
        std::fs::write(&transcript, turn("which port?", "Use 7677 for the standalone daemon.")).unwrap();
        let payload = serde_json::json!({"session_id":"s1","transcript_path":transcript,"cwd":"/repo","hook_event_name":"Stop"});
        assert_eq!(capture_stop_payload(&home, &payload).unwrap(), 1);
        assert_eq!(capture_stop_payload(&home, &payload).unwrap(), 0, "nothing new since the offset");

        let mut more = std::fs::read_to_string(&transcript).unwrap();
        more.push_str(&turn("and the key?", "Set OPENAI_API_KEY=sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123 in your shell."));
        std::fs::write(&transcript, &more).unwrap();
        assert_eq!(capture_stop_payload(&home, &payload).unwrap(), 1);

        let bodies = assistant_bodies(&home);
        assert_eq!(bodies, vec!["Use 7677 for the standalone daemon.".to_string(), "Set OPENAI_API_KEY=[redacted:openai_key] in your shell.".to_string()]);

        // A shorter (replaced) transcript restarts from the top; ingest's
        // idempotency keeps the store unchanged.
        std::fs::write(&transcript, turn("which port?", "Use 7677 for the standalone daemon.")).unwrap();
        assert_eq!(capture_stop_payload(&home, &payload).unwrap(), 1);
        assert_eq!(assistant_bodies(&home).len(), 2);
        let _ = std::fs::remove_dir_all(&home.root);
    }
}
