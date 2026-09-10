// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The org node (plan §4.6, Session E4): `polis serve --org`.
//!
//! Just another principal — its own key, its own device chain, its own
//! gardener over what it holds — that stores and relays signed segments
//! between peers, verifies each on receipt exactly as an import does, and
//! is never a trust root: every peer re-verifies on import. It publishes
//! the firm's catalog as signed events on its own chain (`policy.tree`), so
//! "the firm's view" is auditable and attributable rather than a privileged
//! server view nobody can check.
//!
//! Segments live under `$POLIS_HOME/sync/org/<chain>/<from>-<to>.polis.json`
//! — the folder transport's layout, append-only, a segment never rewritten —
//! and every received segment is also imported into the node's own store,
//! which is what its gardener organizes and what `doctor` reports on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use polis_core::identity::fingerprint;
use polis_core::sync::{AckSummary, ChainSummary, NodeCard, PublishReceipt, RedactionSummary, SegmentSummary, SyncApiError, SyncRelay};
use polis_store::PolisStore;

use crate::envelope::{self, BuildOptions, Envelope, Policy};
use crate::identity::Identity;
use crate::sharing::{self, ImportError, ImportOptions, ImportOutcome};
use crate::transport::{FolderTransport, SegmentRef, Subscription};

/// The transport key the node's own chain is published under (the
/// high-water mark lives in `polis_meta` beside every other transport's).
pub const OWN_KEY: &str = "org:self";

pub struct OrgNode {
    pub store: Arc<PolisStore>,
    pub identity: Arc<Identity>,
    /// The name on the node's human card — the org's name.
    pub display_name: String,
    /// Where the relayed segments live.
    pub segments_dir: PathBuf,
    /// Trust a peer's key on first use (the fingerprint is recorded and
    /// logged); otherwise an unknown key is refused until `polis trust add`.
    pub tofu: bool,
}

impl OrgNode {
    pub fn new(store: Arc<PolisStore>, identity: Arc<Identity>, display_name: impl Into<String>, segments_dir: impl Into<PathBuf>, tofu: bool) -> Self {
        OrgNode { store, identity, display_name: display_name.into(), segments_dir: segments_dir.into(), tofu }
    }

    fn folder(&self) -> &Path {
        &self.segments_dir
    }

    /// Publish this node's own chain — its catalog rides (`policy.tree`) —
    /// when the head moved past what the folder holds. Cheap when nothing
    /// changed: one meta read.
    pub fn publish_own(&self) -> Result<Option<SegmentRef>, String> {
        crate::sharing::flush_redactions(&self.store, &self.identity.device_id(), &self.display_name)?;
        let (head, _) = self.store.chain_head().map_err(|e| e.to_string())?;
        let key = format!("polis.sync.published.{OWN_KEY}");
        let done: i64 = self.store.meta(&key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
        if head <= done {
            return Ok(None);
        }
        let policy = Policy { tree: true, ..Policy::default() };
        let opts = BuildOptions { from_seq: Some(done + 1), policy, org_id: Some(self.identity.principal_id()), include_vectors: false };
        let env = envelope::build(&self.store, &self.identity, &self.display_name, &opts)?;
        let r = FolderTransport::publish_to(self.folder(), &env).map_err(|e| e.to_string())?;
        self.store.set_meta(&key, &r.to_seq.to_string()).map_err(|e| e.to_string())?;
        Ok(Some(r))
    }

    fn import_error(e: ImportError) -> SyncApiError {
        match e {
            ImportError::Forked(d) | ImportError::ForkedBefore(d) => SyncApiError::Forked(d),
            ImportError::Store(s) => SyncApiError::Store(s),
            other => SyncApiError::Refused(other.to_string()),
        }
    }
}

impl SyncRelay for OrgNode {
    fn node(&self) -> Result<NodeCard, SyncApiError> {
        Ok(NodeCard {
            principal_id: self.identity.principal_id(),
            chain_id: self.identity.device_id(),
            display_name: Some(self.display_name.clone()),
            fingerprint: self.identity.fingerprint(),
        })
    }

    fn chains(&self) -> Result<Vec<ChainSummary>, SyncApiError> {
        let cards = FolderTransport::chains_in(self.folder(), &Subscription::default()).map_err(|e| SyncApiError::Store(e.to_string()))?;
        let mut out = Vec::with_capacity(cards.len());
        for c in cards {
            let forked = self.store.get_foreign_chain(&c.chain_id).map_err(|e| SyncApiError::Store(e.to_string()))?.is_some_and(|f| f.forked);
            out.push(ChainSummary { chain_id: c.chain_id, human_id: c.human_id, display_name: c.display_name, device_name: c.device_name, head_seq: c.head_seq, forked });
        }
        Ok(out)
    }

