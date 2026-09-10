// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis serve` — the standalone daemon (§4.7): polis-server's router under
//! its token guard, the MCP streamable-HTTP service at `/mcp`, the gardener
//! loop under `gardener.lock`, and the backup cadence (startup, every 6 h,
//! shutdown; verify after each; keep 7). `serve.json` tells `polis mcp` and
//! `polis capture` where it is, and goes away with the process.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use polis_core::host::{Change, GardenerEvents, IdleSignal, IngestContext, IngestObserver, NoHost, SystemClock};
use polis_core::ledger::now_millis;
use polis_core::MemoryApi;
#[cfg(test)]
use polis_llm::NoopSink;
use polis_server::standalone::{app, check_bind, StandaloneAuth};
use polis_server::PolisState;
use polis_store::PolisStore;

use super::home::{Home, ServeInfo};
use crate::backup;
use crate::gardener::{self, GardenerConfig, GardenerState};
use crate::PolisHandle;

/// The daemon's own idle signal: the last capture it recorded (there is no
/// terminal to watch), so the gardener runs when the lake goes quiet.
#[derive(Default)]
pub struct Activity(AtomicI64);

impl Activity {
    fn touch(&self) {
        self.0.store(now_millis(), Ordering::Relaxed);
    }
}

impl IdleSignal for Activity {
    fn last_activity_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl IngestObserver for Activity {
    fn on_recorded(&self, _cx: &IngestContext<'_>, _seq: Option<i64>) {
        self.touch();
    }
}

/// Logs a write's "something changed" — the daemon has no UI bus.
struct LogEvents;
impl GardenerEvents for LogEvents {
    fn changed(&self, what: &[Change]) {
        tracing::debug!(?what, "memory changed");
    }
}

/// `gardener.lock`: one process runs the gardener over a store. The file
/// carries diagnostic pid/address metadata. Ownership is an exclusive SQLite
/// file lock on a separate sidecar, released by the OS even after a crash.
pub struct GardenerLock {
    path: PathBuf,
    _ownership: std::sync::Mutex<rusqlite::Connection>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LockInfo {
    pid: u32,
    addr: String,
}

impl GardenerLock {
    pub fn acquire(path: &Path, addr: &str) -> Result<GardenerLock, String> {
        let ownership = rusqlite::Connection::open(path.with_extension("owner.sqlite3")).map_err(|e| format!("open gardener ownership: {e}"))?;
        ownership.execute_batch("PRAGMA busy_timeout = 0; BEGIN EXCLUSIVE")
            .map_err(|e| format!("the gardener is already owned — {}: {e}", path.display()))?;
        let info = LockInfo { pid: std::process::id(), addr: addr.to_string() };
        super::home::write_private(path, serde_json::to_string(&info).map_err(|e| e.to_string())?.as_bytes())?;
        Ok(GardenerLock { path: path.to_path_buf(), _ownership: std::sync::Mutex::new(ownership) })
    }
}

impl Drop for GardenerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// E4: an org node on a network needs a token an operator chose and
/// distributed — the home's auto-created loopback token is not that. The
/// plain daemon's rule (`check_bind`) still applies underneath.
pub fn check_org_bind(org: bool, addr: &SocketAddr, token_file: Option<&Path>) -> Result<(), String> {
    if org && !addr.ip().is_loopback() && token_file.is_none() {
        return Err(format!(
            "refusing to serve an org node on {addr} without `--token-file`: the home's own token is for loopback callers; distribute one to the peers and pass it here"
        ));
    }
    Ok(())
}

pub struct ServeOptions {
    pub listen: Option<String>,
    pub token_file: Option<PathBuf>,
    pub no_gardener: bool,
    /// The gardener tick, seconds (default 30).
    pub tick_secs: u64,
    /// E4: run as an org node — relay signed segments over `/v1/sync/*`,
    /// publish this node's own chain (its catalog rides), keep the union.
    pub org: bool,
    /// E4: trust a peer's key on first use (the fingerprint is logged);
    /// otherwise an unknown key is refused until `polis trust add`.
    pub tofu: bool,
}

/// Run until ctrl-c / SIGTERM.
pub async fn run(home: &Home, opts: ServeOptions) -> Result<(), String> {
    home.ensure()?;
    let addr: SocketAddr = opts
        .listen
        .clone()
        .unwrap_or_else(|| home.listen())
        .parse()
        .map_err(|e| format!("listen address: {e}"))?;
    let token = match &opts.token_file {
        Some(p) => Some(std::fs::read_to_string(p).map_err(|e| format!("read {}: {e}", p.display()))?.trim().to_string()).filter(|t| !t.is_empty()),
        None => Some(home.ensure_token()?),
    };
    let auth = StandaloneAuth { token };
    check_bind(&addr, &auth)?;
    check_org_bind(opts.org, &addr, opts.token_file.as_deref())?;
    if let Some(existing) = super::backend::daemon_alive(home) {
        return Err(format!("a daemon is already serving this home at {existing}"));
    }

    let db = home.db_path();
    let store = Arc::new(PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?);
    let identity = match crate::identity::Identity::load(&home.identity_dir(), home.device_name())? {
        Some(identity) => {
            crate::identity::adopt(&store, &identity, &crate::identity::login_name())?;
            Some(Arc::new(identity))
        }
        None => None,
    };
    let activity = Arc::new(Activity::default());
    // C1: the model transport from the environment (an API key, else a
    // CLI on PATH, else none) and this platform's on-device embedder.
    tracing::info!(model = %super::backend::ensure_default_model(home), "default embedding model");
    let handle = Arc::new(
        PolisHandle::new(store.clone(), super::backend::agent_for(), Arc::new(NoHost), Arc::new(crate::usage::LocalUsageSink(store.clone())))
            .with_identity(identity)
            .with_embedder(super::backend::embedder_for(home)),
    );
    tracing::info!(assets = super::backend::request_apple_assets_if_allowed(), "apple contextual embedding assets");
    let api: Arc<dyn MemoryApi> = handle.clone();
    // E4: the org node — its identity is the org's principal; its segments
    // live under the home's sync dir; every received segment is imported
    // into this store, which is the union its gardener works over.
    let org_node = if opts.org {
        let identity = crate::identity::Identity::load(&home.identity_dir(), home.device_name())?
            .ok_or_else(|| "an org node needs an identity — run `polis init --org <name>` first".to_string())?;
        let display = home.config_get("org").unwrap_or_else(|| identity.device_name.clone());
        let node = Arc::new(crate::orgnode::OrgNode::new(store.clone(), Arc::new(identity), display, home.sync_dir().join("org"), opts.tofu));
        std::fs::create_dir_all(&node.segments_dir).map_err(|e| format!("create {}: {e}", node.segments_dir.display()))?;
        match node.publish_own() {
            Ok(Some(r)) => tracing::info!(from = r.from_seq, to = r.to_seq, "org node published its own chain"),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "org node could not publish its own chain"),
        }
        Some(node)
    } else {
        None
    };
    let relay: Arc<dyn polis_core::sync::SyncRelay> = match &org_node {
        Some(n) => n.clone(),
        None => Arc::new(polis_core::sync::NoSyncRelay),
    };
    let state = PolisState { api: api.clone(), ingest: activity.clone(), events: Arc::new(LogEvents), sync: relay };

