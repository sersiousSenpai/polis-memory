// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis-memory` — the crate an integrator adds.
//!
//! [`Polis`] is a borrowed view over the four things memory needs: the store,
//! an optional model ([`polis_llm::Agent`]), the host's answers
//! ([`polis_core::host::HostResolver`]) and where a turn's cost goes
//! ([`polis_llm::UsageSink`]) — plus an optional embedder for the semantic
//! arm. Every function in this crate takes `&Polis<'_>`; a host builds one per
//! call from what it owns (Redline: `polis_host::polis_for(&db)`), and the
//! standalone daemon holds a [`PolisHandle`] (the same four, owned) that
//! implements [`polis_core::MemoryApi`] — the one surface the server, the
//! MCP server and the clients speak.
//!
//! What lives here, lifted from Redline in Session A5 of the Polis
//! extraction: the organizer ([`organize`]), the gardener's passes and gates
//! ([`gardener`]), retrieval ([`retrieval`]), export ([`bundle`]) and the
//! markdown mirror ([`mirror`]). The host keeps only what only a host has:
//! the watch bus, friction, its own tables, provider selection.

pub mod adjudicate;
pub mod agent;
pub mod backup;
pub mod bundle;
#[cfg(feature = "cli")]
pub mod cli;
pub mod canary;
pub mod envelope;
pub mod corpus;
pub mod diagnostics;
pub mod eval;
pub mod filing;
pub mod fence;
pub mod gardener;
pub mod health;
pub mod identity;
pub mod latency;
pub mod mirror;
pub mod organize;
pub mod orgnode;
pub mod retrieval;
pub mod scripted;
pub mod revert;
pub mod sharing;
pub mod skill;
pub mod sync;
pub mod transport;
pub mod union;
pub mod warmth;
pub mod usage;

use std::sync::Arc;

pub use polis_core;
pub use polis_embed;
pub use polis_llm;
pub use polis_store;

use polis_core::api::{
    AnnotateRequest, BoxFuture, BrowseRequest, CaptureRequest, ContextBlock, ContextRequest,
    ForgetReceipt, ForgetRequest, GrepRequest, HealthReport, IngestReceipt, IngestRequest,
    NodeView, OrganizeReceipt, PromptsRequest, ReindexReceipt, RememberRequest, Scope,
    SearchRequest, SupersedeReceipt, SupersedeRequest, TreeNodeView, TreeRequest, WriteReceipt,
};
use polis_core::host::HostResolver;
use polis_core::ledger::{ChainVerdict, CorpusRole, Origin, PromptSource};
use polis_core::pack::AnswerPack;
use polis_core::proposal::Proposal;
use polis_core::types::{
    BrowseHit, ClassRun, ContextStats, GrepHit, LakeItem, LedgerFilters, MemoryMapView,
    NoteOutcome, NoteWrite, PromptFilters, RevertReceipt, RunView, StageResult,
    SupersessionOutcome, TimelineItem,
};
use polis_core::{MemoryApi, MemoryError};
use polis_embed::{Embedder, ProviderKind, SemanticHit};
use polis_llm::{Agent, UsageSink};
use polis_store::record::{
    record_browse_event, record_prompt, record_prompt_at_scoped, BrowseAction, BrowseEventInput,
    PromptInput,
};
use polis_store::search::GrepError;
use polis_store::{PolisStore, StoreError};
use rusqlite::OptionalExtension;

/// The borrowed view every memory function takes.
pub struct Polis<'a> {
    pub store: &'a PolisStore,
    /// `None` is the no-model state (R12): the gardener runs its deterministic
    /// tiers and every model pass reports `no model configured`.
    pub agent: Option<Arc<dyn Agent>>,
    pub host: &'a dyn HostResolver,
    pub sink: &'a dyn UsageSink,
    /// The semantic arm's provider, when the host selected one. `None` means
    /// the arm is ABSENT (reported as such, never as empty).
    pub embedder: Option<Arc<dyn Embedder>>,
}

impl<'a> Polis<'a> {
    pub fn new(
        store: &'a PolisStore,
        agent: Option<Arc<dyn Agent>>,
        host: &'a dyn HostResolver,
        sink: &'a dyn UsageSink,
    ) -> Self {
        Self { store, agent, host, sink, embedder: None }
    }

    pub fn with_embedder(mut self, embedder: Option<Arc<dyn Embedder>>) -> Self {
        self.embedder = embedder;
        self
    }

    // --- the host seams, spelled the way the moved bodies call them ---------
    //
    // Each of these is where a `db.<method>` in Redline read a host table or a
    // host setting. The names are kept so the moved bodies read as they did;
    // the answer now comes from `polis_meta` or the `HostResolver`.

