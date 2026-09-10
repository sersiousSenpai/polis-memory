// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Tool parameters — the JSON shapes a client sends. Snake_case keys, every
//! field optional unless the tool cannot run without it, and one `scope`
//! object on every read (§4.4): `{principal, org, agent, run, project,
//! include_shared}`. Explicit scope fields narrow eligible evidence before
//! retrieval limits.

use polis_core::api::Scope;
use polis_core::types::{GrepScope, LedgerFilters, PromptFilters};
use schemars::JsonSchema;
use serde::Deserialize;

/// Who is asking, over whose memory. Every field optional; an empty scope is
/// "this principal, everything local".
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ScopeArg {
    pub principal: Option<String>,
    pub org: Option<String>,
    pub agent: Option<String>,
    pub run: Option<String>,
    /// Absolute project path to scope the read to.
    pub project: Option<String>,
    /// Widen the read to imported foreign chains (never the default).
    pub include_shared: Option<bool>,
}

impl From<Option<ScopeArg>> for ScopeWrap {
    fn from(v: Option<ScopeArg>) -> Self {
        let a = v.unwrap_or_default();
        ScopeWrap(Scope {
            principal: a.principal,
            org: a.org,
            agent: a.agent,
            run: a.run,
            project: a.project,
            include_shared: a.include_shared.unwrap_or(false),
        })
    }
}

/// A newtype so `Option<ScopeArg>` converts without an orphan impl.
pub struct ScopeWrap(pub Scope);

pub fn scope(v: Option<ScopeArg>) -> Scope {
    ScopeWrap::from(v).0
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct FilterArg {
    #[serde(default)] pub roles: Vec<String>,
    pub after: Option<i64>, pub before: Option<i64>, pub valid_at: Option<i64>, pub known_at: Option<i64>,
}
impl From<FilterArg> for polis_core::api::EvidenceFilter {
    fn from(f: FilterArg) -> Self { Self { roles: f.roles, after: f.after, before: f.before, valid_at: f.valid_at, known_at: f.known_at } }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct EvidenceParams { pub seq: i64, pub chain_id: Option<String>, pub scope: Option<ScopeArg> }
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct TraceParams { pub id: Option<String>, pub limit: Option<usize>, pub scope: Option<ScopeArg> }

/// `memory_search` — START HERE.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SearchParams {
    pub filter: Option<FilterArg>,
    pub candidate_limit: Option<usize>, pub max_tokens: Option<usize>, pub cursor: Option<String>, pub trace_id: Option<String>,
    /// The question, in natural language. Stopwords are dropped and terms
    /// are stemmed; quoted "phrases" are matched adjacently.
    pub q: Option<String>,
    /// A class node id to open directly, when you already know which class.
    pub node: Option<String>,
    /// Hits per arm (1..60, default 20).
    pub limit: Option<i64>,
    pub scope: Option<ScopeArg>,
}

/// `memory_context` — the pack rendered as one grounding block.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ContextParams {
    pub filter: Option<FilterArg>,
    pub max_bytes: Option<usize>, pub trace_id: Option<String>,
    /// The question the block should ground.
    pub q: String,
    /// A class node id to open directly.
    pub node: Option<String>,
    /// Token budget for the block (default 2000, ~4 bytes a token).
    pub max_tokens: Option<usize>,
    pub scope: Option<ScopeArg>,
}

/// `memory_grep` — literal / regex over the trigram index.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct GrepParams {
    /// Substring to find (3+ characters).
    pub q: String,
    /// Optional regex applied to the indexed candidates.
    pub re: Option<String>,
    /// Case-sensitive (default false).
    pub case_sensitive: Option<bool>,
    /// `all` | `prompts` | `browse` (default all).
    pub kinds: Option<String>,
    /// Max hits (1..200, default 30).
    pub limit: Option<i64>,
    pub scope: Option<ScopeArg>,
}

/// `memory_tree` — the catalog, flat with link counts.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct TreeParams {
    /// Scope to the root class bound to this project path.
    pub project: Option<String>,
    /// Scope to this class node id and its descendants.
    pub root: Option<String>,
    pub scope: Option<ScopeArg>,
}

/// `memory_node` — one class with its children, links and observations.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct NodeParams {
    /// The class node id.
    pub id: String,
    pub scope: Option<ScopeArg>,
}

/// `memory_timeline` — a faceted slice of the ledger, newest first.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct TimelineParams {
    /// Event kind (`prompt`, `resolution`, `approval`, `taxonomy_reorg`, …).
    pub kind: Option<String>,
    pub author: Option<String>,
    /// A session id.
    pub session: Option<String>,
    pub surface: Option<String>,
    /// Absolute project path.
    pub project: Option<String>,
    /// Free-text filter over bodies.
    pub q: Option<String>,
    pub since_ts: Option<i64>,
    pub until_ts: Option<i64>,
    /// Page backwards from this seq.
    pub before_seq: Option<i64>,
    /// Page size (default 50, max 500).
    pub limit: Option<i64>,
    /// Exact seqs to fetch.
    pub seqs: Option<Vec<i64>>,
    /// Only events filed under this class node.
    pub class_node: Option<String>,
    pub thread_id: Option<String>,
    pub browse_id: Option<String>,
    /// `user` | `agent` | `system`.
    pub role: Option<String>,
    pub scope: Option<ScopeArg>,
}

