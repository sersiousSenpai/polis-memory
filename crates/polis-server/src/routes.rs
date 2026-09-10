// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The handlers. The first block is Redline's memory and context routes,
//! moved verbatim (Session A6) with their database calls rewritten onto the
//! [`MemoryApi`] — same query shapes, same response bodies, same status codes
//! (including the 502 `{error}` failure shape the browse routes gave them).
//! The second block is the plan's §4.4 additions.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use polis_core::api::{
    AnnotateRequest, BrowseRequest, ContextRequest, ForgetRequest, GrepRequest, IngestRequest,
    PromptsRequest, RememberRequest, Scope, SearchRequest, SupersedeRequest, TreeRequest,
};
use polis_core::host::Change;
use polis_core::pack::budgeted_item_count;
use polis_core::proposal::parse_proposals;
use polis_core::types::{
    clamp_prompt_limit, GrepScope, LedgerFilters, PromptFilters, StageResult, MAX_DELTA_ITEMS,
};
use polis_core::{MemoryApi, MemoryError};

use crate::PolisState;

/// 502 with `{error}` — the failure shape these routes have always had (they
/// were born beside the browse routes and share it), kept so no consumer
/// sees a new code for an old failure.
pub(crate) fn error_response(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_GATEWAY, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

/// A [`MemoryError`] as a response: a guardrail verdict is the caller's
/// mistake (400, with the reason — the agent's next move depends on reading
/// it), a missing thing is 404, a capability the install lacks is 503, and a
/// store fault keeps the routes' 502.
pub(crate) fn memory_error(e: MemoryError) -> Response {
    let (status, detail) = match e {
        MemoryError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
        MemoryError::Rejected(r) => (StatusCode::BAD_REQUEST, r),
        MemoryError::Unavailable(r) => (StatusCode::SERVICE_UNAVAILABLE, r),
        MemoryError::Store(r) => (StatusCode::BAD_GATEWAY, r),
    };
    (status, Json(serde_json::json!({ "error": detail }))).into_response()
}

/// Flat HTTP representation of the API's scope object.
#[derive(Clone, Default, Deserialize)]
pub struct ScopeQ {
    pub principal: Option<String>, pub org: Option<String>, pub agent: Option<String>, pub run: Option<String>, pub project: Option<String>,
    #[serde(default, deserialize_with = "query_bool")] pub include_shared: Option<bool>,
}
impl From<ScopeQ> for Scope {
    fn from(q: ScopeQ) -> Scope { Scope { principal: q.principal, org: q.org, agent: q.agent, run: q.run, project: q.project, include_shared: q.include_shared.unwrap_or(false) } }
}

#[derive(Default, Deserialize)]
pub struct FilterQ {
    roles: Option<String>,
    #[serde(default, deserialize_with = "query_i64")] after: Option<i64>,
    #[serde(default, deserialize_with = "query_i64")] before: Option<i64>,
    #[serde(default, deserialize_with = "query_i64")] valid_at: Option<i64>,
    #[serde(default, deserialize_with = "query_i64")] known_at: Option<i64>,
}
fn query_i64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Option::<String>::deserialize(deserializer)?.map(|s| s.parse().map_err(serde::de::Error::custom)).transpose()
}
fn query_bool<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    Option::<String>::deserialize(deserializer)?.map(|s| match s.as_str() { "true" | "1" => Ok(true), "false" | "0" => Ok(false), _ => Err(serde::de::Error::custom("expected true or false")) }).transpose()
}
impl From<FilterQ> for polis_core::api::EvidenceFilter {
    fn from(q: FilterQ) -> Self { Self { roles: q.roles.map(|v| v.split(',').map(|s| s.trim().to_string()).collect()).unwrap_or_default(), after: q.after, before: q.before, valid_at: q.valid_at, known_at: q.known_at } }
}

// ===========================================================================
// Moved from Redline's lib.rs — the memory routes
// ===========================================================================

#[derive(Deserialize)]
pub struct MemoryTreeQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    project: Option<String>,
    root: Option<String>,
}