    /// A `polis.*` setting from `polis_meta` (was `app_settings`).
    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.store.meta(key).ok().flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.store.set_meta(key, value)
    }

    pub fn list_project_paths(&self) -> rusqlite::Result<Vec<String>> {
        Ok(self.host.project_roots())
    }

    pub fn thread_stats(&self, kind: &str, id: &str) -> Option<(i64, Option<i64>)> {
        self.host.thread_stats(kind, id)
    }

    pub fn thread_label(&self, kind: &str, id: &str) -> Option<String> {
        self.host.label(kind, id)
    }

    pub fn revision_markdown(&self, session: &str, version: i64) -> rusqlite::Result<Option<String>> {
        Ok(self.host.revision_markdown(session, version))
    }

    pub fn decision_event_context(&self, seq: i64) -> rusqlite::Result<Option<String>> {
        Ok(self.host.decision_evidence(seq))
    }

    /// Drop a queued proposal the verifier refuted. Redline also filed a
    /// friction row here; the B2 run journal (`class_run_ops`) is where a
    /// refusal is recorded from now on — until it lands, the log line is the
    /// trace.
    pub fn reject_class_proposal(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.store.conn();
        let op = PolisStore::delete_class_proposal(&conn, id)?;
        tracing::info!(proposal = id, op = op.as_deref().unwrap_or("?"), "proposal refuted by the verifier");
        Ok(())
    }

    /// The Timeline page: the store's rows, with the host's own pictures
    /// joined on (`surface_shots` is a host table).
    pub fn query_ledger_events(&self, f: &LedgerFilters) -> rusqlite::Result<Vec<TimelineItem>> {
        let mut items = self.store.query_ledger_events(f)?;
        if !items.is_empty() {
            let seqs: Vec<i64> = items.iter().map(|it| it.event.seq).collect();
            for (seq, key) in self.host.surface_shot_keys(&seqs) {
                if let Some(it) = items.iter_mut().find(|it| it.event.seq == seq) {
                    it.shot_key = Some(key);
                }
            }
        }
        Ok(items)
    }

    /// Which provider backs the semantic arm — the honest third state
    /// (`Absent`) when none does.
    pub fn provider_kind(&self) -> ProviderKind {
        self.embedder.as_deref().map(|e| e.kind()).unwrap_or(ProviderKind::Absent)
    }
}

/// Nearest targets to `query` by cosine, or `None` when no embedder is
/// configured — the arm is absent, not empty.
pub fn semantic_search(polis: &Polis<'_>, query: &str, limit: usize) -> Option<Vec<SemanticHit>> {
    let embedder = polis.embedder.as_deref()?;
    polis_embed::semantic_search(polis.store, embedder, query, limit)
}

