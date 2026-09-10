// SPDX-License-Identifier: Apache-2.0
//! Cited assertions with independent valid and recorded time. Claims remain
//! alternatives until an explicit, sufficiently authoritative supersession.

use crate::api::Scope;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimPredicate {
    Preference,
    ProjectConfiguration,
    SelectedTechnology,
    Constraint,
    Decision,
    Relationship,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum ClaimValue {
    Text(String),
    Boolean(bool),
    Number(f64),
}

impl ClaimValue {
    pub fn supporting_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Boolean(b) => b.to_string(),
            Self::Number(n) => n.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimRole {
    User,
    Assistant,
    Agent,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimDerivation {
    Structured,
    Gardener,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSource {
    /// Device chain ID; `local` only for evidence captured before identity.
    pub chain_id: String,
    pub seq: i64,
    pub role: ClaimRole,
    /// Exact supporting passage. This is verified against source content.
    pub quote: String,
    /// Optional UTF-8 byte offset, checked when supplied.
    pub start_byte: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimWrite {
    /// Caller-supplied stable ID, used for idempotent structured writes.
    pub id: String,
    pub subject: String,
    pub predicate: ClaimPredicate,
    pub value: ClaimValue,
    pub scope: Scope,
    pub sources: Vec<ClaimSource>,
    pub derivation: ClaimDerivation,
    pub valid_from: i64,
    /// Half-open valid interval [valid_from, valid_until).
    pub valid_until: Option<i64>,
    #[serde(default)]
    pub supersedes: Vec<String>,
    #[serde(default)]
    pub contradicts: Vec<String>,
    pub organizer_run: Option<i64>,
    pub model_version: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClaimQuery {
    pub q: Option<String>,
    pub evidence_filter: crate::api::EvidenceFilter,
    pub subject: Option<String>,
    pub predicate: Option<ClaimPredicate>,
    pub scope: Scope,
    /// Defaults to the query's wall-clock time.
    pub valid_at: Option<i64>,
    /// Defaults to the query's wall-clock time, independently of valid_at.
    pub known_at: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Claim {
    #[serde(flatten)]
    pub assertion: ClaimWrite,
    pub recorded_at: i64,
    pub event_seq: i64,
    /// Populated when another eligible assertion states a different value.
    pub unresolved_alternatives: Vec<String>,
}
