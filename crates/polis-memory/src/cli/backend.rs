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
use polis_embed::Embedder;
use polis_llm::{Agent, NoopSink};
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
            let store = Arc::new(PolisStore::open(&db).map_err(|e| format!("open {}: {e}", db.display()))?);
            // The identity, when `polis init` has made one: adoption is
            // idempotent and cheap, so every open re-runs it — a store that
            // grew new author strings gets their aliases without a command.
            let identity = match crate::identity::Identity::load(&home.identity_dir(), home.device_name())? {
                Some(id) => {
                    if let Err(e) = crate::identity::adopt(&store, &id, &crate::identity::login_name()) {
                        tracing::warn!(error = %e, "adoption on open failed");
                    }
                    Some(Arc::new(id))
                }
                None => None,
            };
            Ok(Arc::new(
                PolisHandle::new(store, agent_for(), Arc::new(NoHost), Arc::new(NoopSink)).with_identity(identity).with_embedder(embedder_for(home)),
            ))
        }
        Backend::Remote { base } => Ok(Arc::new(RemoteApi::new(base.clone(), home.read_token()))),
    }
}

/// The model a standalone `polis` speaks to (plan §4.7, C1's transport
/// default), decided from the environment, in this order:
///
/// 1. `POLIS_NO_NETWORK=1` → never an HTTP backend (the CLI backends only).
/// 2. `ANTHROPIC_API_KEY` → the Anthropic API (`POLIS_MODEL` overrides the
///    model): no process spawn, no 5–10 s CLI startup per pass.
/// 3. `OPENAI_BASE_URL` + `OPENAI_API_KEY` (+ `OPENAI_MODEL`) → an
///    OpenAI-compatible endpoint.
/// 4. `claude` on PATH → the Claude Code CLI; else `codex` → the Codex CLI.
/// 5. Nothing → `None`: the no-model state. Capture, retrieval and filing
///    still work (R12); ambiguous items wait in `~inbox`.
pub fn agent_for() -> Option<Arc<dyn Agent>> {
    let no_network = std::env::var("POLIS_NO_NETWORK").map(|v| v == "1").unwrap_or(false);
    let nonempty = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    if !no_network {
        if let Some(api) = polis_llm::anthropic::AnthropicApi::from_env() {
            let api = match nonempty("POLIS_MODEL") {
                Some(m) => api.model(m),
                None => api,
            };
            tracing::info!(backend = "anthropic", "model transport");
            return Some(Arc::new(api));
        }
        if let (Some(base), Some(key)) = (nonempty("OPENAI_BASE_URL"), nonempty("OPENAI_API_KEY")) {
            let model = nonempty("OPENAI_MODEL").or_else(|| nonempty("POLIS_MODEL")).unwrap_or_else(|| "gpt-5".to_string());
            tracing::info!(backend = "openai-compat", %base, %model, "model transport");
            return Some(Arc::new(polis_llm::openai_compat::OpenAiCompat::new(base, model).api_key(key)));
        }
    }
    if let Some(bin) = find_on_path("claude") {
        tracing::info!(backend = "claude-cli", bin = %bin.display(), "model transport");
        return Some(Arc::new(polis_llm::claude_cli::ClaudeCli::new(bin.to_string_lossy().to_string())));
    }
    if let Some(bin) = find_on_path("codex") {
        tracing::info!(backend = "codex-cli", bin = %bin.display(), "model transport");
        return Some(Arc::new(polis_llm::codex_cli::CodexCli::new(bin.to_string_lossy().to_string())));
    }
    tracing::info!(backend = "none", "no model configured (R12: capture, retrieval and filing still work)");
    None
}

/// The embedder the deterministic filer and the semantic arm use (C2):
/// `POLIS_EMBED` when set (`apple` | `model2vec` | `fastembed` | `remote` |
/// `none`), else the platform default `polis_embed::select` decides —
/// never a download on this path (a read must not start one; `polis
/// reindex --model …` is where a first-use fetch happens). An explicit
/// choice that cannot be met is logged and the arm is absent.
pub fn embedder_for(home: &Home) -> Option<Arc<dyn Embedder>> {
    let choice = polis_embed::ProviderChoice::from_env();
    match polis_embed::select(choice, Some(&home.models_dir()), false) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(choice = choice.as_str(), error = %err, "embedding provider unavailable — the semantic arm is absent");
            None
        }
    }
}

/// An explicitly named provider for `polis reindex --model`, allowed to
/// fetch its files on first use. Accepts a choice name (`model2vec`) or a
/// row model id (`model2vec/potion-base-8M`, `apple-contextual-en-r1`).
pub fn embedder_named(home: &Home, name: &str) -> Result<Arc<dyn Embedder>, String> {
    let choice = polis_embed::ProviderChoice::parse(name)
        .or_else(|| match polis_embed::ProviderKind::of_model_id(name) {
            polis_embed::ProviderKind::AppleContextual | polis_embed::ProviderKind::AppleSentence => Some(polis_embed::ProviderChoice::Apple),
            polis_embed::ProviderKind::Model2Vec => Some(polis_embed::ProviderChoice::Model2Vec),
            polis_embed::ProviderKind::FastEmbed => Some(polis_embed::ProviderChoice::FastEmbed),
            polis_embed::ProviderKind::Remote => Some(polis_embed::ProviderChoice::Remote),
            _ => None,
        })
        .ok_or_else(|| format!("unknown provider `{name}` (apple | model2vec | fastembed | remote | none, or a row model id)"))?;
    if choice == polis_embed::ProviderChoice::None {
        return Err("`none` is not a provider to reindex under".into());
    }
    polis_embed::select(choice, Some(&home.models_dir()), true)?.ok_or_else(|| format!("provider `{name}` is not available on this machine"))
}

/// Ask the OS for Apple's contextual embedding assets once (C2): a real
/// action with a network cost, so only `init` and `serve` call it, and
/// never under `POLIS_NO_NETWORK=1`. Returns what was done, for the log.
pub fn request_apple_assets_if_allowed() -> &'static str {
    if std::env::var("POLIS_NO_NETWORK").map(|v| v == "1").unwrap_or(false) {
        return "skipped (POLIS_NO_NETWORK)";
    }
    match polis_embed::request_apple_assets() {
        Ok(true) => "requested",
        Ok(false) => "present or not offered",
        Err(e) => {
            tracing::warn!(error = %e, "Apple contextual assets request failed");
            "failed"
        }
    }
}

/// `which`, without a dependency: the first executable named `name` on PATH.
pub fn find_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if candidate.is_file() && candidate.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) {
                return Some(candidate);
            }
        }
        #[cfg(windows)]
        {
            for ext in ["exe", "cmd", "bat"] {
                let c = candidate.with_extension(ext);
                if c.is_file() {
                    return Some(c);
                }
            }
        }
    }
    None
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