/// Embed one tick's worth of backlog; 0 when no embedder is configured.
pub fn index_tick(polis: &Polis<'_>, max_targets: usize) -> usize {
    match polis.embedder.as_deref() {
        Some(embedder) => polis_embed::index_tick(polis.store, embedder, max_targets),
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// The owned handle
// ---------------------------------------------------------------------------

/// The same four things, owned — what a long-lived process (the daemon, the
/// MCP server, a host's app state) holds, and what implements [`MemoryApi`].
pub struct PolisHandle {
    pub store: Arc<PolisStore>,
    pub agent: Option<Arc<dyn Agent>>,
    pub host: Arc<dyn HostResolver>,
    pub sink: Arc<dyn UsageSink>,
    pub embedder: Option<Arc<dyn Embedder>>,
    /// The key this process writes as (E2). `None` = a store nobody has
    /// adopted yet: writes carry the store's legacy author string.
    pub identity: Option<Arc<identity::Identity>>,
}

impl PolisHandle {
    pub fn new(store: Arc<PolisStore>, agent: Option<Arc<dyn Agent>>, host: Arc<dyn HostResolver>, sink: Arc<dyn UsageSink>) -> Self {
        Self { store, agent, host, sink, embedder: None, identity: None }
    }

    pub fn with_embedder(mut self, embedder: Option<Arc<dyn Embedder>>) -> Self {
        self.embedder = embedder;
        self
    }

    /// Write as this identity: the device id, or the agent id the request's
    /// scope names (`scope.agent`), never a chosen name.
    pub fn with_identity(mut self, identity: Option<Arc<identity::Identity>>) -> Self {
        self.identity = identity;
        self
    }

    /// The author a write is stamped with. A named agent becomes a principal
    /// on first sight, so the scope stamp can resolve the id it writes.
    fn actor(&self, scope: &Scope) -> String {
        if let (Some(id), Some(agent)) = (self.identity.as_ref(), scope.agent.as_deref().map(str::trim).filter(|a| !a.is_empty())) {
            if let Err(e) = identity::ensure_agent(&self.store, id, agent) {
                tracing::warn!(error = %e, agent, "could not register the agent principal");
            }
        }
        identity::actor_for(self.identity.as_ref(), scope.agent.as_deref(), self.store.author())
    }

    /// The identity half of a request scope, for the store's clauses.
    fn filter(&self, scope: &Scope) -> Result<polis_store::principals::ScopeFilter, MemoryError> {
        let mut resolved = scope.clone();
        if let Some(identity) = &self.identity {
            let owner = identity.principal_id();
            if let Some(requested) = &scope.principal {
                let requested = self.store.resolve_author(requested).map_err(store_err)?.unwrap_or_else(|| requested.clone());
                if !self.store.principal_id_set(&owner).map_err(store_err)?.contains(&requested) {
                    return Err(MemoryError::Rejected("principal is outside this handle's permitted scope".into()));
                }
            } else { resolved.principal = Some(owner); }
        }
        Ok(polis_store::principals::ScopeFilter::from_scope(&resolved))
    }

    fn stamp_scope(&self, seq: Option<i64>, scope: &Scope) -> Result<(), MemoryError> {
        let Some(seq) = seq else { return Ok(()); };
        let f = self.filter(scope)?;
        let conn = self.store.conn();
        for (table, selector) in [("prompts", "id = (SELECT prompt_id FROM ledger_events WHERE seq = ?1)"), ("browse_events", "id = (SELECT CAST(ref_id AS INTEGER) FROM ledger_events WHERE seq = ?1 AND ref_kind = 'browse_event')"), ("user_notes", "seq = ?1")] {
            conn.execute(&format!("UPDATE {table} SET principal_id = COALESCE(principal_id, ?2), agent_id = COALESCE(agent_id, ?3), run_id = COALESCE(?4, run_id), org_id = COALESCE(?5, org_id), project_path = COALESCE(?6, project_path) WHERE {selector}"), rusqlite::params![seq, f.principal, f.agent, f.run, f.org, f.project]).map_err(store_err)?;
        }
        Ok(())
    }

    /// After a write: stamp the new rows' scope columns from their author.
    /// Cheap (a partial index over the unstamped rows) and never fatal.
    fn stamp(&self) {
        if let Err(e) = self.store.stamp_unscoped() {
            tracing::warn!(error = %e, "scope stamp failed");
        }
    }

    /// The borrowed view every memory function takes.
    pub fn view(&self) -> Polis<'_> {
        Polis {
            store: &self.store,
            agent: self.agent.clone(),
            host: &*self.host,
            sink: &*self.sink,
            embedder: self.embedder.clone(),
        }
    }
}

fn store_err(e: rusqlite::Error) -> MemoryError {
    MemoryError::Store(e.to_string())
}

impl MemoryApi for PolisHandle {
    fn decide(&self, req: &polis_core::diagnostics::DecisionRequest) -> Result<WriteReceipt, MemoryError> {
        let scope = self.filter(&req.scope)?;
        self.store.record_cited_decision(req.kind.as_deref().unwrap_or("decision"), req.source_seq, &scope)
            .map(|seq| WriteReceipt { seq: Some(seq), id: None }).map_err(|e| match e {
                rusqlite::Error::InvalidParameterName(reason) => MemoryError::Rejected(reason),
                other => store_err(other),
            })
    }
    fn write_claim(&self, req: &polis_core::claims::ClaimWrite) -> Result<polis_core::claims::Claim, MemoryError> {
        let filter = self.filter(&req.scope)?;
        let mut req = req.clone();
        req.scope.principal = filter.principal;
        self.store.write_claim(&req, polis_core::ledger::now_millis()).map_err(|e| match e {
            rusqlite::Error::InvalidParameterName(reason) => MemoryError::Rejected(reason),
            other => store_err(other),
        })
    }

    fn claims(&self, req: &polis_core::claims::ClaimQuery) -> Result<Vec<polis_core::claims::Claim>, MemoryError> {
        let filter = self.filter(&req.scope)?;
        let mut req = req.clone();
        req.scope.principal = filter.principal;
        self.store.query_claims(&req).map_err(store_err)
    }

    fn evidence(&self, req: &polis_core::diagnostics::EvidenceRequest) -> Result<polis_core::diagnostics::EvidenceRecord, MemoryError> {
        diagnostics::evidence(&self.view(), req, &self.filter(&req.scope)?)
    }

    fn traces(&self, req: &polis_core::diagnostics::TraceRequest) -> Result<Vec<polis_core::diagnostics::RetrievalTrace>, MemoryError> {
        diagnostics::traces(&self.view(), req, &self.filter(&req.scope)?)
    }

    fn search(&self, req: &SearchRequest) -> Result<AnswerPack, MemoryError> {
        retrieval::search_request(&self.view(), req, &self.filter(&req.scope)?)
    }

    fn grep(&self, req: &GrepRequest) -> Result<Vec<GrepHit>, MemoryError> {
        let limit = req.limit.unwrap_or(20).clamp(1, 200);
        latency::timed("grep", || {
            self.store
                .grep_memory_scoped(&req.literal, req.regex.as_deref(), req.case_sensitive, req.kinds, limit, &self.filter(&req.scope)?)
                .map_err(|e| match e {
                    GrepError::Db(m) => MemoryError::Store(m),
                    other => MemoryError::Rejected(other.to_string()),
                })
        })
    }

    fn tree(&self, req: &TreeRequest) -> Result<Vec<TreeNodeView>, MemoryError> {
        retrieval::tree_view_scoped(&self.view(), req.root.as_deref(), req.project.as_deref(), &self.filter(&req.scope)?).map_err(store_err)
    }

    fn node(&self, id: &str, scope: &Scope) -> Result<Option<NodeView>, MemoryError> {
        retrieval::node_view_scoped(&self.view(), id, &self.filter(scope)?).map_err(store_err)
    }

    fn prompts(&self, req: &PromptsRequest) -> Result<Vec<LakeItem>, MemoryError> {
        let limit = req.limit.unwrap_or(200).clamp(1, 400);
        self.store.list_lake_items_since_scoped(req.since_seq, limit, &self.filter(&req.scope)?).map_err(store_err)
    }

    fn timeline(&self, filters: &LedgerFilters, scope: &Scope) -> Result<Vec<TimelineItem>, MemoryError> {
        let mut f = filters.clone();
        let mut effective_scope = scope.clone();
        if let Some(principal) = f.principal.take() {
            if scope.principal.as_ref().is_some_and(|p| p != &principal) { return Err(MemoryError::Rejected("conflicting principal filters".into())); }
            effective_scope.principal = Some(principal);
        }
        f.project = f.project.or_else(|| scope.project.clone());
        if let Some(role) = f.role.as_mut() { *role = CorpusRole::parse(role).ok_or_else(|| MemoryError::Rejected(format!("unknown role `{role}`")))?.as_str().into(); }
        self.store.query_ledger_events_scoped(&f, &self.filter(&effective_scope)?).map_err(store_err)
    }

    fn stats(&self, scope: &Scope) -> Result<ContextStats, MemoryError> {
        let filter = self.filter(scope)?;
        if !filter.is_empty() { let mut stats=self.store.context_stats_scoped(&filter).map_err(store_err)?;stats.latency=latency::report();return Ok(stats); }
        // The counts are cached (head-seq keyed); the latency table is live.
        let mut stats = retrieval::build_stats_cached(&self.view());
        stats.latency = latency::report();
        Ok(stats)
    }

    fn map(&self, scope: &Scope) -> Result<MemoryMapView, MemoryError> {
        retrieval::build_memory_map_scoped(&self.view(), &self.filter(scope)?).map_err(store_err)
    }

    fn verify(&self) -> Result<ChainVerdict, MemoryError> {
        self.store.verify_ledger_chain().map_err(store_err)
    }

    fn list_prompts(&self, filters: &PromptFilters, scope: &Scope) -> Result<Vec<LakeItem>, MemoryError> {
        let resolved = self.filter(scope)?;
        for (name, left, right) in [("agent",&filters.agent,&scope.agent),("run",&filters.run,&scope.run),("org",&filters.org,&scope.org),("project",&filters.project,&scope.project)] {
            if left.as_ref().zip(right.as_ref()).is_some_and(|(a,b)|a!=b) { return Err(MemoryError::Rejected(format!("conflicting {name} filters"))); }
        }
        let mut f = filters.clone();
        if let Some(role) = f.role.as_mut() { *role = CorpusRole::parse(role).ok_or_else(|| MemoryError::Rejected(format!("unknown role `{role}`")))?.as_str().into(); }
        if f.principal.is_some() && resolved.principal.is_some() && f.principal != resolved.principal { return Err(MemoryError::Rejected("conflicting principal filters".into())); }
        f.principal = resolved.principal.or(f.principal);
        f.agent = f.agent.or_else(|| scope.agent.clone());
        f.run = f.run.or_else(|| scope.run.clone());
        f.org = f.org.or_else(|| scope.org.clone());
        f.project = f.project.or_else(|| scope.project.clone());
        retrieval::list_prompts(&self.view(), &f).map_err(MemoryError::Store)
    }

    fn browse_search(&self, q: &str, limit: i64, scope: &Scope) -> Result<Vec<BrowseHit>, MemoryError> {
        self.store.search_browse_events_scoped(q, limit.clamp(1, 100), &self.filter(scope)?).map_err(store_err)
    }

    fn thread_tree(&self, kind: &str, id: &str, scope: &Scope) -> Result<serde_json::Value, MemoryError> {
        retrieval::build_thread_tree_scoped(&self.view(), kind, id, &self.filter(scope)?).map_err(store_err)
    }

    fn thread(&self, kind: &str, id: &str, limit: i64, scope: &Scope) -> Result<Option<serde_json::Value>, MemoryError> {
        retrieval::thread_view_scoped(&self.view(), kind, id, limit.clamp(1, 200), &self.filter(scope)?).map_err(store_err)
    }

    fn context(&self, req: &ContextRequest) -> Result<ContextBlock, MemoryError> {
        let _timer = latency::Timer::start("context");
        retrieval::context_request(&self.view(), req, &self.filter(&req.scope)?)
    }

    fn health(&self) -> Result<HealthReport, MemoryError> {
        let view = self.view();
        let chain = self.store.verify_ledger_chain().map_err(store_err)?;
        let head_seq = self.store.max_ledger_seq().map_err(store_err)?;
        let class_nodes = self.store.list_class_nodes().map_err(store_err)?.len() as i64;
        // Counted directly, not through `build_stats_cached`: that cache is
        // process-global with a wall-clock floor, so two stores in one process
        // (two tests, or a daemon serving a fresh file) could read each
        // other's numbers. Health must be this store's.
        let total_prompts: i64 = self.store.prompt_counts_by_surface().map_err(store_err)?.iter().map(|(_, c)| c).sum();
        Ok(HealthReport {
            ok: chain.ok,
            chain,
            head_seq,
            total_prompts,
            class_nodes,
            model: self.agent.as_ref().map(|a| a.name().to_string()),
            embedder: view.provider_kind().as_str().to_string(),
            schema_version: view.get_setting(polis_store::meta::SCHEMA_VERSION_KEY),
            lexical_version: view.get_setting(polis_store::meta::LEXICAL_VERSION_KEY),
            catalog: Some(health::catalog_health(&view)),
        })
    }

    fn capture(&self, req: &CaptureRequest) -> Result<Option<i64>, MemoryError> {
        // The hook's row, exactly as Redline's ingest route always built it
        // (`source: Hook`, the captured-text role classifier, no seat, no
        // model — the transcript backfill stamps that later).
        let input = PromptInput {
            source: PromptSource::Hook,
            origin: req.origin,
            surface: req.surface.clone(),
            role: CorpusRole::classify_captured(&req.body),
            user_text: None,
            session_id: req.session.clone(),
            claude_session_id: req.session.clone(),
            mission_id: None,
            project_path: req.project.clone(),
            body: req.body.clone(),
            thread: None,
            // The hook fires inside a harness session: the writer is the
            // `claude-code` agent under this device (E2). Without an identity
            // the legacy author stays.
            author: self.identity.as_ref().map(|id| id.agent_id("claude-code")),
            model: None,
            model_source: None,
        };
        let seq = latency::timed("ingest.capture", || record_prompt(&self.store, input).map_err(MemoryError::Store))?;
        if seq.is_some() {
            self.stamp();
        }
        Ok(seq)
    }

    fn remember(&self, req: &RememberRequest) -> Result<WriteReceipt, MemoryError> {
        if req.text.trim().is_empty() {
            return Err(MemoryError::Rejected("nothing to remember".into()));
        }
        let _timer = latency::Timer::start("remember");
        let filter = self.filter(&req.scope)?;
        if req.project.as_ref().zip(req.scope.project.as_ref()).is_some_and(|(a,b)| a != b) { return Err(MemoryError::Rejected("item project conflicts with request scope".into())); }
        let actor = self.actor(&req.scope);
        {
            let seq = record_prompt_at_scoped(
                &self.store,
                PromptInput {
                    source: PromptSource::Api,
                    origin: Origin::External,
                    surface: "api".to_string(),
                    role: if req.as_user { CorpusRole::User } else { CorpusRole::Assistant },
                    session_id: None,
                    claude_session_id: req.scope.run.clone(),
                    mission_id: None,
                    project_path: req.project.clone().or_else(|| req.scope.project.clone()),
                    body: req.text.clone(),
                    thread: None,
                    author: Some(actor),
                    model: None,
                    model_source: None,
                    user_text: None,
                },
                polis_core::ledger::now_millis(), &filter,
            )
            .map_err(MemoryError::Store)?;
            Ok(WriteReceipt { seq, id: None })
        }
    }

    fn ingest(&self, req: &IngestRequest) -> Result<IngestReceipt, MemoryError> {
        let _timer = latency::Timer::start("ingest");
        let base_filter = self.filter(&req.scope)?;
        let actor = self.actor(&req.scope);
        for item in &req.items {
            if item.project.as_ref().zip(req.scope.project.as_ref()).is_some_and(|(a,b)|a!=b) || item.run.as_ref().zip(req.scope.run.as_ref()).is_some_and(|(a,b)|a!=b) { return Err(MemoryError::Rejected("item project/run conflicts with request scope".into())); }
            if let Some(role) = &item.role { if CorpusRole::parse(role).is_none() { return Err(MemoryError::Rejected(format!("unknown role `{role}`"))); } }
        }
        let mut receipt = IngestReceipt::default();
        for item in &req.items {
            let _item_timer = latency::Timer::start("ingest.item");
            if item.body.trim().is_empty() {
                receipt.skipped += 1;
                continue;
            }
            let role = item.role.as_deref().and_then(CorpusRole::parse).unwrap_or(CorpusRole::User);
            let input = PromptInput {
                source: PromptSource::Api,
                origin: Origin::External,
                surface: "import".to_string(),
                role,
                session_id: item.session.clone(),
                claude_session_id: item.run.clone().or_else(|| req.scope.run.clone()),
                mission_id: None,
                project_path: item.project.clone().or_else(|| req.scope.project.clone()),
                body: item.body.clone(),
                thread: None,
                author: Some(actor.clone()),
                model: None,
                model_source: None,
                user_text: None,
            };
            let ts = item.ts.unwrap_or_else(polis_core::ledger::now_millis);
            let mut filter = base_filter.clone();
            filter.run = item.run.clone().or(filter.run);
            filter.project = item.project.clone().or(filter.project);
            match record_prompt_at_scoped(&self.store, input, ts, &filter).map_err(MemoryError::Store)? {
                Some(seq) => {
                    receipt.recorded.push(seq);
                },
                None => receipt.skipped += 1, // dedup on (body_hash, run)
            }
        }
        if !receipt.recorded.is_empty() {
            self.stamp();
        }
        Ok(receipt)
    }

    fn annotate(&self, req: &AnnotateRequest) -> Result<WriteReceipt, MemoryError> {
        let filter = self.filter(&req.scope)?;
        match (req.target_kind.as_str(), req.target_id.as_deref()) {
            ("ledger_event", Some(id)) => { let seq=id.parse::<i64>().map_err(|_|MemoryError::Rejected("target_id must be a ledger sequence".into()))?; if !self.store.eligible_seqs(&[seq],&filter).map_err(store_err)?.contains(&seq) { return Err(MemoryError::NotFound); } }
            ("class_node", Some(id)) if self.store.get_class_node_scoped(id,&filter).map_err(store_err)?.is_none() => { return Err(MemoryError::NotFound); }
            _ => {}
        }
        let write = NoteWrite {
            target_kind: Some(req.target_kind.clone()),
            target_id: req.target_id.clone(),
            text: Some(req.text.clone()),
            ..Default::default()
        };
        let out = note_receipt(self.store.write_user_note_scoped(&write, &self.actor(&req.scope), &filter).map_err(store_err)?);
        self.stamp();
        if let Ok(receipt) = &out { self.stamp_scope(receipt.seq, &req.scope)?; }
        out
    }

    fn forget(&self, req: &ForgetRequest) -> Result<ForgetReceipt, MemoryError> {
        if req.confirm != "forget" {
            return Err(MemoryError::Rejected(
                "forget requires confirm: \"forget\"".into(),
            ));
        }
        if req.target_kind == "ledger_event" {
            let source = req
                .target_id
                .trim_start_matches('#')
                .parse::<i64>()
                .map_err(|_| MemoryError::Rejected("target_id must be a ledger sequence".into()))?;
            let filter = self.filter(&req.scope)?;
            if !self
                .store
                .eligible_seqs(&[source], &filter)
                .map_err(store_err)?
                .contains(&source)
            {
                return Err(MemoryError::NotFound);
            }
            if let Some(note) = self.store.note_for_seq(source).map_err(store_err)? {
                return self.forget(&ForgetRequest {
                    target_kind: "user_note".into(),
                    target_id: note.id.to_string(),
                    confirm: req.confirm.clone(),
                    scope: req.scope.clone(),
                });
            }
            let item = self.evidence(&polis_core::diagnostics::EvidenceRequest {
                seq: source,
                scope: req.scope.clone(),
                chain_id: None,
            })?;
            if item.status == "redacted" {
                return Ok(ForgetReceipt {
                    forgotten: true,
                    seq: None,
                });
            }
            if item.status != "available" {
                return Err(MemoryError::NotFound);
            }
            let target=self.store.conn().query_row("SELECT COALESCE(le.prompt_id,src.prompt_id),le.ref_kind,le.ref_id FROM ledger_events le LEFT JOIN decision_evidence de ON de.seq=le.seq LEFT JOIN ledger_events src ON src.seq=de.source_seq WHERE le.seq=?1",[source],|r|Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?))).map_err(store_err)?;
            let (kind, id) = if let Some(id) = target.0 {
                ("prompt", id.to_string())
            } else if target.1.as_deref() == Some("browse_event") {
                ("browse_event", target.2.ok_or(MemoryError::NotFound)?)
            } else {
                return Err(MemoryError::Unavailable(
                    "this citation's content type does not support durable forgetting".into(),
                ));
            };
            return self.forget(&ForgetRequest {
                target_kind: kind.into(),
                target_id: id,
                confirm: req.confirm.clone(),
                scope: req.scope.clone(),
            });
        }
        match req.target_kind.as_str() {
            "prompt" => {
                let id: i64 =
                    req.target_id.trim().parse().map_err(|_| {
                        MemoryError::Rejected("target_id must be a prompt id".into())
                    })?;
                let filter = self.filter(&req.scope)?;
                let source = self
                    .store
                    .seqs_for_prompt_ids(&[id])
                    .map_err(store_err)?
                    .get(&id)
                    .copied()
                    .ok_or(MemoryError::NotFound)?;
                if !self
                    .store
                    .eligible_seqs(&[source], &filter)
                    .map_err(store_err)?
                    .contains(&source)
                {
                    return Err(MemoryError::NotFound);
                }
                let actor = self.actor(&req.scope);
                let seq = self
                    .store
                    .compact_prompt_body(
                        id,
                        "[forgotten]",
                        "forget",
                        gardener::GIST_SOURCE_DETERMINISTIC,
                        &actor,
                    )
                    .map_err(store_err)?;
                // --- E3 --- a `redaction` event so peers that hold this body
                // tombstone it on their next sync (plan §4.6 "Forget
                // propagates"). Only a device chain can name what it forgot.
                if let Some(identity) = self.identity.as_ref() {
                    if let Err(e) =
                        sharing::append_redaction(&self.store, &identity.device_id(), id, &actor)
                    {
                        tracing::warn!(error = %e, prompt = id, "forget: the redaction event was not appended");
                    }
                }
                Ok(ForgetReceipt {
                    forgotten: true,
                    seq,
                })
            }
            "browse_event" => {
                let id = req.target_id.parse::<i64>().map_err(|_| {
                    MemoryError::Rejected("target_id must be a browse event row id".into())
                })?;
                let source=self.store.conn().query_row("SELECT seq FROM ledger_events WHERE kind='browse_event' AND ref_kind='browse_event' AND ref_id=?1 ORDER BY seq LIMIT 1",[id.to_string()],|r|r.get::<_,i64>(0)).map_err(store_err)?;
                let filter = self.filter(&req.scope)?;
                if !self
                    .store
                    .eligible_seqs(&[source], &filter)
                    .map_err(store_err)?
                    .contains(&source)
                {
                    return Err(MemoryError::NotFound);
                }
                let seq = self
                    .store
                    .forget_browse_event(id, &self.actor(&req.scope))
                    .map_err(store_err)?;
                Ok(ForgetReceipt {
                    forgotten: true,
                    seq,
                })
            }
            "user_note" | "note" => {
                let id = req
                    .target_id
                    .parse::<i64>()
                    .map_err(|_| MemoryError::Rejected("target_id must be a note row id".into()))?;
                let source = self
                    .store
                    .conn()
                    .query_row("SELECT seq FROM user_notes WHERE id=?1", [id], |r| {
                        r.get::<_, Option<i64>>(0)
                    })
                    .optional()
                    .map_err(store_err)?
                    .flatten()
                    .ok_or(MemoryError::NotFound)?;
                let filter = self.filter(&req.scope)?;
                if !self
                    .store
                    .eligible_seqs(&[source], &filter)
                    .map_err(store_err)?
                    .contains(&source)
                {
                    return Err(MemoryError::NotFound);
                }
                let actor = self.actor(&req.scope);
                let seq = self.store.forget_user_note(id, &actor).map_err(store_err)?;
                if let Some(identity) = &self.identity {
                    if let Err(error) =
                        sharing::flush_redactions(&self.store, &identity.device_id(), &actor)
                    {
                        tracing::warn!(%error, note=id, "note redaction remains queued for retry");
                    }
                }
                Ok(ForgetReceipt {
                    forgotten: true,
                    seq,
                })
            }
            other => Err(MemoryError::Unavailable(format!(
                "durable forgetting is not supported for `{other}`"
            ))),
        }
    }

    fn supersede(&self, req: &SupersedeRequest) -> Result<SupersedeReceipt, MemoryError> {
        let filter = self.filter(&req.scope)?;
        let seqs = [req.old_seq, req.new_seq];
        let eligible = self.store.eligible_seqs(&seqs, &filter).map_err(store_err)?;
        if seqs.iter().any(|seq| !eligible.contains(seq)) { return Err(MemoryError::NotFound); }
        match self
            .store
            .apply_supersession(req.old_seq, req.new_seq, req.rationale.as_deref().unwrap_or(""), &self.actor(&req.scope))
            .map_err(store_err)?
        {
            SupersessionOutcome::Applied { effective_old, new_seq: _, event_seq } => Ok(SupersedeReceipt {
                applied: true,
                effective_old: Some(effective_old),
                event_seq: Some(event_seq),
                rejected: None,
            }),
            SupersessionOutcome::Rejected(reason) => Ok(SupersedeReceipt {
                applied: false,
                effective_old: None,
                event_seq: None,
                rejected: Some(reason),
            }),
        }
    }

    fn stage_proposals(&self, proposals: &[Proposal], actor: &str) -> Result<StageResult, MemoryError> {
        // B3: the route stages into the same queue under the same
        // adjudication as the gardener's own ops — file / create judged now,
        // structural ops queued for the next run.
        let out = organize::stage_adjudicated(&self.view(), None, proposals, actor)
            .map(|s| s.result)
            .map_err(MemoryError::Store);
        self.stamp();
        out
    }

    fn browse(&self, req: &BrowseRequest) -> Result<WriteReceipt, MemoryError> {
        self.filter(&req.scope)?;
        if self.identity.is_some() { if let Some(author)=&req.author { let mut source_scope=req.scope.clone();source_scope.principal=Some(author.clone());self.filter(&source_scope)?; } }
        if req.url.trim().is_empty() {
            return Err(MemoryError::Rejected("a browse event needs a url".into()));
        }
        let action = match req.action.as_deref().unwrap_or("navigate") {
            "navigate" => BrowseAction::Navigate,
            "select" => BrowseAction::Select,
            "submit" => BrowseAction::Submit,
            "leave" => BrowseAction::Leave,
            other => return Err(MemoryError::Rejected(format!("unknown browse action `{other}`"))),
        };
        let seq = record_browse_event(
            &self.store,
            BrowseEventInput {
                action,
                browse_id: req.browse_id.clone(),
                url: req.url.clone(),
                title: req.title.clone(),
                text: req.text.clone(),
                from_event_id: None,
                author: req.author.clone().or_else(|| Some(self.actor(&req.scope))),
            },
        )
        .map_err(MemoryError::Store)?;
        self.stamp();
        self.stamp_scope(seq, &req.scope)?;
        Ok(WriteReceipt { seq, id: None })
    }

    fn organize(&self, scope: &Scope) -> BoxFuture<'_, Result<OrganizeReceipt, MemoryError>> {
        let restricted = !polis_store::principals::ScopeFilter::from_scope(scope).is_empty();
        Box::pin(async move {
            if restricted { return Err(MemoryError::Unavailable("organization is store-wide; scoped organization is not available".into())); }
            let view = self.view();
            match organize::organize_once(&view).await {
                Ok(o) => {
                    self.stamp();
                    Ok(OrganizeReceipt {
                        ran: o.ran,
                        auto_applied: o.auto_applied,
                        summary: o.summary,
                        seq_from: o.seq_from,
                        seq_to: o.seq_to,
                        staged: o.staged,
                    })
                }
                Err(e) if e == agent::NO_MODEL => Err(MemoryError::Unavailable(e)),
                Err(e) => Err(MemoryError::Store(e)),
            }
        })
    }

    fn reindex(&self, scope: &Scope) -> Result<ReindexReceipt, MemoryError> {
        if !polis_store::principals::ScopeFilter::from_scope(scope).is_empty() { return Err(MemoryError::Unavailable("index maintenance is store-wide; scoped reindex is not available".into())); }
        let view = self.view();
        let provider = view.provider_kind().as_str().to_string();
        let embedded = index_tick(&view, REINDEX_MAX_TARGETS);
        // C1: the filing cache follows the index (docs/filing.md).
        let centroids = filing::rebuild_centroids(&view);
        tracing::info!(embedded, centroids, "reindex rebuilt the class centroids");
        Ok(ReindexReceipt { embedded, provider })
    }

    // --- B2: the runs, reversible ------------------------------------------

    fn list_runs(&self, limit: i64, scope: &Scope) -> Result<Vec<ClassRun>, MemoryError> {
        self.store.list_class_runs_scoped(limit.clamp(1, revert::RUNS_PAGE_MAX), &self.filter(scope)?).map_err(store_err)
    }

    fn run(&self, id: i64) -> Result<Option<RunView>, MemoryError> {
        revert::run_view(&self.view(), id)
    }

    fn revert_run(&self, id: i64) -> Result<RevertReceipt, MemoryError> {
        revert::revert_run(&self.view(), id)
    }
}

