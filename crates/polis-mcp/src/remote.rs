// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `RemoteApi` (feature `remote`): the [`MemoryApi`] implemented as an HTTP
//! client over polis-server's routes, so one `polis mcp` process can front a
//! running daemon (`polis serve`, or Redline's `127.0.0.1:7676`) instead of
//! opening the store a second time. Reads have real bodies; the writes that
//! need identity land in E2 and report `Unavailable` until then — except
//! `capture`, which is the open hook contract and works today.
//!
//! Synchronous on purpose (the trait is), over a runtime-free client: an MCP
//! tool call runs it inside `spawn_blocking`, the CLI calls it inline.

use std::sync::Arc;
use std::time::Duration;

use polis_core::api::*;
use polis_core::ledger::ChainVerdict;
use polis_core::pack::AnswerPack;
use polis_core::proposal::Proposal;
use polis_core::types::*;
use polis_core::{MemoryApi, MemoryError};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A daemon by address, with an optional bearer token for its writes.
#[derive(Clone)]
pub struct RemoteApi {
    base: String,
    token: Option<String>,
    agent: Arc<ureq::Agent>,
}

/// One read's timeout — a loopback daemon answers in milliseconds; a
/// stalled one must not hang a tool call.
pub const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

/// Percent-encode a path segment (ids and thread kinds travel in the path).
fn segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl RemoteApi {
    /// `base` like `http://127.0.0.1:7677` (no trailing slash needed).
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self { base, token, agent: Arc::new(agent(REMOTE_TIMEOUT)) }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.agent = Arc::new(agent(timeout));
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn map_status(status: u16, body: &str) -> MemoryError {
        let detail = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| body.trim().to_string());
        match status {
            404 => MemoryError::NotFound,
            400 => MemoryError::Rejected(detail),
            401 | 403 => MemoryError::Rejected(format!("daemon refused the credential: {detail}")),
            503 => MemoryError::Unavailable(detail),
            _ => MemoryError::Store(format!("HTTP {status}: {detail}")),
        }
    }

    fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T, MemoryError> {
        let mut req = self.agent.get(format!("{}{}", self.base, path));
        for (k, v) in query {
            req = req.query(*k, v);
        }
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut resp = req.call().map_err(|e| MemoryError::Unavailable(format!("daemon unreachable at {}: {e}", self.base)))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().map_err(|e| MemoryError::Store(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(Self::map_status(status, &body));
        }
        serde_json::from_str(&body).map_err(|e| MemoryError::Store(format!("{path}: unexpected body: {e}")))
    }

    fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), MemoryError> {
        let mut req = self.agent.post(format!("{}{}", self.base, path));
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut resp = req.send_json(body).map_err(|e| MemoryError::Unavailable(format!("daemon unreachable at {}: {e}", self.base)))?;
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().map_err(|e| MemoryError::Store(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(Self::map_status(status, &text));
        }
        Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
    }

    /// The hook contract, raw: POST the UserPromptSubmit payload as the hook
    /// would and return the daemon's body verbatim (a `{seq}` receipt, a
    /// `{skipped}` note, or a `hookSpecificOutput` answer the harness must
    /// see). What `polis capture` relays.
    pub fn capture_raw(&self, payload: &Value) -> Result<Value, MemoryError> {
        self.post_json("/v1/prompts/ingest", payload).map(|(_, v)| v)
    }

    /// `GET /v1/memory/health` with a short timeout: is a daemon alive at
    /// `base`? The backend chooser's probe.
    pub fn probe(base: &str, timeout: Duration) -> Option<HealthReport> {
        Self::probe_authenticated(base, None, timeout)
    }

    pub fn probe_authenticated(base: &str, token: Option<String>, timeout: Duration) -> Option<HealthReport> {
        let api = RemoteApi::new(base, token).with_timeout(timeout);
        api.health().ok()
    }
}

fn q(v: &Option<String>) -> Option<String> {
    v.as_ref().filter(|s| !s.trim().is_empty()).cloned()
}

