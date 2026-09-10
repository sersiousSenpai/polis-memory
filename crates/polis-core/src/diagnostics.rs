// SPDX-License-Identifier: Apache-2.0
//! Portable diagnostics refer to evidence; they never duplicate source bodies.
use crate::{api::Scope, types::LakeItem};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DecisionRequest {
    pub source_seq: i64,
    /// decision (default), resolution, approval, or review_verdict.
    pub kind: Option<String>,
    pub scope: Scope,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EvidenceRequest {
    pub seq: i64,
    pub chain_id: Option<String>,
    pub scope: Scope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub seq: i64,
    pub chain_id: String,
    /// available, redacted, unavailable, or legacy_gap.
    pub status: String,
    pub item: Option<LakeItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TraceRequest {
    pub id: Option<String>,
    pub limit: Option<usize>,
    pub scope: Scope,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetrievalTrace {
    pub id: String,
    pub started_at: i64,
    pub elapsed_ms: f64,
    pub scope: Scope,
    pub head_seq: i64,
    pub snapshot_hash: String,
    pub config: serde_json::Value,
    pub coverage: serde_json::Value,
    pub selected_seqs: Vec<i64>,
    pub cuts: Vec<String>,
    pub errors: Vec<String>,
    /// Raw queries are off by default. Exact replay needs the query and the
    /// declared evidence/index snapshot; a ledger head alone is insufficient.
    pub replay: String,
}
