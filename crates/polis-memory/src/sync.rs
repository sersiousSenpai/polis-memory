// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis sync` (Session E3): publish this device's new segments, then
//! fetch and import every subscribed peer's. Replicate, never federate:
//! after a sync every question is answered from local rows.

use polis_store::PolisStore;
use serde::Serialize;

use crate::envelope::{self, BuildOptions, Policy};
use crate::identity::Identity;
use crate::sharing::{self, ImportOptions, ImportReport};
use crate::transport::{SegmentRef, SegmentTransport, Subscription};

#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub publish: bool,
    pub fetch: bool,
    pub tofu: bool,
    pub force: bool,
    pub policy: Policy,
    pub include_vectors: bool,
    pub local_model: Option<String>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions { publish: true, fetch: true, tofu: false, force: false, policy: Policy::default(), include_vectors: false, local_model: None }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub published: Option<SegmentRef>,
    pub imported: Vec<ImportReport>,
    /// `(chain, reason)` — chains listed but not imported.
    pub skipped: Vec<(String, String)>,
    pub errors: Vec<(String, String)>,
    /// E4: acks of our chain the transport relayed (an org node's).
    #[serde(default)]
    pub acks_relayed: usize,
}

fn published_key(transport_key: &str) -> String {
    format!("polis.sync.published.{transport_key}")
}

/// The seq up to which this transport has our chain.
pub fn published_up_to(store: &PolisStore, transport: &dyn SegmentTransport) -> i64 {
    store.meta(&published_key(&transport.key())).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// The subscription filter from the store's rows.
pub fn subscription_filter(store: &PolisStore) -> Subscription {
    let rows = store.subscriptions().unwrap_or_default();
    Subscription {
        principals: rows.iter().filter_map(|r| r.principal.clone()).filter(|p| !p.trim().is_empty()).collect(),
        projects: rows.iter().filter_map(|r| r.project.clone()).collect(),
        classes: rows.iter().filter_map(|r| r.class.clone()).collect(),
    }
}

pub async fn sync(store: &PolisStore, identity: &Identity, login: &str, transport: &dyn SegmentTransport, opts: &SyncOptions) -> SyncReport {
    let mut report = SyncReport::default();
    if opts.publish {
        let (head, _) = store.chain_head().unwrap_or((0, String::new()));
        let done = published_up_to(store, transport);
        if head > done {
            // E4: an org node stamps the org it relays for; a folder or a
            // git remote has none.
            let org_id = transport.org_id().await;
            let build = BuildOptions { from_seq: Some(done + 1), policy: opts.policy.clone(), org_id, include_vectors: opts.include_vectors };
            match envelope::build(store, identity, login, &build) {
                Ok(env) => match transport.publish(&env).await {
                    Ok(r) => {
                        let _ = store.set_meta(&published_key(&transport.key()), &r.to_seq.to_string());
                        report.published = Some(r);
                    }
                    Err(e) => report.errors.push(("publish".into(), e.to_string())),
                },
                Err(e) => report.errors.push(("publish".into(), e)),
            }
        }
    }
    if opts.fetch {
        let filter = subscription_filter(store);
        let own = identity.device_id();
        match transport.chains(&filter).await {
            Ok(cards) => {
                for card in cards {
                    if card.chain_id == own {
                        continue;
                    }
                    let held = store.get_foreign_chain(&card.chain_id).ok().flatten().map(|c| c.head_seq).unwrap_or(0);
                    if card.head_seq <= held {
                        report.skipped.push((card.chain_id.clone(), "up to date".into()));
                        continue;
                    }
                    let refs = match transport.list(&card.chain_id, held).await {
                        Ok(r) => r,
                        Err(e) => {
                            report.errors.push((card.chain_id.clone(), e.to_string()));
                            continue;
                        }
                    };
                    let iopts = ImportOptions { tofu: opts.tofu, force: opts.force, local_model: opts.local_model.clone() };
                    for r in refs {
                        match transport.fetch(&r).await {
                            Ok(env) => match sharing::import(store, &env, &iopts) {
                                Ok(rep) => report.imported.push(rep),
                                Err(e) => {
                                    report.errors.push((card.chain_id.clone(), e.to_string()));
                                    break;
                                }
                            },
                            Err(e) => {
                                report.errors.push((card.chain_id.clone(), e.to_string()));
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => report.errors.push(("chains".into(), e.to_string())),
        }
        // E4: tell an org node what we hold now — signed, so the node can
        // attribute it — rather than waiting for our next published segment.
        let held: Vec<(String, i64)> = store.list_foreign_chains().map(|cs| cs.into_iter().map(|c| (c.chain_id, c.head_seq)).collect()).unwrap_or_default();
        if !held.is_empty() {
            let mut rep = polis_core::sync::AckReport { acker_chain: own.clone(), acks: held, at: polis_core::ledger::now_millis(), signature: String::new() };
            rep.signature = identity.sign_hex(rep.signed_line().as_bytes());
            if let Err(e) = transport.report_acks(&rep).await {
                report.errors.push(("acks".into(), e.to_string()));
            }
        }
        // E4: acks the transport relays for our chain (an org node's), so a
        // subscriber we do not hold still counts as having moved past a
        // redaction. Recorded like an ack carried in a peer's own segment.
        match transport.acks_for(&own).await {
            Ok(acks) => {
                for (acker, seq) in acks {
                    if acker != own {
                        let _ = store.set_foreign_ack(&acker, seq);
                        report.acks_relayed += 1;
                    }
                }
            }
            Err(e) => report.errors.push(("acks".into(), e.to_string())),
        }
    }
    report
}
