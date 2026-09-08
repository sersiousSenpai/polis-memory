// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The org node's relay surface (plan §4.6, Session E4), as vocabulary.
//!
//! An org node stores and relays signed segments between peers, verifies
//! each on receipt, and is never a trust root — every peer re-verifies on
//! import. polis-server's `/v1/sync/*` routes speak to a [`SyncRelay`]; the
//! standalone daemon implements it over its store and its segment folder,
//! and a host that is not an org node installs [`NoSyncRelay`] so the
//! routes answer "not an org node" rather than not existing.
//!
//! Pure: the envelope travels as JSON (`serde_json::Value`). Its shape and
//! its verifier live beside the store, never here.

use serde::{Deserialize, Serialize};

/// The node itself: which principal relays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeCard {
    pub principal_id: String,
    pub chain_id: String,
    pub display_name: Option<String>,
    pub fingerprint: String,
}

/// A chain the node holds segments of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainSummary {
    pub chain_id: String,
    pub human_id: String,
    pub display_name: Option<String>,
    pub device_name: Option<String>,
    /// The newest `to_seq` the node holds.
    pub head_seq: i64,
    /// The node refused a rewritten segment of this chain; nothing newer
    /// from it lands until an operator resets it.
    pub forked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SegmentSummary {
    pub chain_id: String,
    pub from_seq: i64,
    pub to_seq: i64,
}

/// What a `POST /v1/sync/segments` landed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishReceipt {
    pub chain_id: String,
    pub from_seq: i64,
    pub to_seq: i64,
    /// `appended` | `no_op` (already held, identical bytes).
    pub outcome: String,
    pub appended: usize,
    /// The human fingerprint the node trusted on first use, if it did.
    pub trusted_now: Option<String>,
}

/// A redaction the node has seen, and which subscriber chains have moved
/// past it (their reported head of the emitter's chain ≥ the redaction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactionSummary {
    /// The emitter's chain.
    pub chain_id: String,
    /// The redaction event's seq on that chain.
    pub event_seq: i64,
    /// The forgotten prompt's seq on that chain.
    pub target_seq: i64,
    pub acked_by: Vec<String>,
    pub pending: Vec<String>,
}

/// "`acker_chain` has imported `chain_id` up to `acked_seq`" — carried in
/// the acker's segments, recorded by the node, served back so an emitter
/// that does not hold the acker's chain still learns of the ack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckSummary {
    pub acker_chain: String,
    pub chain_id: String,
    pub acked_seq: i64,
}

/// A peer's signed statement of what it holds — posted to the org node
/// after every fetch, so an ack does not wait for the peer's next published
/// segment (an idle peer would otherwise never acknowledge a redaction).
/// The node verifies the signature against the peer's trusted key: an ack
/// is attributable, like everything else that crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckReport {
    pub acker_chain: String,
    /// `(chain, seq held)`.
    pub acks: Vec<(String, i64)>,
    pub at: i64,
    /// Ed25519 over [`AckReport::signed_line`], hex.
    pub signature: String,
}

impl AckReport {
    /// The bytes the signature covers: the report without its signature,
    /// as canonical JSON.
    pub fn signed_line(&self) -> String {
        serde_json::json!({ "ackerChain": self.acker_chain, "acks": self.acks, "at": self.at }).to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum SyncApiError {
    /// This daemon does not relay (`polis serve` without `--org`).
    NotAnOrgNode,
    /// The envelope failed verification, trust, or continuity; the reason.
    Refused(String),
    /// The envelope rewrites history the node holds; recorded and refused.
    Forked(String),
    NotFound,
    Store(String),
}

impl std::fmt::Display for SyncApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncApiError::NotAnOrgNode => write!(f, "not an org node — start the daemon with `polis serve --org`"),
            SyncApiError::Refused(r) => write!(f, "refused: {r}"),
            SyncApiError::Forked(r) => write!(f, "forked: {r}"),
            SyncApiError::NotFound => write!(f, "not found"),
            SyncApiError::Store(r) => write!(f, "store: {r}"),
        }
    }
}

/// What the org node's routes are served from. Object-safe and sync (the
/// handlers wrap calls in `spawn_blocking`, like the memory routes).
pub trait SyncRelay: Send + Sync {
    fn node(&self) -> Result<NodeCard, SyncApiError>;
    fn chains(&self) -> Result<Vec<ChainSummary>, SyncApiError>;
    /// Segments of one chain past `after_seq`, oldest first.
    fn segments(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentSummary>, SyncApiError>;
    /// One segment's envelope, exactly as published (a peer re-verifies it).
    fn segment(&self, chain_id: &str, from_seq: i64, to_seq: i64) -> Result<Option<serde_json::Value>, SyncApiError>;
    /// Verify and store an envelope; relay it from now on.
    fn publish(&self, envelope: serde_json::Value) -> Result<PublishReceipt, SyncApiError>;
    fn redactions(&self) -> Result<Vec<RedactionSummary>, SyncApiError>;
    /// Acks recorded from every subscriber's segments; `chain_id` narrows
    /// to acks OF one chain.
    fn acks(&self, chain_id: Option<&str>) -> Result<Vec<AckSummary>, SyncApiError>;
    /// A subscriber's signed [`AckReport`] (as JSON); verified, then
    /// recorded. Returns how many acks landed.
    fn record_acks(&self, report: serde_json::Value) -> Result<usize, SyncApiError>;
}

/// A daemon that is not an org node.
pub struct NoSyncRelay;

impl SyncRelay for NoSyncRelay {
    fn node(&self) -> Result<NodeCard, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn chains(&self) -> Result<Vec<ChainSummary>, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn segments(&self, _chain_id: &str, _after_seq: i64) -> Result<Vec<SegmentSummary>, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn segment(&self, _chain_id: &str, _from_seq: i64, _to_seq: i64) -> Result<Option<serde_json::Value>, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn publish(&self, _envelope: serde_json::Value) -> Result<PublishReceipt, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn redactions(&self) -> Result<Vec<RedactionSummary>, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn acks(&self, _chain_id: Option<&str>) -> Result<Vec<AckSummary>, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
    fn record_acks(&self, _report: serde_json::Value) -> Result<usize, SyncApiError> {
        Err(SyncApiError::NotAnOrgNode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_null_relay_says_so_on_every_call_and_the_error_is_data() {
        let r: &dyn SyncRelay = &NoSyncRelay;
        assert_eq!(r.node().unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.chains().unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.segments("c", 0).unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.segment("c", 1, 2).unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.publish(serde_json::json!({})).unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.redactions().unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.acks(None).unwrap_err(), SyncApiError::NotAnOrgNode);
        assert_eq!(r.record_acks(serde_json::json!({})).unwrap_err(), SyncApiError::NotAnOrgNode);
        let rep = AckReport { acker_chain: "b".into(), acks: vec![("a".into(), 4)], at: 7, signature: String::new() };
        assert_eq!(rep.signed_line(), r#"{"ackerChain":"b","acks":[["a",4]],"at":7}"#);
        let j = serde_json::to_string(&SyncApiError::Forked("seq 3 rewritten".into())).unwrap();
        assert_eq!(j, r#"{"kind":"forked","detail":"seq 3 rewritten"}"#);
    }
}