fn add_scope(query: &mut Vec<(&'static str, String)>, scope: &Scope) -> Result<(), MemoryError> {
    for (key, value) in [("principal", &scope.principal), ("org", &scope.org), ("agent", &scope.agent), ("run", &scope.run), ("project", &scope.project)] {
        if let Some(value) = q(value) {
            if let Some((_, existing)) = query.iter().find(|(k, _)| *k == key) {
                if existing != &value { return Err(MemoryError::Rejected(format!("conflicting {key} filters"))); }
            } else { query.push((key, value)); }
        }
    }
    if scope.include_shared { query.push(("include_shared", "true".into())); }
    Ok(())
}

fn scope_query(scope: &Scope) -> Result<Vec<(&'static str, String)>, MemoryError> {
    let mut query = Vec::new(); add_scope(&mut query, scope)?; Ok(query)
}

fn add_filter(query: &mut Vec<(&'static str, String)>, filter: &EvidenceFilter) {
    if !filter.roles.is_empty() { query.push(("roles", filter.roles.join(","))); }
    if let Some(ts) = filter.after { query.push(("after", ts.to_string())); }
    if let Some(ts) = filter.before { query.push(("before", ts.to_string())); }
    if let Some(ts) = filter.valid_at { query.push(("valid_at", ts.to_string())); }
    if let Some(ts) = filter.known_at { query.push(("known_at", ts.to_string())); }
}

#[derive(serde::Deserialize)]
struct Items<T> {
    items: Vec<T>,
}

#[derive(serde::Deserialize)]
struct Nodes {
    nodes: Vec<TreeNodeView>,
}

#[derive(serde::Deserialize)]
struct Hits {
    hits: Vec<GrepHit>,
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn from_value<T: DeserializeOwned>(v: Value, route: &str) -> Result<T, MemoryError> {
    serde_json::from_value(v).map_err(|e| MemoryError::Store(format!("{route}: unexpected body: {e}")))
}

fn proposal_value(proposal: &Proposal) -> Value {
    match proposal {
        Proposal::File { parent_id, sub_class, target_kind, target_id, note, rationale } => serde_json::json!({"op":"file","parent_id":parent_id,"sub_class":sub_class,"target_kind":target_kind,"target_id":target_id,"note":note,"rationale":rationale}),
        Proposal::Create { parent_id, title, rationale } => serde_json::json!({"op":"create","parent_id":parent_id,"title":title,"rationale":rationale}),
        Proposal::Promote { node_id, new_parent_id, rationale } => serde_json::json!({"op":"promote","node_id":node_id,"new_parent_id":new_parent_id,"rationale":rationale}),
        Proposal::Split { node_id, into, rationale } => serde_json::json!({"op":"split","node_id":node_id,"into":into,"rationale":rationale}),
        Proposal::Merge { node_ids, title, parent_id, rationale } => serde_json::json!({"op":"merge","node_ids":node_ids,"title":title,"parent_id":parent_id,"rationale":rationale}),
        Proposal::Collapse { node_id, summary, cite_seqs, rationale } => serde_json::json!({"op":"collapse","node_id":node_id,"summary":summary,"cite_seqs":cite_seqs,"rationale":rationale}),
        Proposal::Supersede { old_seq, new_seq, rationale } => serde_json::json!({"op":"supersede","old_seq":old_seq,"new_seq":new_seq,"rationale":rationale}),
    }
}

impl MemoryApi for RemoteApi {
    fn decide(&self, req: &polis_core::diagnostics::DecisionRequest) -> Result<WriteReceipt, MemoryError> {
        let (_, value) = self.post_json("/v1/memory/decisions", &to_value(req))?; from_value(value, "decisions")
    }

    fn write_claim(&self, req: &polis_core::claims::ClaimWrite) -> Result<polis_core::claims::Claim, MemoryError> {
        let (_, value) = self.post_json("/v1/memory/claims", &to_value(req))?; from_value(value, "claims")
    }

    fn claims(&self, req: &polis_core::claims::ClaimQuery) -> Result<Vec<polis_core::claims::Claim>, MemoryError> {
        let mut query = scope_query(&req.scope)?;
        if let Some(q) = &req.q { query.push(("q", q.clone())); }
        if let Some(subject) = &req.subject { query.push(("subject", subject.clone())); }
        if let Some(predicate) = &req.predicate { query.push(("predicate", to_value(predicate).as_str().unwrap_or_default().to_string())); }
        if let Some(t) = req.valid_at { query.push(("valid_at", t.to_string())); }
        if let Some(t) = req.known_at { query.push(("known_at", t.to_string())); }
        if let Some(limit) = req.limit { query.push(("limit", limit.to_string())); }
        add_filter(&mut query, &req.evidence_filter);
        let value: Value = self.get_json("/v1/memory/claims", &query)?;
        from_value(value.get("claims").cloned().unwrap_or(Value::Null), "claims")
    }

    fn evidence(&self, req: &polis_core::diagnostics::EvidenceRequest) -> Result<polis_core::diagnostics::EvidenceRecord, MemoryError> {
        let mut query = scope_query(&req.scope)?;
        if let Some(chain) = &req.chain_id { query.push(("chain_id", chain.clone())); }
        self.get_json(&format!("/v1/memory/evidence/{}", req.seq), &query)
    }

    fn traces(&self, req: &polis_core::diagnostics::TraceRequest) -> Result<Vec<polis_core::diagnostics::RetrievalTrace>, MemoryError> {
        let mut query = scope_query(&req.scope)?;
        if let Some(id) = &req.id { query.push(("id", id.clone())); }
        if let Some(limit) = req.limit { query.push(("limit", limit.to_string())); }
        let value: Value = self.get_json("/v1/memory/traces", &query)?;
        from_value(value.get("traces").cloned().unwrap_or(Value::Null), "traces")
    }

    fn search(&self, req: &SearchRequest) -> Result<AnswerPack, MemoryError> {
        let mut query = Vec::new();
        if let Some(v) = q(&req.q) {
            query.push(("q", v));
        }
        if let Some(v) = q(&req.node) {
            query.push(("node", v));
        }
        if let Some(l) = req.limit {
            query.push(("limit", l.to_string()));
        }
        add_scope(&mut query, &req.scope)?;
        add_filter(&mut query, &req.filter);
        for (key, value) in [("candidate_limit", req.candidate_limit), ("max_tokens", req.max_tokens)] { if let Some(value) = value { query.push((key, value.to_string())); } }
        if let Some(value) = &req.cursor { query.push(("cursor", value.clone())); }
        if let Some(value) = &req.trace_id { query.push(("trace_id", value.clone())); }
        self.get_json("/v1/memory/answer-pack", &query)
    }

    fn grep(&self, req: &GrepRequest) -> Result<Vec<GrepHit>, MemoryError> {
        let mut query = vec![("q", req.literal.clone())];
        if let Some(v) = q(&req.regex) {
            query.push(("re", v));
        }
        if req.case_sensitive {
            query.push(("case", "1".into()));
        }
        let kinds = match req.kinds {
            GrepScope::All => "all",
            GrepScope::Prompts => "prompts",
            GrepScope::Browse => "browse",
        };
        query.push(("scope", kinds.into()));
        if let Some(l) = req.limit {
            query.push(("limit", l.to_string()));
        }
        add_scope(&mut query, &req.scope)?;
        self.get_json::<Hits>("/v1/memory/grep", &query).map(|h| h.hits)
    }

    fn tree(&self, req: &TreeRequest) -> Result<Vec<TreeNodeView>, MemoryError> {
        let mut query = Vec::new();
        if let Some(v) = q(&req.root) {
            query.push(("root", v));
        }
        if let Some(v) = q(&req.project) {
            query.push(("project", v));
        }
        add_scope(&mut query, &req.scope)?;
        self.get_json::<Nodes>("/v1/memory/tree", &query).map(|n| n.nodes)
    }

    fn node(&self, id: &str, scope: &Scope) -> Result<Option<NodeView>, MemoryError> {
        match self.get_json::<NodeView>(&format!("/v1/memory/node/{}", segment(id)), &scope_query(scope)?) {
            Ok(v) => Ok(Some(v)),
            Err(MemoryError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn prompts(&self, req: &PromptsRequest) -> Result<Vec<LakeItem>, MemoryError> {
        let mut query = vec![("since_seq", req.since_seq.to_string())];
        if let Some(l) = req.limit {
            query.push(("limit", l.to_string()));
        }
        add_scope(&mut query, &req.scope)?;
        self.get_json::<Items<LakeItem>>("/v1/memory/prompts", &query).map(|i| i.items)
    }

    fn timeline(&self, f: &LedgerFilters, scope: &Scope) -> Result<Vec<TimelineItem>, MemoryError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        for (k, v) in [
            ("kind", &f.kind),
            ("author", &f.author),
            ("session", &f.session_id),
            ("surface", &f.surface),
            ("project", &f.project),
            ("q", &f.q),
            ("class_node", &f.class_node),
            ("thread_id", &f.thread_id),
            ("browse_id", &f.browse_id),
            ("role", &f.role),
            ("principal", &f.principal),
        ] {
            if let Some(v) = q(v) {
                query.push((k, v));
            }
        }
        for (k, v) in [("since_ts", f.since_ts), ("until_ts", f.until_ts), ("before_seq", f.before_seq), ("limit", f.limit)] {
            if let Some(v) = v {
                query.push((k, v.to_string()));
            }
        }
        if f.starred == Some(true) {
            query.push(("starred", "1".into()));
        }
        if f.noted == Some(true) {
            query.push(("noted", "1".into()));
        }
        if let Some(seqs) = &f.seqs {
            query.push(("seqs", seqs.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(",")));
        }
        add_scope(&mut query, scope)?;
        self.get_json::<Items<TimelineItem>>("/v1/memory/ledger", &query).map(|i| i.items)
    }

    fn stats(&self, scope: &Scope) -> Result<ContextStats, MemoryError> {
        self.get_json("/v1/context/stats", &scope_query(scope)?)
    }

    fn map(&self, scope: &Scope) -> Result<MemoryMapView, MemoryError> {
        self.get_json("/v1/memory/map", &scope_query(scope)?)
    }

    fn verify(&self) -> Result<ChainVerdict, MemoryError> {
        self.get_json("/v1/memory/verify", &[])
    }

    fn list_prompts(&self, f: &PromptFilters, scope: &Scope) -> Result<Vec<LakeItem>, MemoryError> {
        let mut query: Vec<(&str, String)> = Vec::new();
        for (k, v) in [
            ("session", &f.session_id),
            ("mission", &f.mission_id),
            ("surface", &f.surface),
            ("project", &f.project),
            ("q", &f.substring),
            ("thread_kind", &f.thread_kind),
            ("thread_id", &f.thread_id),
            ("parent_session", &f.parent_session_id),
            ("model", &f.model),
            ("role", &f.role),
            ("principal", &f.principal), ("agent", &f.agent), ("run", &f.run), ("org", &f.org),
        ] {
            if let Some(v) = q(v) {
                query.push((k, v));
            }
        }
        if let Some(s) = f.since_seq {
            query.push(("since_seq", s.to_string()));
        }
        query.push(("limit", f.limit.to_string()));
        if f.include_agent {
            query.push(("include_agent", "1".into()));
        }
        add_scope(&mut query, scope)?;
        self.get_json::<Items<LakeItem>>("/v1/context/prompts", &query).map(|i| i.items)
    }

    fn browse_search(&self, query_text: &str, limit: i64, scope: &Scope) -> Result<Vec<BrowseHit>, MemoryError> {
        let mut query = vec![("q", query_text.to_string()), ("limit", limit.to_string())]; add_scope(&mut query, scope)?;
        self.get_json::<Items<BrowseHit>>("/v1/context/browse/search", &query)
            .map(|i| i.items)
    }

    fn thread_tree(&self, kind: &str, id: &str, scope: &Scope) -> Result<Value, MemoryError> {
        self.get_json(&format!("/v1/context/tree/{}/{}", segment(kind), segment(id)), &scope_query(scope)?)
    }

    fn thread(&self, kind: &str, id: &str, limit: i64, scope: &Scope) -> Result<Option<Value>, MemoryError> {
        let mut query = vec![("limit", limit.to_string())]; add_scope(&mut query, scope)?;
        match self.get_json::<Value>(&format!("/v1/context/threads/{}/{}", segment(kind), segment(id)), &query) {
            Ok(v) => Ok(Some(v)),
            Err(MemoryError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn context(&self, req: &ContextRequest) -> Result<ContextBlock, MemoryError> {
        let mut query = vec![("q", req.q.clone())];
        if let Some(v) = q(&req.node) {
            query.push(("node", v));
        }
        if let Some(m) = req.max_tokens {
            query.push(("max_tokens", m.to_string()));
        }
        add_scope(&mut query, &req.scope)?;
        add_filter(&mut query, &req.filter);
        if let Some(value) = req.max_bytes { query.push(("max_bytes", value.to_string())); }
        if let Some(value) = &req.trace_id { query.push(("trace_id", value.clone())); }
        self.get_json("/v1/memory/context", &query)
    }

    fn health(&self) -> Result<HealthReport, MemoryError> {
        self.get_json("/v1/memory/health", &[])
    }

    fn capture(&self, req: &CaptureRequest) -> Result<Option<i64>, MemoryError> {
        // The hook contract, as the hook itself speaks it.
        let body = serde_json::json!({
            "prompt": req.body,
            "session_id": req.session,
            "cwd": req.project,
        });
        let (_, v) = self.post_json("/v1/prompts/ingest", &body)?;
        Ok(v.get("seq").and_then(Value::as_i64))
    }

    // The writes (E2): the daemon's routes, with the daemon's token. Each
    // route answers the receipt type as JSON (201/200); a refusal is 400 with
    // a reason, mapped to `Rejected` by `map_status`.
    fn remember(&self, req: &RememberRequest) -> Result<WriteReceipt, MemoryError> {
        let (_, v) = self.post_json("/v1/memory/remember", &to_value(req))?;
        from_value(v, "remember")
    }
    fn ingest(&self, req: &IngestRequest) -> Result<IngestReceipt, MemoryError> {
        let (_, v) = self.post_json("/v1/memory/events", &to_value(req))?;
        from_value(v, "events")
    }
    fn annotate(&self, req: &AnnotateRequest) -> Result<WriteReceipt, MemoryError> {
        let (_, v) = self.post_json("/v1/memory/annotate", &to_value(req))?;
        from_value(v, "annotate")
    }
    fn forget(&self, req: &ForgetRequest) -> Result<ForgetReceipt, MemoryError> {
        let (_, v) = self.post_json("/v1/memory/forget", &to_value(req))?;
        from_value(v, "forget")
    }
    fn supersede(&self, req: &SupersedeRequest) -> Result<SupersedeReceipt, MemoryError> {
        let (_, v) = self.post_json("/v1/memory/supersede", &to_value(req))?;
        from_value(v, "supersede")
    }
    fn stage_proposals(&self, proposals: &[Proposal], actor: &str) -> Result<StageResult, MemoryError> {
        let body = serde_json::json!({"proposals":proposals.iter().map(proposal_value).collect::<Vec<_>>(),"actor":actor});
        let (_, value) = self.post_json("/v1/memory/proposals", &body)?;
        from_value(value.get("staged").cloned().unwrap_or(Value::Null), "proposals")
    }
    fn browse(&self, req: &BrowseRequest) -> Result<WriteReceipt, MemoryError> {
        let (_, value) = self.post_json("/v1/memory/browse", &to_value(req))?; from_value(value, "browse")
    }
    fn organize(&self, scope: &Scope) -> BoxFuture<'_, Result<OrganizeReceipt, MemoryError>> {
        let scope = scope.clone();
        Box::pin(async move { let (_, value) = self.post_json("/v1/memory/organize", &to_value(&scope))?; from_value(value, "organize") })
    }
    fn reindex(&self, scope: &Scope) -> Result<ReindexReceipt, MemoryError> {
        let (_, value) = self.post_json("/v1/memory/reindex", &to_value(scope))?; from_value(value, "reindex")
    }

    // --- B2: the runs (reads over the routes; the revert is a POST) ----------

    fn list_runs(&self, limit: i64, scope: &Scope) -> Result<Vec<ClassRun>, MemoryError> {
        let mut query = vec![("limit", limit.to_string())]; add_scope(&mut query, scope)?;
        let v: Value = self.get_json("/v1/memory/runs", &query)?;
        serde_json::from_value(v.get("runs").cloned().unwrap_or(Value::Array(vec![])))
            .map_err(|e| MemoryError::Store(format!("runs: unexpected body: {e}")))
    }

    fn run(&self, id: i64) -> Result<Option<RunView>, MemoryError> {
        match self.get_json::<RunView>(&format!("/v1/memory/runs/{id}"), &[]) {
            Ok(v) => Ok(Some(v)),
            Err(MemoryError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn revert_run(&self, id: i64) -> Result<RevertReceipt, MemoryError> {
        let (_, v) = self.post_json(&format!("/v1/memory/runs/{id}/revert"), &serde_json::json!({}))?;
        serde_json::from_value(v).map_err(|e| MemoryError::Store(format!("revert: unexpected body: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_wire_payload_is_accepted_by_the_http_parser() {
        let proposals = vec![
            Proposal::File { parent_id:"root".into(),sub_class:Some("sub".into()),target_kind:"prompt".into(),target_id:"42".into(),note:Some("note".into()),rationale:None },
            Proposal::Create { parent_id:"root".into(),title:"Title".into(),rationale:Some("because".into()) },
            Proposal::Promote { node_id:"child".into(),new_parent_id:None,rationale:None },
            Proposal::Split { node_id:"child".into(),into:vec![polis_core::proposal::SplitPart {title:"part".into(),link_ids:vec![1,2]}],rationale:None },
            Proposal::Merge {node_ids:vec!["a".into(),"b".into()],title:Some("Merged".into()),parent_id:None,rationale:None},
            Proposal::Collapse {node_id:"child".into(),summary:"Summary".into(),cite_seqs:vec![42],rationale:None},
            Proposal::Supersede {old_seq:1,new_seq:2,rationale:Some("updated".into())},
        ];
        let payload = serde_json::json!({"proposals":proposals.iter().map(proposal_value).collect::<Vec<_>>(),"actor":"agent"});
        assert_eq!(polis_core::proposal::parse_proposals(&payload.to_string()), proposals);
    }

    #[test]
    fn scope_and_filter_transport_keep_all_axes_and_reject_conflicts() {
        let scope = Scope { principal: Some("human".into()), agent: Some("agent".into()), run: Some("run".into()), org: Some("org".into()), project: Some("/project".into()), include_shared: true };
        let mut query = scope_query(&scope).unwrap();
        add_filter(&mut query, &EvidenceFilter { roles: vec!["assistant".into()], after: Some(1), before: Some(2), ..Default::default() });
        for key in ["principal", "agent", "run", "org", "project", "include_shared", "roles", "after", "before"] { assert!(query.iter().any(|(k,_)| *k == key), "{key}"); }
        let mut conflicting = vec![("project", "/other".into())];
        assert!(add_scope(&mut conflicting, &scope).is_err());
    }

    #[test]
    fn segments_are_percent_encoded_and_statuses_map_to_the_error_vocabulary() {
        assert_eq!(segment("a b/c"), "a%20b%2Fc");
        assert_eq!(segment("plain-id_1.x~"), "plain-id_1.x~");
        assert_eq!(RemoteApi::map_status(404, ""), MemoryError::NotFound);
        assert_eq!(RemoteApi::map_status(400, r#"{"error":"q is required"}"#), MemoryError::Rejected("q is required".into()));
        assert!(matches!(RemoteApi::map_status(503, "no model"), MemoryError::Unavailable(_)));
        assert!(matches!(RemoteApi::map_status(502, "x"), MemoryError::Store(_)));
    }

    #[test]
    fn an_unreachable_daemon_is_unavailable_not_a_panic() {
        let api = RemoteApi::new("http://127.0.0.1:1", None).with_timeout(Duration::from_millis(300));
        assert!(matches!(api.verify(), Err(MemoryError::Unavailable(_))));
        assert!(RemoteApi::probe("http://127.0.0.1:1", Duration::from_millis(300)).is_none());
    }
}
