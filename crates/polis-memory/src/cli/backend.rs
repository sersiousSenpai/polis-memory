// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Which memory a command talks to (§4.7): a running daemon (Remote — all
//! writes go through the one long-lived process) or the store file opened
//! in this process (Local). Chosen once at start: `--remote` / `POLIS_REMOTE`
//! force a daemon; else a live `serve.json` means Remote; else Local.

use std::sync::Arc;
use std::time::Duration;

use polis_core::host::NoHost;
use polis_core::MemoryApi;
use polis_llm::NoopSink;
use polis_mcp::remote::RemoteApi;
use polis_store::PolisStore;

use super::home::Home;
use crate::PolisHandle;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// The store file, opened here.
    Local,
    /// A daemon at this base URL.
    Remote { base: String },
}

impl Backend {
    pub fn describe(&self) -> String {
        match self {
            Backend::Local => "local (the store file, opened in this process)".into(),
            Backend::Remote { base } => format!("remote ({base})"),
        }
    }
}

/// How long the liveness probe waits for `serve.json`'s daemon.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(800);

/// Is the daemon `serve.json` names actually answering?
pub fn daemon_alive(home: &Home) -> Option<String> {
    let info = home.read_serve()?;
    let base = info.base_url();
    RemoteApi::probe(&base, PROBE_TIMEOUT).map(|_| base)
}

pub fn choose(home: &Home, remote: Option<String>) -> Backend {
    if let Some(base) = remote.or_else(|| std::env::var("POLIS_REMOTE").ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Backend::Remote { base: normalize(&base) };
    }
    match daemon_alive(home) {
        Some(base) => Backend::Remote { base },
        None => Backend::Local,
    }
}

fn normalize(base: &str) -> String {
    let b = base.trim_end_matches('/');
    if b.starts_with("http://") || b.starts_with("https://") {
        b.to_string()
    } else {
        format!("http://{b}")
    }
}

/// Open the chosen backend as the one surface.
pub fn open(home: &Home, backend: &Backend) -> Result<Arc<dyn MemoryApi>, String> {
    match backend {
        Backend::Local => {
            let db = home.db_path();
            if !db.exists() {
                return Err(format!("no store at {} — run `polis init` first", db.display()));
            }
            let store = PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?;
            Ok(Arc::new(PolisHandle::new(Arc::new(store), None, Arc::new(NoHost), Arc::new(NoopSink))))
        }
        Backend::Remote { base } => Ok(Arc::new(RemoteApi::new(base.clone(), home.read_token()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_remote_wins_and_a_bare_host_gets_a_scheme() {
        let home = Home { root: std::env::temp_dir().join("polis-nowhere") };
        assert_eq!(choose(&home, Some("127.0.0.1:7676/".into())), Backend::Remote { base: "http://127.0.0.1:7676".into() });
        assert_eq!(choose(&home, Some("https://x.test".into())), Backend::Remote { base: "https://x.test".into() });
        // No serve.json anywhere → local.
        assert_eq!(choose(&home, None), Backend::Local);
    }
}