/// How much of the semantic backlog one `reindex` call embeds. Bounded so a
/// route call is a bounded amount of work; a caller drains a large backlog by
/// calling again (the gardener's own tick keeps draining it regardless).
pub const REINDEX_MAX_TARGETS: usize = 256;

fn note_receipt(outcome: NoteOutcome) -> Result<WriteReceipt, MemoryError> {
    match outcome {
        NoteOutcome::Written(n) | NoteOutcome::Unchanged(n) => Ok(WriteReceipt { seq: n.seq, id: Some(n.id) }),
        NoteOutcome::Rejected(r) => Err(MemoryError::Rejected(r)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_core::host::NoHost;
    use polis_llm::NoopSink;

    fn handle() -> PolisHandle {
        PolisHandle::new(Arc::new(PolisStore::open_in_memory().unwrap()), None, Arc::new(NoHost), Arc::new(NoopSink))
    }

    #[test]
    fn stats_report_the_latency_of_every_op_that_ran() {
        let api = handle();
        let _ = api.search(&SearchRequest { q: Some("anything at all".into()), ..Default::default() }).unwrap();
        let _ = api.grep(&GrepRequest { literal: "anything".into(), ..Default::default() }).unwrap();
        let _ = api.context(&ContextRequest { q: "anything".into(), ..Default::default() }).unwrap();
        let stats = api.stats(&Scope::default()).unwrap();
        let ops: Vec<&str> = stats.latency.iter().map(|r| r.op.as_str()).collect();
        for op in ["pack", "pack.resolve", "pack.lexical", "pack.browse", "pack.grep", "grep", "context"] {
            assert!(ops.contains(&op), "stats.latency lacks `{op}`: {ops:?}");
        }
        let json = serde_json::to_value(&stats).unwrap();
        assert!(json["latency"].as_array().map(|a| !a.is_empty()).unwrap_or(false), "serialized as `latency`");
    }

    #[test]
    fn the_handle_serves_the_one_surface_over_an_empty_store() {
        let h = handle();
        let api: &dyn MemoryApi = &h;
        assert!(api.verify().unwrap().ok);
        assert!(api.tree(&TreeRequest::default()).unwrap().is_empty());
        assert_eq!(api.node("nope", &Scope::default()).unwrap().map(|_| ()), None);
        let pack = api.search(&SearchRequest { q: Some("anything".into()), ..Default::default() }).unwrap();
        assert!(pack.prompt_hits.is_empty());
        assert!(api.stats(&Scope::default()).unwrap().total_prompts == 0);
        assert!(api.map(&Scope::default()).unwrap().nodes.is_empty());
    }

    #[test]
    fn remember_and_ingest_write_through_the_chain_and_dedupe() {
        let h = handle();
        let api: &dyn MemoryApi = &h;
        let r = api.remember(&RememberRequest { text: "use postgres".into(), as_user: true, ..Default::default() }).unwrap();
        assert_eq!(r.seq, Some(1));
        let items = api.prompts(&PromptsRequest::default()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].role.as_deref(), Some("user"));

        let batch = IngestRequest {
            items: vec![
                polis_core::api::IngestItem { body: "hello".into(), ts: Some(1_000), run: Some("r1".into()), ..Default::default() },
                polis_core::api::IngestItem { body: "hello".into(), ts: Some(2_000), run: Some("r1".into()), ..Default::default() },
                polis_core::api::IngestItem { body: "  ".into(), ..Default::default() },
            ],
            ..Default::default()
        };
        let receipt = api.ingest(&batch).unwrap();
        assert_eq!(receipt.recorded, vec![2]);
        assert_eq!(receipt.skipped, 2, "the replay and the blank are skipped");
        assert!(api.verify().unwrap().ok);

        let note = api.remember(&RememberRequest { text: "a standalone thought".into(), as_user: false, ..Default::default() }).unwrap();
        assert!(note.seq.is_some());
        let remembered = h.store.lake_items_for_seqs(&[note.seq.unwrap()]).unwrap();
        assert_eq!(remembered[0].role.as_deref(), Some("assistant"));
        assert!(api.forget(&ForgetRequest { target_kind: "prompt".into(), target_id: "1".into(), confirm: "nope".into(), ..Default::default() }).is_err());
        let f = api.forget(&ForgetRequest { target_kind: "prompt".into(), target_id: "1".into(), confirm: "forget".into(), ..Default::default() }).unwrap();
        assert!(f.forgotten);
        assert!(api.verify().unwrap().ok, "forget keeps the chain green");
    }

    /// The A6 additions: the hook's capture row, a browse event, the filtered
    /// reads, health in the no-model state, and a reindex with no embedder —
    /// every one answered, none an error.
    #[test]
    fn the_capture_browse_and_maintenance_methods_answer_over_an_empty_install() {
        let h = handle();
        let api: &dyn MemoryApi = &h;
        let seq = api
            .capture(&CaptureRequest {
                body: "captured by the hook".into(),
                origin: Origin::External,
                surface: "external".into(),
                session: Some("sess-1".into()),
                project: Some("/tmp/p".into()),
            })
            .unwrap();
        assert_eq!(seq, Some(1));
        let again = api
            .capture(&CaptureRequest {
                body: "captured by the hook".into(),
                origin: Origin::External,
                surface: "external".into(),
                session: Some("sess-1".into()),
                project: Some("/tmp/p".into()),
            })
            .unwrap();
        assert_eq!(again, None, "the store's own dedup");
        let items = api.list_prompts(&PromptFilters { limit: 10, ..Default::default() }, &Scope::default()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].surface.as_deref(), Some("external"));

        let b = api
            .browse(&BrowseRequest { url: "https://example.test/a".into(), text: "Example page".into(), title: Some("Example".into()), ..Default::default() })
            .unwrap();
        assert_eq!(b.seq, Some(2));
        assert!(api.browse(&BrowseRequest { url: "".into(), ..Default::default() }).is_err());
        assert!(api.browse(&BrowseRequest { url: "https://x".into(), action: Some("teleport".into()), ..Default::default() }).is_err());
        assert!(!api.browse_search("Example", 10, &Scope::default()).unwrap().is_empty());

        assert!(api.thread("browse", "nope", 10, &Scope::default()).unwrap().is_none(), "NoHost owns no threads");
        let tree = api.thread_tree("session", "s1", &Scope::default()).unwrap();
        assert_eq!(tree["node"]["kind"], "session");

        let health = api.health().unwrap();
        assert!(health.ok);
        assert_eq!(health.head_seq, 2);
        assert_eq!(health.model, None, "no model → reported, not an error");
        assert_eq!(health.embedder, "absent");
        assert_eq!(health.schema_version.as_deref(), Some(polis_store::meta::STORE_SCHEMA_VERSION));

        let r = api.reindex(&Scope::default()).unwrap();
        assert_eq!((r.embedded, r.provider.as_str()), (0, "absent"));

        let block = api.context(&ContextRequest { q: "captured hook".into(), ..Default::default() }).unwrap();
        assert!(!block.terms.is_empty(), "the plan names what it searched");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let o = rt.block_on(api.organize(&Scope::default())).expect("C1: organize files without a model (R12)");
        assert!(o.ran);
        assert!(o.summary.contains(filing::INBOX_TITLE), "no model, no embedder → the item is parked: {}", o.summary);
    }
}