impl TimelineParams {
    pub fn filters(&self) -> LedgerFilters {
        LedgerFilters {
            kind: self.kind.clone(),
            author: self.author.clone(),
            session_id: self.session.clone(),
            surface: self.surface.clone(),
            project: self.project.clone(),
            q: self.q.clone(),
            since_ts: self.since_ts,
            until_ts: self.until_ts,
            before_seq: self.before_seq,
            limit: self.limit,
            starred: None,
            noted: None,
            seqs: self.seqs.clone().filter(|v| !v.is_empty()),
            class_node: self.class_node.clone(),
            thread_id: self.thread_id.clone(),
            browse_id: self.browse_id.clone(),
            role: self.role.clone(),
            // the identity scope rides in `scope`, merged by the handle
            principal: None,
        }
    }
}

/// `memory_stats`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct StatsParams {
    pub scope: Option<ScopeArg>,
}

// --- the compat aliases' shapes: the eight tool names a Redline install
// already teaches its external sessions (`context-analysis`, `sensei`), kept
// for one release with their original keys ----------------------------------

/// `answer_pack` (alias of `memory_search`).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct AnswerPackParams {
    pub q: Option<String>,
    pub node: Option<String>,
    pub limit: Option<i64>,
}

/// `grep_memory` (alias of `memory_grep`; `case`/`scope` are its old keys).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct GrepMemoryParams {
    /// Substring to find (3+ characters).
    pub q: String,
    pub re: Option<String>,
    /// Case-sensitive (default false).
    pub case: Option<bool>,
    /// `prompts` | `browse` | `all`.
    pub scope: Option<String>,
    pub limit: Option<i64>,
}

/// `search_memory` (alias of `memory_search` with `q` only).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SearchMemoryParams {
    pub q: String,
    pub limit: Option<i64>,
}

/// `query_prompts` (the filtered lake read).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct QueryPromptsParams {
    pub session_id: Option<String>,
    pub mission_id: Option<String>,
    pub surface: Option<String>,
    pub project: Option<String>,
    pub since_seq: Option<i64>,
    /// Free-text query; all words must match (quote a "phrase" for adjacency).
    pub q: Option<String>,
    /// `user` | `agent` | `system`.
    pub role: Option<String>,
    /// Include constructed agent prompts (off by default).
    pub include_agent: Option<bool>,
    /// Max items (1..200, default 200).
    pub limit: Option<i64>,
}

impl QueryPromptsParams {
    pub fn filters(&self) -> PromptFilters {
        PromptFilters {
            session_id: self.session_id.clone(),
            mission_id: self.mission_id.clone(),
            surface: self.surface.clone(),
            project: self.project.clone(),
            since_seq: self.since_seq,
            substring: self.q.clone(),
            limit: self.limit.unwrap_or(200).clamp(1, 200),
            thread_kind: None,
            thread_id: None,
            parent_session_id: None,
            model: None,
            role: self.role.clone(),
            include_agent: self.include_agent.unwrap_or(false),
            principal: None,
            agent: None,
            run: None,
            org: None,
        }
    }
}

/// `session_history` (a host thread: `thread("session", id)`).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SessionHistoryParams {
    pub session_id: String,
    pub limit: Option<i64>,
}

/// `search_browsing` (lexical search over the browsing stream).
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SearchBrowsingParams {
    pub q: String,
    /// Max hits (1..100, default 20).
    pub limit: Option<i64>,
}

pub fn grep_scope(s: Option<&str>) -> GrepScope {
    GrepScope::parse(s)
}

// ---------------------------------------------------------------------------
// The writes (E2). Every one takes the same optional `scope`; the server
// stamps `scope.agent = mcp:<client>` when the caller names no agent.
// ---------------------------------------------------------------------------

/// `memory_remember`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct RememberParams {
    /// The memory to keep.
    pub text: String,
    /// Record as the user's own words. Default `false`: assistant evidence.
    pub as_user: Option<bool>,
    /// The project it belongs to (a path or a name).
    pub project: Option<String>,
    pub scope: Option<ScopeArg>,
}

/// One episode / message in a `memory_ingest` batch.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct IngestItemArg {
    pub body: String,
    /// Unix milliseconds when it happened (default: now).
    pub ts: Option<i64>,
    /// `user` | `assistant` | `agent` | `system` (default `user`).
    pub role: Option<String>,
    pub session: Option<String>,
    /// The run/thread id; retries deduplicate within the complete source namespace.
    pub run: Option<String>,
    pub project: Option<String>,
}

/// `memory_ingest`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct IngestParams {
    pub items: Vec<IngestItemArg>,
    pub scope: Option<ScopeArg>,
}

/// `memory_annotate`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct AnnotateParams {
    /// `ledger_event` | `class_node` | `session` | `none` (a standalone note).
    pub target_kind: String,
    /// The event seq / node id / session id; omit for `none`.
    pub target_id: Option<String>,
    pub text: String,
    pub scope: Option<ScopeArg>,
}

/// `memory_forget`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ForgetParams {
    /// `ledger_event` for a citation sequence; `prompt`, `browse_event`, `note` or `user_note` for a source row ID.
    pub target_kind: String,
    pub target_id: String,
    /// Must be the literal `forget`.
    pub confirm: String,
    pub scope: Option<ScopeArg>,
}

/// `memory_supersede`.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct SupersedeParams {
    pub old_seq: i64,
    pub new_seq: i64,
    pub rationale: Option<String>,
    pub scope: Option<ScopeArg>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct DecisionParams { pub source_seq: i64, pub kind: Option<String>, pub scope: Option<ScopeArg> }
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
pub struct ClaimsParams { pub filter: Option<FilterArg>, pub q: Option<String>, pub subject: Option<String>, pub predicate: Option<String>, pub valid_at: Option<i64>, pub known_at: Option<i64>, pub limit: Option<usize>, pub scope: Option<ScopeArg> }
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ClaimWriteParams {
    /// A ClaimWrite object matching /v1/memory/schema, with sources and exact supporting quotations.
    pub claim: serde_json::Value,
}