    fn segments(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentSummary>, SyncApiError> {
        let refs = FolderTransport::segments_in(self.folder(), chain_id).map_err(|e| SyncApiError::Store(e.to_string()))?;
        Ok(refs.into_iter().filter(|r| r.to_seq > after_seq).map(|r| SegmentSummary { chain_id: r.chain_id, from_seq: r.from_seq, to_seq: r.to_seq }).collect())
    }

    fn segment(&self, chain_id: &str, from_seq: i64, to_seq: i64) -> Result<Option<serde_json::Value>, SyncApiError> {
        let refs = FolderTransport::segments_in(self.folder(), chain_id).map_err(|e| SyncApiError::Store(e.to_string()))?;
        let Some(r) = refs.into_iter().find(|r| r.from_seq == from_seq && r.to_seq == to_seq) else { return Ok(None) };
        let text = std::fs::read_to_string(&r.locator).map_err(|e| SyncApiError::Store(e.to_string()))?;
        serde_json::from_str(&text).map(Some).map_err(|e| SyncApiError::Store(e.to_string()))
    }

    fn publish(&self, envelope: serde_json::Value) -> Result<PublishReceipt, SyncApiError> {
        let env: Envelope = serde_json::from_value(envelope).map_err(|e| SyncApiError::Refused(format!("not a polis.bundle/2 envelope: {e}")))?;
        // Verified, trusted, continuous — through the same import a peer
        // runs; the node's store is the union its gardener works over.
        let opts = ImportOptions { tofu: self.tofu, force: true, local_model: None };
        let report = sharing::import(&self.store, &env, &opts).map_err(Self::import_error)?;
        let (outcome, appended) = match report.outcome {
            ImportOutcome::Appended { appended } => ("appended", appended),
            ImportOutcome::NoOp => ("no_op", 0),
            ImportOutcome::OwnChain => return Err(SyncApiError::Refused("that is this node's own chain — a chain has one writer".into())),
        };
        // Stored verbatim for relay: a peer re-verifies the bytes the
        // publisher signed, never a re-serialization.
        FolderTransport::publish_to(self.folder(), &env).map_err(|e| SyncApiError::Refused(e.to_string()))?;
        // The node holds the chain up to here too — its own ack, so an
        // emitter's `doctor` does not list the relay as a peer that never
        // acknowledged a redaction it has in fact imported.
        self.store
            .record_org_ack(&self.identity.device_id(), &env.header.chain_id, env.header.segment.to_seq)
            .map_err(|e| SyncApiError::Store(e.to_string()))?;
        if let Some(fp) = &report.trusted_now {
            tracing::info!(peer = %fp, "trusted a peer's key on first use");
        }
        Ok(PublishReceipt { chain_id: env.header.chain_id.clone(), from_seq: env.header.segment.from_seq, to_seq: env.header.segment.to_seq, outcome: outcome.into(), appended, trusted_now: report.trusted_now })
    }

    fn redactions(&self) -> Result<Vec<RedactionSummary>, SyncApiError> {
        let rows = self.store.list_foreign_redactions().map_err(|e| SyncApiError::Store(e.to_string()))?;
        let peers = self.store.list_foreign_chains().map_err(|e| SyncApiError::Store(e.to_string()))?;
        let acks = self.store.org_acks(None).map_err(|e| SyncApiError::Store(e.to_string()))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let (mut acked_by, mut pending) = (Vec::new(), Vec::new());
            for p in peers.iter().filter(|p| p.chain_id != r.chain_id) {
                let moved_past = acks.iter().any(|(acker, chain, seq)| acker == &p.chain_id && chain == &r.chain_id && *seq >= r.event_seq);
                if moved_past {
                    acked_by.push(p.chain_id.clone());
                } else {
                    pending.push(p.chain_id.clone());
                }
            }
            out.push(RedactionSummary { chain_id: r.chain_id, event_seq: r.event_seq, target_seq: r.target_seq, acked_by, pending });
        }
        Ok(out)
    }

    fn acks(&self, chain_id: Option<&str>) -> Result<Vec<AckSummary>, SyncApiError> {
        let rows = self.store.org_acks(chain_id).map_err(|e| SyncApiError::Store(e.to_string()))?;
        Ok(rows.into_iter().map(|(acker_chain, chain_id, acked_seq)| AckSummary { acker_chain, chain_id, acked_seq }).collect())
    }

    fn record_acks(&self, report: serde_json::Value) -> Result<usize, SyncApiError> {
        let rep: polis_core::sync::AckReport = serde_json::from_value(report).map_err(|e| SyncApiError::Refused(format!("not an ack report: {e}")))?;
        // Attributable: the acker is a chain the node holds, and the report
        // is signed by that chain's human — the key the node trusted when
        // it accepted the chain's first segment.
        let chain = self
            .store
            .get_foreign_chain(&rep.acker_chain)
            .map_err(|e| SyncApiError::Store(e.to_string()))?
            .ok_or_else(|| SyncApiError::Refused(format!("unknown acker chain {} — publish a segment first", fingerprint(&rep.acker_chain))))?;
        let trust = self
            .store
            .trust_get(&chain.human_id)
            .map_err(|e| SyncApiError::Store(e.to_string()))?
            .ok_or_else(|| SyncApiError::Refused(format!("no trusted key for human {}", fingerprint(&chain.human_id))))?;
        if !Identity::verify_hex(&trust.pubkey, rep.signed_line().as_bytes(), &rep.signature) {
            return Err(SyncApiError::Refused("ack report signature does not verify with the acker's trusted key".into()));
        }
        let mut n = 0;
        for (chain_id, seq) in &rep.acks {
            self.store.record_org_ack(&rep.acker_chain, chain_id, *seq).map_err(|e| SyncApiError::Store(e.to_string()))?;
            n += 1;
        }
        Ok(n)
    }
}

/// What `doctor` prints for an org node: per subscriber, the redactions it
/// has not moved past.
pub fn pending_by_subscriber(node: &OrgNode) -> Result<Vec<(String, String, usize)>, String> {
    let rows = node.redactions().map_err(|e| e.to_string())?;
    let peers = node.store.list_foreign_chains().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for p in peers {
        let n = rows.iter().filter(|r| r.pending.iter().any(|c| c == &p.chain_id)).count();
        let label = p.display_name.clone().unwrap_or_else(|| fingerprint(&p.chain_id));
        out.push((p.chain_id, label, n));
    }
    Ok(out)
}