/// `GET /v1/memory/tree?project=&root=` — the accepted (and proposed) class tree,
/// flat with link counts (the caller/FE builds the hierarchy). Scoped to a single
/// root subtree when `root=<id>` or `project=<path>` is given. Read-only.
pub async fn handle_memory_tree(
    State(state): State<PolisState>,
    Query(q): Query<MemoryTreeQ>,
) -> Response {
    match state.api.tree(&TreeRequest { root: q.root, project: q.project, scope: q.scope_filter.into() }) {
        Ok(views) => Json(serde_json::json!({ "nodes": views })).into_response(),
        Err(e) => memory_error(e),
    }
}

/// `GET /v1/memory/node/:id` — one node, its children, its links (pointers
/// into the lake, with resolved labels + supersession status), and its
/// observations. The retrieval agent's descend step.
pub async fn handle_memory_node(
    State(state): State<PolisState>,
    Path(id): Path<String>,
    Query(scope): Query<ScopeQ>,
) -> Response {
    match state.api.node(&id, &scope.into()) {
        Ok(Some(view)) => Json(view).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no such class node").into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(Deserialize)]
pub struct AnswerPackQ {
    #[serde(flatten)] filter: FilterQ,
    candidate_limit: Option<usize>, max_tokens: Option<usize>, cursor: Option<String>, trace_id: Option<String>,
    #[serde(flatten)] scope_filter: ScopeQ,
    q: Option<String>,
    node: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/memory/answer-pack?q=&node=&limit=` — the retrieval agent's ONE
/// call. Resolves the question to a class node (explicitly via `?node=`, else
/// by best title match) and returns that node's subtree, links (labelled, with
/// `supersededBy`) and observations, together with the user's matching notes,
/// matching lake prompts and matching browsed pages.
///
/// It replaces a 5–7 turn walk, so it must never send the agent back into one:
/// the lexical arms are populated from `?q=` regardless of whether a node
/// resolved, and a stale `?node=` degrades to the best match instead of an
/// empty answer. Read-only, byte-bounded.
pub async fn handle_memory_answer_pack(
    State(state): State<PolisState>,
    Query(q): Query<AnswerPackQ>,
) -> Response {
    let api = state.api.clone();
    let req = SearchRequest { q: q.q, node: q.node, limit: q.limit, scope: q.scope_filter.into(), filter: q.filter.into(), candidate_limit: q.candidate_limit, max_tokens: q.max_tokens, cursor: q.cursor, trace_id: q.trace_id };
    // Assembly touches several tables under the DB lock — off the async
    // executor's thread, like every other heavy bridge read.
    let pack = tokio::task::spawn_blocking(move || api.search(&req)).await;
    match pack {
        Ok(Ok(pack)) => Json(pack).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("answer-pack assembly failed: {e}")),
    }
}

#[derive(Deserialize)]
pub struct MemoryGrepQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    q: Option<String>,
    re: Option<String>,
    case: Option<String>,
    scope: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/memory/grep?q=&re=&case=&scope=&limit=` — literal and regex search
/// over the record, for the things tokenization cannot reach: flags
/// (`--allowedTools`), paths (`src-tauri/src/db.rs`), error strings,
/// attributes (`#[serde(rename_all)]`).
///
/// `q` is a substring answered from a trigram index and must be at least
/// `GREP_MIN_LITERAL` characters — shorter is refused by name rather than
/// silently turned into a scan. `re` is applied in Rust to what the index
/// returned, so a pathological pattern costs one pass over the candidates
/// instead of a walk of the corpus under the DB lock.
pub async fn handle_memory_grep(
    State(state): State<PolisState>,
    Query(q): Query<MemoryGrepQ>,
) -> Response {
    let literal = q.q.unwrap_or_default();
    let case_sensitive = matches!(q.case.as_deref(), Some("1") | Some("true"));
    let scope = GrepScope::parse(q.scope.as_deref());
    let limit = q.limit.unwrap_or(30);
    let re = q.re;
    let api = state.api.clone();
    let hits = tokio::task::spawn_blocking(move || {
        api.grep(&GrepRequest {
            literal,
            regex: re,
            case_sensitive,
            kinds: scope,
            limit: Some(limit),
            scope: q.scope_filter.into(),
        })
    })
    .await;
    match hits {
        Ok(Ok(hits)) => Json(serde_json::json!({ "hits": hits })).into_response(),
        // A refusal is a 400 WITH its reason in the body: the agent's next move
        // ("lengthen the needle") is only available if it can read why.
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("grep failed: {e}")),
    }
}

#[derive(Deserialize)]
pub struct MemoryPromptsQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    since_seq: Option<i64>,
    limit: Option<i64>,
}

/// `GET /v1/memory/prompts?since_seq=&limit=` — the lake delta (prompts +
/// decision events) since a seq, oldest first. The classifier's delta input;
/// also a general context read. Bounded.
pub async fn handle_memory_prompts(
    State(state): State<PolisState>,
    Query(q): Query<MemoryPromptsQ>,
) -> Response {
    let limit = q.limit.unwrap_or(200).clamp(1, MAX_DELTA_ITEMS as i64);
    let since = q.since_seq.unwrap_or(0).max(0);
    match state.api.prompts(&PromptsRequest { since_seq: since, limit: Some(limit), scope: q.scope_filter.into() }) {
        Ok(mut items) => {
            // Byte-budget the response, the way `/v1/context/prompts` does.
            // The item count was capped but the BYTES were not, and this
            // route's body column is `COALESCE(p.body, be.text, un.text)` —
            // `be.text` is a whole normalized page, so 400 browse items could
            // serialize megabytes under the DB lock. The route is live on the
            // MCP proxy, so a remote caller could ask for that at will.
            let kept = budgeted_item_count(items.iter().map(|i| i.body.as_deref()));
            items.truncate(kept);
            Json(serde_json::json!({ "items": items })).into_response()
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/proposals` {proposals:[…]} — stage a batch of classifier
/// proposals as reviewable rows. **Staging only** — nothing is accepted or
/// moved. Mirrors the parse the internal Organize path uses, so an external tool
/// (or the classifier itself) can stage over the curl bridge.
pub async fn handle_memory_proposals(
    State(state): State<PolisState>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > 256_000 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "proposals payload too large").into_response();
    }
    let text = String::from_utf8_lossy(&body);
    let proposals = parse_proposals(&text);
    if proposals.is_empty() {
        return Json(serde_json::json!({ "ok": true, "staged": StageResult::default() }))
            .into_response();
    }
    let payload = serde_json::from_slice::<serde_json::Value>(&body).ok();
    let actor = payload.as_ref().and_then(|v| v.get("actor")).and_then(serde_json::Value::as_str).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("api");
    match state.api.stage_proposals(&proposals, actor) {
        Ok(staged) => {
            state.events.changed(&[Change::Catalog]);
            Json(serde_json::json!({ "ok": true, "staged": staged })).into_response()
        }
        Err(e) => memory_error(e),
    }
}

#[derive(Deserialize)]
pub struct ContextPromptsQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    session: Option<String>,
    mission: Option<String>,
    surface: Option<String>,
    project: Option<String>,
    since_seq: Option<i64>,
    /// Free-text substring — bound as a LIKE parameter in the DB layer.
    q: Option<String>,
    limit: Option<i64>,
    thread_kind: Option<String>,
    thread_id: Option<String>,
    parent_session: Option<String>,
    /// Exact-match filter on the recorded model (`prompts.model`).
    model: Option<String>,
    /// Corpus role: `user` (default view) | `agent` | `system`.
    role: Option<String>,
    /// Opt in to the host's own constructed prefaces, which are excluded by
    /// default. `1`/`true` to include.
    include_agent: Option<String>,
}

/// `GET /v1/context/prompts?session=&mission=&surface=&project=&since_seq=&q=&limit=&role=&include_agent=`
/// — filtered read of the captured-prompt lake. Every filter is ANDed; `q` is
/// planned through the FTS index (AND→OR→LIKE cascade). `agent` rows are
/// excluded unless asked for. Oldest-first, byte-bounded. Read-only.
pub async fn handle_context_prompts(
    State(state): State<PolisState>,
    Query(q): Query<ContextPromptsQ>,
) -> Response {
    let scope: Scope = q.scope_filter.clone().into();
    let filters = PromptFilters {
        session_id: q.session,
        mission_id: q.mission,
        surface: q.surface,
        project: q.project,
        since_seq: q.since_seq,
        substring: q.q,
        limit: clamp_prompt_limit(q.limit),
        thread_kind: q.thread_kind,
        thread_id: q.thread_id,
        parent_session_id: q.parent_session,
        model: q.model,
        role: q.role,
        include_agent: matches!(q.include_agent.as_deref(), Some("1") | Some("true")),
        principal: q.scope_filter.principal,
        agent: q.scope_filter.agent,
        run: q.scope_filter.run,
        org: q.scope_filter.org,
    };
    match state.api.list_prompts(&filters, &scope) {
        Ok(items) => Json(serde_json::json!({ "items": items })).into_response(),
        Err(e) => memory_error(e),
    }
}

/// `GET /v1/context/stats` — aggregate counts (per day / surface / kind /
/// class / author). Read-only.
pub async fn handle_context_stats(State(state): State<PolisState>, Query(scope): Query<ScopeQ>) -> Response {
    match state.api.stats(&scope.into()) {
        Ok(stats) => Json(stats).into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(Deserialize)]
pub struct ContextThreadQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    limit: Option<i64>,
}

/// `GET /v1/context/threads/:kind/:id?limit=` — generic read-only fetch of any
/// surface's discussion thread (browse / linked / mission / companion / drafter
/// / a plan session's comment threads), tail-bounded, oldest-first.
pub async fn handle_context_thread(
    State(state): State<PolisState>,
    Path((kind, id)): Path<(String, String)>,
    Query(q): Query<ContextThreadQ>,
) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    match state.api.thread(&kind, &id, limit, &q.scope_filter.into()) {
        Ok(Some(view)) => Json(view).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "unknown thread kind").into_response(),
        Err(e) => memory_error(e),
    }
}

/// `GET /v1/context/tree/:kind/:id` — one session-tree node with its parent and
/// child digests (message counts + recency), the traversable spine of
/// memory-by-session. Read-only.
pub async fn handle_context_tree(
    State(state): State<PolisState>,
    Path((kind, id)): Path<(String, String)>,
    Query(scope): Query<ScopeQ>,
) -> Response {
    match state.api.thread_tree(&kind, &id, &scope.into()) {
        Ok(tree) => Json(tree).into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(Deserialize)]
pub struct BrowseSearchQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    /// Free-text query — tokenized + quoted into a safe FTS5 MATCH in the DB.
    q: Option<String>,
    limit: Option<i64>,
}

/// `GET /v1/context/browse/search?q=&limit=` — lexical (BM25) search over
/// the browsing-behavior stream. High-volume, keyword-heavy browse events get
/// fuzzy full-text recall here (plans/prompts stay on the vectorless walk).
/// Read-only; returns `{items:[{id,ts,url,title,snippet,score}]}` best-first.
pub async fn handle_browse_search(
    State(state): State<PolisState>,
    Query(q): Query<BrowseSearchQ>,
) -> Response {
    let query = q.q.unwrap_or_default();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    match state.api.browse_search(&query, limit, &q.scope_filter.into()) {
        Ok(items) => Json(serde_json::json!({ "items": items })).into_response(),
        Err(e) => memory_error(e),
    }
}

// ===========================================================================
// New in A6 — the plan's §4.4 reads and writes
// ===========================================================================

#[derive(Deserialize)]
pub struct ContextQ {
    #[serde(flatten)] filter: FilterQ,
    max_bytes: Option<usize>, trace_id: Option<String>,
    #[serde(flatten)] scope_filter: ScopeQ,
    q: Option<String>,
    node: Option<String>,
    max_tokens: Option<usize>,
}

/// `GET /v1/memory/context?q=&node=&max_tokens=` — the answer pack rendered as
/// one grounding block (the discussion prefetch's shape), for a model to be
/// handed verbatim. `q` is required: the block is about a question.
pub async fn handle_memory_context(
    State(state): State<PolisState>,
    Query(q): Query<ContextQ>,
) -> Response {
    let Some(question) = q.q.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": "q is required" })))
            .into_response();
    };
    let api = state.api.clone();
    let req = ContextRequest { q: question, node: q.node, max_tokens: q.max_tokens, scope: q.scope_filter.into(), filter: q.filter.into(), max_bytes: q.max_bytes, trace_id: q.trace_id };
    match tokio::task::spawn_blocking(move || api.context(&req)).await {
        Ok(Ok(block)) => Json(block).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("context assembly failed: {e}")),
    }
}

/// The timeline's facets as a query string. `LedgerFilters` itself is the
/// command/IPC shape (a JSON body with a `seqs` array); a URL carries the
/// same axes flat, with `seqs` comma-separated.
#[derive(Deserialize)]
pub struct LedgerQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    kind: Option<String>,
    author: Option<String>,
    session: Option<String>,
    surface: Option<String>,
    project: Option<String>,
    q: Option<String>,
    since_ts: Option<i64>,
    until_ts: Option<i64>,
    before_seq: Option<i64>,
    limit: Option<i64>,
    starred: Option<String>,
    noted: Option<String>,
    seqs: Option<String>,
    class_node: Option<String>,
    thread_id: Option<String>,
    browse_id: Option<String>,
    role: Option<String>,
}

fn flag(v: Option<&str>) -> Option<bool> {
    matches!(v, Some("1") | Some("true")).then_some(true)
}

/// `GET /v1/memory/ledger?…` — a faceted timeline page, newest first; the next
/// page's cursor is the last row's seq as `before_seq`.
pub async fn handle_memory_ledger(
    State(state): State<PolisState>,
    Query(q): Query<LedgerQ>,
) -> Response {
    let scope: Scope = q.scope_filter.clone().into();
    let seqs: Option<Vec<i64>> = q.seqs.map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect());
    let filters = LedgerFilters {
        kind: q.kind,
        author: q.author,
        session_id: q.session,
        surface: q.surface,
        project: q.project,
        q: q.q,
        since_ts: q.since_ts,
        until_ts: q.until_ts,
        before_seq: q.before_seq,
        limit: q.limit,
        starred: flag(q.starred.as_deref()),
        noted: flag(q.noted.as_deref()),
        seqs: seqs.filter(|v| !v.is_empty()),
        class_node: q.class_node,
        thread_id: q.thread_id,
        browse_id: q.browse_id,
        role: q.role,
        principal: q.scope_filter.principal,
    };
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.timeline(&filters, &scope)).await {
        Ok(Ok(items)) => Json(serde_json::json!({ "items": items })).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("timeline query failed: {e}")),
    }
}

/// `GET /v1/memory/verify` — re-walk the chain genesis→head.
pub async fn handle_memory_verify(State(state): State<PolisState>) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.verify()).await {
        Ok(Ok(verdict)) => Json(verdict).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("verify failed: {e}")),
    }
}

/// `GET /v1/memory/health` — intactness and capability in one read. The
/// no-model install reports `model: null`; nothing here is an error state.
pub async fn handle_memory_health(State(state): State<PolisState>) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.health()).await {
        Ok(Ok(report)) => Json(report).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("health failed: {e}")),
    }
}

/// `GET /v1/memory/map` — classes + threads with declared edge kinds.
pub async fn handle_memory_map(State(state): State<PolisState>, Query(scope): Query<ScopeQ>) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.map(&scope.into())).await {
        Ok(Ok(map)) => Json(map).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("map failed: {e}")),
    }
}

/// 201 when the write appended a row, 200 when it was a no-op (a dedup, an
/// unchanged note) — the receipt says which either way.
fn write_response<T: serde::Serialize>(created: bool, receipt: T) -> Response {
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    (status, Json(receipt)).into_response()
}

/// `POST /v1/memory/remember` — one memory: the user's own words as a prompt
/// row (`asUser`), or a standalone note.
pub async fn handle_memory_remember(
    State(state): State<PolisState>,
    Json(req): Json<RememberRequest>,
) -> Response {
    match state.api.remember(&req) {
        Ok(receipt) => {
            if receipt.seq.is_some() {
                state.events.changed(&[Change::Ledger]);
            }
            write_response(receipt.seq.is_some(), receipt)
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/annotate` — a note on a seq, a node or a session.
pub async fn handle_memory_annotate(
    State(state): State<PolisState>,
    Json(req): Json<AnnotateRequest>,
) -> Response {
    match state.api.annotate(&req) {
        Ok(receipt) => {
            if receipt.seq.is_some() {
                state.events.changed(&[Change::Ledger]);
            }
            write_response(receipt.seq.is_some(), receipt)
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/forget` — the one destructive verb. Refused (400) without
/// the literal `confirm: "forget"`.
pub async fn handle_memory_forget(
    State(state): State<PolisState>,
    Json(req): Json<ForgetRequest>,
) -> Response {
    match state.api.forget(&req) {
        Ok(receipt) => {
            if receipt.seq.is_some() {
                state.events.changed(&[Change::Ledger, Change::Memory]);
            }
            Json(receipt).into_response()
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/supersede` (E2) — a newer decision replaces an older one
/// on the same subject; never deletes. A guardrail refusal is data
/// (`applied: false`, `rejected`), not an error.
pub async fn handle_memory_supersede(
    State(state): State<PolisState>,
    Json(req): Json<SupersedeRequest>,
) -> Response {
    match state.api.supersede(&req) {
        Ok(receipt) => {
            if receipt.applied {
                state.events.changed(&[Change::Ledger, Change::Memory]);
            }
            Json(receipt).into_response()
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/events` — a batch import with its own clock, idempotent on
/// `(body hash, run)`.
pub async fn handle_memory_events(
    State(state): State<PolisState>,
    Json(req): Json<IngestRequest>,
) -> Response {
    match state.api.ingest(&req) {
        Ok(receipt) => {
            let created = !receipt.recorded.is_empty();
            if created {
                state.events.changed(&[Change::Ledger]);
            }
            write_response(created, receipt)
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/browse` — one browsing event into the lake.
pub async fn handle_memory_browse(
    State(state): State<PolisState>,
    Json(req): Json<BrowseRequest>,
) -> Response {
    match state.api.browse(&req) {
        Ok(receipt) => {
            if receipt.seq.is_some() {
                state.events.changed(&[Change::Ledger]);
            }
            write_response(receipt.seq.is_some(), receipt)
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/organize` — one classifier pass, now. Drives the model;
/// 503 when the install has none.
pub async fn handle_memory_organize(State(state): State<PolisState>, scope: Option<Json<Scope>>) -> Response {
    let scope = scope.map(|s| s.0).unwrap_or_default();
    match state.api.organize(&scope).await {
        Ok(receipt) => {
            if receipt.ran {
                state.events.changed(&[Change::Catalog, Change::Ledger]);
            }
            Json(receipt).into_response()
        }
        Err(e) => memory_error(e),
    }
}

/// `POST /v1/memory/reindex` — embed one call's worth of the semantic backlog.
pub async fn handle_memory_reindex(State(state): State<PolisState>, scope: Option<Json<Scope>>) -> Response {
    let scope = scope.map(|s| s.0).unwrap_or_default();
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.reindex(&scope)).await {
        Ok(Ok(receipt)) => {
            if receipt.embedded > 0 {
                state.events.changed(&[Change::Embeddings]);
            }
            Json(receipt).into_response()
        }
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("reindex failed: {e}")),
    }
}

// ===========================================================================
// B2 — the gardener's runs, reversible (§5.2)
// ===========================================================================

#[derive(Deserialize)]
pub struct RunsQ {
    #[serde(flatten)] scope_filter: ScopeQ,
    limit: Option<i64>,
}

/// `GET /v1/memory/runs?limit=` — the newest runs, newest first, every mode
/// (organize / compaction / observations / revert / curation): the timeline.
pub async fn handle_memory_runs(State(state): State<PolisState>, Query(q): Query<RunsQ>) -> Response {
    let api = state.api.clone();
    let limit = q.limit.unwrap_or(50);
    match tokio::task::spawn_blocking(move || api.list_runs(limit, &q.scope_filter.into())).await {
        Ok(Ok(runs)) => Json(serde_json::json!({ "runs": runs })).into_response(),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("runs query failed: {e}")),
    }
}

/// `GET /v1/memory/runs/:id` — one run with its journaled ops (no image
/// blobs): what it did, to what, and whether it was undone.
pub async fn handle_memory_run(State(state): State<PolisState>, Path(id): Path<i64>) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.run(id)).await {
        Ok(Ok(Some(view))) => Json(view).into_response(),
        Ok(Ok(None)) => memory_error(MemoryError::NotFound),
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("run query failed: {e}")),
    }
}

/// `POST /v1/memory/runs/:id/revert` — undo one run in one transaction;
/// appends `gardener_revert`. 400 with the reason when a later run touched
/// the same subjects ("revert run N first"), when the run is past the
/// vacuum horizon, or when an op cannot be inverted. A GUI/HTTP action
/// only — deliberately not an MCP tool (§5.4).
pub async fn handle_memory_run_revert(State(state): State<PolisState>, Path(id): Path<i64>) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.revert_run(id)).await {
        Ok(Ok(receipt)) => {
            state.events.changed(&[Change::Catalog, Change::Ledger, Change::Memory]);
            Json(receipt).into_response()
        }
        Ok(Err(e)) => memory_error(e),
        Err(e) => error_response(format!("revert failed: {e}")),
    }
}

// The trait is named in the module docs; keep the import honest under
// `#![deny(unused)]`-style builds.
#[allow(dead_code)]
fn _uses_trait(_: &dyn MemoryApi) {}

#[derive(Deserialize)]
pub struct EvidenceQ { chain_id: Option<String>, #[serde(flatten)] scope: ScopeQ }

pub async fn handle_memory_evidence(State(state): State<PolisState>, Path(seq): Path<i64>, Query(q): Query<EvidenceQ>) -> Response {
    let req = polis_core::diagnostics::EvidenceRequest { seq, chain_id: q.chain_id, scope: q.scope.into() };
    match state.api.evidence(&req) { Ok(record) => Json(record).into_response(), Err(error) => memory_error(error) }
}

#[derive(Deserialize)]
pub struct TraceQ { id: Option<String>, limit: Option<usize>, #[serde(flatten)] scope: ScopeQ }

pub async fn handle_memory_traces(State(state): State<PolisState>, Query(q): Query<TraceQ>) -> Response {
    let req = polis_core::diagnostics::TraceRequest { id: q.id, limit: q.limit, scope: q.scope.into() };
    match state.api.traces(&req) { Ok(rows) => Json(serde_json::json!({"traces": rows})).into_response(), Err(error) => memory_error(error) }
}

#[cfg(test)]
mod scope_query_tests {
    use super::*;
    #[test]
    fn flat_transport_parameters_preserve_scope_roles_time_and_budgets() {
        let uri = "/?q=quartz&principal=human&agent=agent&run=run&org=org&project=%2Fproject&include_shared=true&roles=assistant,user&after=10&before=20&candidate_limit=100&max_tokens=300&trace_id=trace".parse().unwrap();
        let Query(q) = Query::<AnswerPackQ>::try_from_uri(&uri).unwrap();
        let scope: Scope = q.scope_filter.into();
        assert_eq!(scope.project.as_deref(), Some("/project"));
        assert_eq!(scope.principal.as_deref(), Some("human"));
        assert_eq!(scope.agent.as_deref(), Some("agent"));
        assert_eq!(scope.run.as_deref(), Some("run"));
        assert_eq!(scope.org.as_deref(), Some("org"));
        assert!(scope.include_shared);
        let filter: polis_core::api::EvidenceFilter = q.filter.into();
        assert_eq!(filter.roles, vec!["assistant", "user"]);
        assert_eq!((filter.after, filter.before), (Some(10), Some(20)));
        assert_eq!((q.candidate_limit, q.max_tokens), (Some(100), Some(300)));
        assert_eq!(q.trace_id.as_deref(), Some("trace"));
    }
}
