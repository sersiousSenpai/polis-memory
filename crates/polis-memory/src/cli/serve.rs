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
use polis_llm::NoopSink;
use polis_mcp::remote::RemoteApi;
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
/// carries the holder's pid and address; a holder that no longer answers
/// its own health route is stale and the lock is taken over.
pub struct GardenerLock {
    path: PathBuf,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LockInfo {
    pid: u32,
    addr: String,
}

impl GardenerLock {
    pub fn acquire(path: &Path, addr: &str) -> Result<GardenerLock, String> {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(info) = serde_json::from_str::<LockInfo>(&text) {
                let alive = std::process::id() != info.pid && RemoteApi::probe(&format!("http://{}", info.addr), Duration::from_millis(500)).is_some();
                if alive {
                    return Err(format!("the gardener is already running (pid {} at {}) — {}", info.pid, info.addr, path.display()));
                }
                tracing::info!(pid = info.pid, addr = %info.addr, "taking over a stale gardener lock");
            }
        }
        let info = LockInfo { pid: std::process::id(), addr: addr.to_string() };
        super::home::write_private(path, serde_json::to_string(&info).map_err(|e| e.to_string())?.as_bytes())?;
        Ok(GardenerLock { path: path.to_path_buf() })
    }
}

impl Drop for GardenerLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct ServeOptions {
    pub listen: Option<String>,
    pub token_file: Option<PathBuf>,
    pub no_gardener: bool,
    /// The gardener tick, seconds (default 30).
    pub tick_secs: u64,
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
    if let Some(existing) = super::backend::daemon_alive(home) {
        return Err(format!("a daemon is already serving this home at {existing}"));
    }

    let db = home.db_path();
    let store = Arc::new(PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?);
    let activity = Arc::new(Activity::default());
    // C1: the model transport from the environment (an API key, else a
    // CLI on PATH, else none) and this platform's on-device embedder.
    let handle = Arc::new(
        PolisHandle::new(store.clone(), super::backend::agent_for(), Arc::new(NoHost), Arc::new(NoopSink))
            .with_embedder(super::backend::embedder_for(home)),
    );
    tracing::info!(assets = super::backend::request_apple_assets_if_allowed(), "apple contextual embedding assets");
    let api: Arc<dyn MemoryApi> = handle.clone();
    let state = PolisState { api: api.clone(), ingest: activity.clone(), events: Arc::new(LogEvents) };

    // Startup backup — verified, pruned.
    let backups = home.backups_dir();
    let policy = backup::BackupPolicy::default();
    match backup::backup_verify_prune(&store, &backups, policy.keep) {
        Ok(r) => tracing::info!(path = %r.path.display(), verified = r.verdict.ok, "startup snapshot"),
        Err(e) => tracing::warn!(error = %e, "startup snapshot failed"),
    }

    let router = app(state, auth).nest_service("/mcp", polis_mcp::http_service(api.clone()));
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = listener.local_addr().map_err(|e| e.to_string())?;
    let token_path = match &opts.token_file {
        Some(p) => p.to_string_lossy().to_string(),
        None => home.token_path().to_string_lossy().to_string(),
    };
    home.write_serve(&ServeInfo { pid: std::process::id(), addr: bound.to_string(), token_path, started_at: now_millis() })?;
    tracing::info!(%bound, "polis serve listening (HTTP /v1/*, MCP at /mcp)");
    eprintln!("polis serve · http://{bound} · MCP at http://{bound}/mcp · home {}", home.root.display());

    // The gardener, if this process may run it.
    let lock = if opts.no_gardener {
        None
    } else {
        match GardenerLock::acquire(&home.lock_path(), &bound.to_string()) {
            Ok(l) => Some(l),
            Err(e) => {
                tracing::warn!(error = %e, "gardener not started");
                None
            }
        }
    };
    let gardener_task = lock.is_some().then(|| {
        let handle = handle.clone();
        let activity = activity.clone();
        let cfg = GardenerConfig { backup_dir: Some(backups.clone()), backup_every_ms: policy.every_ms, backup_keep: policy.keep, ..GardenerConfig::default() };
        let tick = Duration::from_secs(opts.tick_secs.max(1));
        tokio::spawn(async move {
            let mut state = GardenerState { last_backup_ms: Some(now_millis()), ..Default::default() };
            loop {
                tokio::time::sleep(tick).await;
                let out = gardener::step(&handle.view(), &mut state, &*activity, &SystemClock, &cfg, &LogEvents).await;
                tracing::debug!(gate = ?out.gate, organized = out.organized, compacted = out.compacted, backed_up = ?out.backed_up, "gardener tick");
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
    drop(lock);
    match backup::backup_verify_prune(&store, &backups, policy.keep) {
        Ok(r) => tracing::info!(path = %r.path.display(), verified = r.verdict.ok, "shutdown snapshot"),
        Err(e) => tracing::warn!(error = %e, "shutdown snapshot failed"),
    }
    home.remove_serve();
    served.map_err(|e| format!("serve: {e}"))
}