    // Startup backup — verified, pruned.
    let backups = home.backups_dir();
    let policy = backup::BackupPolicy::default();
    match backup::backup_verify_prune(&store, &backups, policy.keep) {
        Ok(r) => tracing::info!(path = %r.path.display(), verified = r.verdict.ok, "startup snapshot"),
        Err(e) => tracing::warn!(error = %e, "startup snapshot failed"),
    }

    let router = polis_server::standalone::guard(
        app(state, auth.clone()).nest_service("/mcp", polis_mcp::http_service(api.clone())), auth);
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let token_path = match &opts.token_file {
        Some(p) => p.to_string_lossy().to_string(),
        None => home.token_path().to_string_lossy().to_string(),
    };
    home.write_serve(&ServeInfo { pid: std::process::id(), addr: bound.to_string(), token_path, started_at: now_millis() })?;
    tracing::info!(%bound, org = opts.org, "polis serve listening (HTTP /v1/*, MCP at /mcp)");
    eprintln!(
        "polis serve{} · http://{bound} · MCP at http://{bound}/mcp{} · home {}",
        if opts.org { " --org" } else { "" },
        if opts.org { " · relay at /v1/sync/* (token required)" } else { "" },
        home.root.display()
    );

    // The gardener, if this process may run it.
    let lock = if opts.no_gardener {
        None
    } else {
        match GardenerLock::acquire(&home.lock_path(), &bound.to_string()) {
            Ok(l) => Some(Arc::new(l)),
            Err(e) => {
                tracing::warn!(error = %e, "gardener not started");
                None
            }
        }
    };
    let gardener_task = lock.is_some().then(|| {
        let handle = handle.clone();
        let activity = activity.clone();
        let cfg = GardenerConfig { backup_dir: Some(backups.clone()), backup_every_ms: policy.every_ms, backup_keep: policy.keep,
            embed_every_ms: i64::MAX, ..GardenerConfig::default() };
        let tick = Duration::from_secs(opts.tick_secs.max(1));
        let org_node = org_node.clone();
        tokio::spawn(async move {
            let mut state = GardenerState { last_backup_ms: Some(now_millis()), last_embed_ms: Some(now_millis()), ..Default::default() };
            loop {
                tokio::time::sleep(tick).await;
                if let Some(identity) = handle.identity.as_ref() {
                    if let Err(error) = crate::sharing::flush_redactions(&handle.store, &identity.device_id(), handle.store.author()) {
                        tracing::warn!(%error, "pending redaction retry failed");
                    }
                }
                let out = gardener::step(&handle.view(), &mut state, &*activity, &SystemClock, &cfg, &LogEvents).await;
                tracing::debug!(gate = ?out.gate, organized = out.organized, compacted = out.compacted, backed_up = ?out.backed_up, "gardener tick");
                // E4: whatever the node's own chain grew by (a run's events,
                // its catalog) is published for the peers — one meta read
                // when nothing moved.
                if let Some(node) = &org_node {
                    match node.publish_own() {
                        Ok(Some(r)) => tracing::info!(from = r.from_seq, to = r.to_seq, "org node published its own chain"),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(error = %e, "org node could not publish its own chain"),
                    }
                }
            }
        })
    });

    // Indexing has its own worker and a two-second cadence. A slow model
    // consolidation can never hold this loop behind its idle gate or await.
    let indexing_task = lock.is_some().then(|| {
        let handle = handle.clone();
        let ownership = lock.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let worker = handle.clone();
                let owner = ownership.clone();
                let result = tokio::task::spawn_blocking(move || {
                    // Keep process ownership until a synchronous provider has
                    // actually returned, including during daemon shutdown.
                    let _owner = owner;
                    crate::index_tick(&worker.view(), gardener::EMBED_BATCH)
                }).await;
                if let Err(error) = result { tracing::warn!(%error, "indexing worker interrupted"); }
            }
        })
    });
    let filing_task = lock.is_some().then(|| {
        let handle = handle.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if let Err(error) = gardener::basic_filing_once(&handle.view()).await {
                    tracing::warn!(%error, "basic filing worker failed; durable job will retry");
                }
            }
        })
    });

    let shutdown = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
    };
    let served = axum::serve(listener, router).with_graceful_shutdown(shutdown).await;

    // Shutdown: stop the gardener, one more snapshot, forget the address.
    if let Some(t) = gardener_task {
        t.abort();
    }
    if let Some(t) = indexing_task { t.abort(); }
    if let Some(t) = filing_task { t.abort(); }
    drop(lock);
    match backup::backup_verify_prune(&store, &backups, policy.keep) {
        Ok(r) => tracing::info!(path = %r.path.display(), verified = r.verdict.ok, "shutdown snapshot"),
        Err(e) => tracing::warn!(error = %e, "shutdown snapshot failed"),
    }
    home.remove_serve();
    served.map_err(|e| format!("serve: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_is_atomic_before_health_is_available_and_releases() {
        let dir = std::env::temp_dir().join(format!("polis-gardener-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gardener.lock");
        let owner = GardenerLock::acquire(&path, "127.0.0.1:1").unwrap();
        assert!(GardenerLock::acquire(&path, "127.0.0.1:2").is_err(), "health is intentionally unavailable, but ownership is exclusive");
        drop(owner);
        let replacement = GardenerLock::acquire(&path, "127.0.0.1:2").unwrap();
        drop(replacement);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn assembled_mcp_service_requires_the_http_bearer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let api: Arc<dyn MemoryApi> = Arc::new(PolisHandle::new(Arc::new(PolisStore::open_in_memory().unwrap()), None, Arc::new(NoHost), Arc::new(NoopSink)));
        let state = PolisState { api: api.clone(), ingest: Arc::new(Activity::default()), events: Arc::new(LogEvents), sync: Arc::new(polis_core::sync::NoSyncRelay) };
        let auth = StandaloneAuth { token: Some("test-secret".into()) };
        let router = polis_server::standalone::guard(app(state, auth.clone()).nest_service("/mcp", polis_mcp::http_service(api)), auth);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
        for bearer in ["", "Authorization: Bearer wrong\r\n", "Authorization: Bearer test-secret\r\n"] {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let request = format!("POST /mcp HTTP/1.1\r\nHost: {addr}\r\n{bearer}Content-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            stream.write_all(request.as_bytes()).await.unwrap();
            let mut response = Vec::new();
            tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response)).await.unwrap().unwrap();
            let response = String::from_utf8_lossy(&response);
            if bearer.contains("test-secret") { assert!(response.starts_with("HTTP/1.1 200"), "{response}"); }
            else { assert!(response.starts_with("HTTP/1.1 401"), "{response}"); }
        }
        server.abort();
    }
}
