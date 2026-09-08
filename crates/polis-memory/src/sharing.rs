// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Sharing (Session E3, plan §4.6): a peer's signed segment becomes foreign
//! rows here — after every check the plan names, in order: the key hashes
//! to its principal, the key is trusted (admin-distributed or first-use),
//! the signature, the bind, every event's hash and link, then continuity
//! against the head of that chain we already hold (append / no-op /
//! overlap-check / gap-reject / mismatch = **forked**). Foreign rows are
//! never re-chained; a redaction the segment carries tombstones the body it
//! names. Nothing here contacts a peer: the transport fetched bytes, this
//! decides what they are worth.

use polis_core::identity::{fingerprint, principal_id, PrincipalKind};
use polis_core::ledger::{EventKind, LedgerAppend, LedgerEventRow};
use polis_store::foreign::{ForeignChain, ForeignNoteInput, ForeignPromptInput, ForeignRedactionInput, ImportedRows, TrustEntry};
use polis_store::PolisStore;
use serde::Serialize;

use crate::envelope::{self, Envelope, Redaction, RedactionPayload, Verified, VerifyError};
use crate::identity::hex_decode;

/// How an import is allowed to proceed.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    /// Trust a key on first use (the fingerprint is reported); otherwise an
    /// unknown key is refused until `polis trust add`.
    pub tofu: bool,
    /// Import even when no subscription matches the chain.
    pub force: bool,
    /// The local embedder's model id — a shipped vector is kept only when
    /// its `model` equals this; the rest are discarded and re-embedded.
    pub local_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ImportOutcome {
    /// New events landed (`appended` of them).
    Appended { appended: usize },
    /// Everything in the segment was already here and matched.
    NoOp,
    /// The segment is one of this store's own device chains: verified for
    /// continuity, nothing stored (a chain has one writer).
    OwnChain,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub outcome: ImportOutcome,
    pub chain_id: String,
    pub human: String,
    pub source: String,
    pub from_seq: i64,
    pub to_seq: i64,
    pub rows: ImportedRows,
    pub vectors_kept: usize,
    pub vectors_discarded: usize,
    /// The fingerprint trusted on first use by this import, if any.
    pub trusted_now: Option<String>,
    pub verified: Verified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum ImportError {
    Verify(VerifyError),
    /// The human's key is unknown here and `tofu` was not allowed.
    NotTrusted { principal: String, fingerprint: String },
    /// A different key than the one trusted for this human, and the
    /// segment carries no bind event that would make it a rotation.
    KeyChanged { principal: String, trusted: String, offered: String },
    /// The segment rewrites history we hold: an event at a seq we have
    /// carries a different hash. The chain is marked forked; nothing newer
    /// from it lands until an operator clears the mark.
    Forked(String),
    /// The chain is marked forked from an earlier import.
    ForkedBefore(String),
    /// The segment does not start where our copy of the chain ends.
    Gap(String),
    /// No subscription matches this chain (and `force` was not given).
    NotSubscribed { principal: String },
    Store(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Verify(e) => write!(f, "{e}"),
            ImportError::NotTrusted { principal, fingerprint } => write!(f, "not trusted: human {} ({fingerprint}) — `polis trust add` it, or import with --tofu", fingerprint_of(principal)),
            ImportError::KeyChanged { principal, trusted, offered } => write!(f, "key changed: human {} is trusted with key {trusted}, the envelope offers {offered} and carries no bind for the change — refused", fingerprint_of(principal)),
            ImportError::Forked(s) => write!(f, "forked: {s}"),
            ImportError::ForkedBefore(s) => write!(f, "forked (earlier import): {s} — `polis trust` / `polis subscribe rm --purge` to reset"),
            ImportError::Gap(s) => write!(f, "gap: {s}"),
            ImportError::NotSubscribed { principal } => write!(f, "not subscribed: human {} — `polis subscribe add --principal …` or import with --force", fingerprint_of(principal)),
            ImportError::Store(s) => write!(f, "store: {s}"),
        }
    }
}

fn fingerprint_of(principal: &str) -> String {
    fingerprint(principal)
}

impl From<VerifyError> for ImportError {
    fn from(e: VerifyError) -> Self {
        ImportError::Verify(e)
    }
}

fn store_err(e: rusqlite::Error) -> ImportError {
    ImportError::Store(e.to_string())
}

/// Does a subscription row match this chain? A row names a human id, a
/// fingerprint prefix or a display name; a row with no principal matches
/// every chain (it exists for its project filter).
fn subscription_matches(row: &polis_store::foreign::SubscriptionRow, human: &str, display_name: Option<&str>) -> bool {
    match row.principal.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => true,
        Some(p) => {
            let p_l = p.to_ascii_lowercase();
            human == p || human.starts_with(&p_l) || fingerprint(human) == p_l || display_name.is_some_and(|d| d.eq_ignore_ascii_case(p))
        }
    }
}

/// Import one envelope. Verification first (all of it, from the bytes
/// alone), then trust, then the local decisions.
pub fn import(store: &PolisStore, env: &Envelope, opts: &ImportOptions) -> Result<ImportReport, ImportError> {
    let v = envelope::verify(env)?;
    let h = &env.header;
    let human = &h.principal.human;
    let pubkey_hex = human.pubkey.clone().unwrap_or_default();
    let display_name = human.display_name.clone();

    // A chain of ours: continuity only, never stored as foreign.
    if store.known_principal_kind(&h.chain_id).map_err(store_err)? == Some(PrincipalKind::Device) {
        envelope::verify_against(store, env)?;
        return Ok(ImportReport {
            outcome: ImportOutcome::OwnChain,
            chain_id: h.chain_id.clone(),
            human: human.principal_id.clone(),
            source: "own".into(),
            from_seq: h.segment.from_seq,
            to_seq: h.segment.to_seq,
            rows: ImportedRows::default(),
            vectors_kept: 0,
            vectors_discarded: 0,
            trusted_now: None,
            verified: v,
        });
    }

    // Trust: the human's key, admin-distributed or on first use.
    let mut trusted_now = None;
    match store.trust_get(&human.principal_id).map_err(store_err)? {
        Some(t) if t.pubkey == pubkey_hex => {}
        Some(t) => {
            // The trusted row names a different key for this id. By
            // construction (`principal_id = sha256(pubkey)`) a genuine key
            // change is a NEW principal, never this branch — so this is a
            // corrupted or hand-edited trust row, refused rather than
            // silently repaired. `polis trust rm` + `add` is the operator's
            // explicit path. (A rotation with a bind event is E4's — the
            // org node is where a firm re-keys a person.)
            return Err(ImportError::KeyChanged {
                principal: human.principal_id.clone(),
                trusted: fingerprint(&principal_id(&hex_decode(&t.pubkey).unwrap_or_default())),
                offered: fingerprint(&human.principal_id),
            });
        }
        None => {
            if !opts.tofu {
                return Err(ImportError::NotTrusted { principal: human.principal_id.clone(), fingerprint: fingerprint(&human.principal_id) });
            }
            store
                .trust_set(&TrustEntry { principal_id: human.principal_id.clone(), pubkey: pubkey_hex.clone(), fingerprint: fingerprint(&human.principal_id), source: "tofu".into(), display_name: display_name.clone(), added_at: polis_core::ledger::now_millis() })
                .map_err(store_err)?;
            trusted_now = Some(fingerprint(&human.principal_id));
        }
    }

    // Subscriptions: selective by design.
    let subs = store.subscriptions().map_err(store_err)?;
    let matching: Vec<&polis_store::foreign::SubscriptionRow> = subs.iter().filter(|r| subscription_matches(r, &human.principal_id, display_name.as_deref())).collect();
    if !subs.is_empty() && matching.is_empty() && !opts.force {
        return Err(ImportError::NotSubscribed { principal: human.principal_id.clone() });
    }
    let projects: Vec<String> = matching.iter().filter_map(|r| r.project.clone()).filter(|p| !p.trim().is_empty()).collect();

    // Continuity against what we hold of this chain.
    let mut events: Vec<LedgerEventRow> = env.payload.events.clone();
    events.sort_by_key(|e| e.seq);
    let existing = store.get_foreign_chain(&h.chain_id).map_err(store_err)?;
    let (first_import_at, start_seq) = match &existing {
        None => {
            if h.segment.from_seq != 1 {
                return Err(ImportError::Gap(format!("first segment of chain {} must start at seq 1, this one starts at {}", fingerprint(&h.chain_id), h.segment.from_seq)));
            }
            (polis_core::ledger::now_millis(), 1)
        }
        Some(c) => {
            if c.forked {
                return Err(ImportError::ForkedBefore(c.fork_detail.clone().unwrap_or_default()));
            }
            if h.segment.from_seq > c.head_seq + 1 {
                return Err(ImportError::Gap(format!("we hold chain {} up to seq {}, the segment starts at {} — fetch the segments between first", fingerprint(&h.chain_id), c.head_seq, h.segment.from_seq)));
            }
            // Overlap: every event at a seq we hold must carry the hash we hold.
            for e in events.iter().filter(|e| e.seq <= c.head_seq) {
                match store.foreign_event_hash(&h.chain_id, e.seq).map_err(store_err)? {
                    Some(ours) if ours == e.entry_hash => {}
                    Some(ours) => {
                        let detail = format!("seq {} here has hash {}, the segment carries {} — the peer rewrote history", e.seq, ours, e.entry_hash);
                        store.mark_foreign_forked(&h.chain_id, &detail).map_err(store_err)?;
                        return Err(ImportError::Forked(detail));
                    }
                    None => {}
                }
            }
            if h.segment.from_seq == c.head_seq + 1 && h.segment.prev_hash_at_from != c.head_hash {
                let detail = format!("the segment links onto {} but our head at seq {} is {}", h.segment.prev_hash_at_from, c.head_seq, c.head_hash);
                store.mark_foreign_forked(&h.chain_id, &detail).map_err(store_err)?;
                return Err(ImportError::Forked(detail));
            }
            (c.first_import_at, c.head_seq + 1)
        }
    };
    let new_events: Vec<LedgerEventRow> = events.iter().filter(|e| e.seq >= start_seq).cloned().collect();
    let source = display_name.clone().filter(|n| !n.trim().is_empty()).map(|n| format!("shared:{n}")).unwrap_or_else(|| format!("shared:{}", fingerprint(&h.chain_id)));
    if new_events.is_empty() {
        return Ok(ImportReport {
            outcome: ImportOutcome::NoOp,
            chain_id: h.chain_id.clone(),
            human: human.principal_id.clone(),
            source,
            from_seq: h.segment.from_seq,
            to_seq: h.segment.to_seq,
            rows: ImportedRows::default(),
            vectors_kept: 0,
            vectors_discarded: 0,
            trusted_now,
            verified: v,
        });
    }

    // The rows: prompts at their redaction (a project filter stubs the rest),
    // notes, principal cards, and the redactions this segment carries.
    let mut prompts = Vec::new();
    let mut prompt_seq_by_id: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for e in new_events.iter().filter(|e| e.kind == "prompt") {
        if let Some(pid) = e.prompt_id {
            prompt_seq_by_id.insert(pid, e.seq);
        }
    }
    let redaction_str = |r: Redaction| match r {
        Redaction::Full => "full",
        Redaction::Gist => "gist",
        Redaction::Stub => "stub",
    };
    for p in &env.payload.prompts {
        let Some(&seq) = prompt_seq_by_id.get(&p.id) else { continue };
        let keep_body = projects.is_empty() || p.project.as_deref().is_some_and(|pp| projects.iter().any(|f| pp == f || pp.starts_with(f)));
        let (redaction, text) = if keep_body { (redaction_str(p.redaction), p.text.as_deref()) } else { ("stub", None) };
        prompts.push((seq, p, redaction, text));
    }
    let prompt_inputs: Vec<ForeignPromptInput<'_>> = prompts
        .iter()
        .map(|(seq, p, redaction, text)| ForeignPromptInput { seq: *seq, prompt_id: p.id, role: &p.role, body_hash: &p.body_hash, redaction, text: *text, project: p.project.as_deref() })
        .collect();
    let new_seqs: std::collections::HashSet<i64> = new_events.iter().map(|e| e.seq).collect();
    let note_inputs: Vec<ForeignNoteInput<'_>> = env
        .payload
        .notes
        .iter()
        .filter(|n| n.seq.is_none_or(|s| new_seqs.contains(&s)))
        .map(|n| ForeignNoteInput { note_id: n.id, seq: n.seq, target_kind: &n.target_kind, target_id: n.target_id.as_deref(), text: &n.text, created_at: n.created_at })
        .collect();
    let redaction_inputs: Vec<ForeignRedactionInput<'_>> = env
        .payload
        .redactions
        .iter()
        .filter(|r| new_seqs.contains(&r.event_seq))
        .map(|r| ForeignRedactionInput { event_seq: r.event_seq, target_chain: &r.chain_id, target_seq: r.seq })
        .collect();
    let last = new_events.last().expect("non-empty");
    let chain = ForeignChain {
        chain_id: h.chain_id.clone(),
        human_id: human.principal_id.clone(),
        device_name: h.principal.device.display_name.clone(),
        display_name: display_name.clone(),
        head_seq: last.seq,
        head_hash: last.entry_hash.clone(),
        forked: false,
        fork_detail: None,
        first_import_at,
        last_import_at: polis_core::ledger::now_millis(),
    };
    let rows = store
        .import_foreign_segment(&chain, &new_events, &prompt_inputs, &note_inputs, &env.payload.principals, &redaction_inputs)
        .map_err(store_err)?;

    // Vectors: reused only under the same model id; the rest re-embed locally.
    let (mut kept, mut discarded) = (0usize, 0usize);
    for vct in &env.payload.vectors {
        let same_model = opts.local_model.as_deref() == Some(vct.model.as_str());
        let Some(&seq) = prompt_seq_by_id.get(&vct.prompt_id) else { continue };
        let Some(row) = store.foreign_prompt(&h.chain_id, seq).map_err(store_err)? else { continue };
        if !same_model || row.tombstoned || row.text.is_none() {
            discarded += 1;
            continue;
        }
        let Some(bytes) = hex_decode(&vct.vec_hex) else {
            discarded += 1;
            continue;
        };
        store
            .store_foreign_embedding_raw(row.id, &vct.model, &row.body_hash, vct.dim, vct.chunk_ix, vct.char_start, vct.char_len, vct.scale, &bytes)
            .map_err(store_err)?;
        kept += 1;
    }

    // Acks: the peer names the newest seq of OUR chains it has imported.
    for (chain_id, seq) in &env.payload.acks {
        if store.known_principal_kind(chain_id).map_err(store_err)? == Some(PrincipalKind::Device) {
            store.set_foreign_ack(&h.chain_id, *seq).map_err(store_err)?;
        }
    }

    Ok(ImportReport {
        outcome: ImportOutcome::Appended { appended: new_events.len() },
        chain_id: h.chain_id.clone(),
        human: human.principal_id.clone(),
        source,
        from_seq: h.segment.from_seq,
        to_seq: h.segment.to_seq,
        rows,
        vectors_kept: kept,
        vectors_discarded: discarded,
        trusted_now,
        verified: v,
    })
}

/// `forget` appends this: a `redaction` event naming `(chain, seq)` of the
/// forgotten prompt, its payload kept beside the chain so an export can
/// carry it. Returns the redaction event's seq, or `None` when the prompt
/// has no event on the chain (nothing a peer could hold).
pub fn append_redaction(store: &PolisStore, chain_id: &str, prompt_id: i64, actor: &str) -> Result<Option<i64>, String> {
    let Some(target_seq) = store.prompt_event_seq(prompt_id).map_err(|e| e.to_string())? else { return Ok(None) };
    let payload = RedactionPayload { chain_id: chain_id.to_string(), seq: target_seq, event_seq: 0 };
    let hash = payload.payload_hash();
    let ref_id = prompt_id.to_string();
    let row = store
        .append_event(&LedgerAppend {
            kind: EventKind::Redaction.as_str(),
            author: actor,
            ts: polis_core::ledger::now_millis(),
            prompt_id: Some(prompt_id),
            session_id: None,
            version_number: None,
            ref_kind: Some("prompt"),
            ref_id: Some(&ref_id),
            payload_hash: &hash,
        })
        .map_err(|e| e.to_string())?;
    let kept = RedactionPayload { event_seq: row.seq, ..payload };
    store
        .set_meta(&format!("polis.redaction.{}", row.seq), &serde_json::to_string(&kept).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    Ok(Some(row.seq))
}

/// Redactions we emitted that a peer has not yet acknowledged (its reported
/// head of our chain is older than the redaction). `doctor` lists these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnackedRedaction {
    pub redaction_seq: i64,
    pub target_seq: i64,
    pub peer_chain: String,
    pub peer_source: String,
    pub peer_acked_seq: Option<i64>,
}

pub fn unacknowledged_redactions(store: &PolisStore) -> Result<Vec<UnackedRedaction>, String> {
    let ours = store.own_redactions().map_err(|e| e.to_string())?;
    if ours.is_empty() {
        return Ok(Vec::new());
    }
    let peers = store.list_foreign_chains().map_err(|e| e.to_string())?;
    let acks: std::collections::HashMap<String, i64> = store.foreign_acks().map_err(|e| e.to_string())?.into_iter().collect();
    let mut out = Vec::new();
    for (seq, _chain, target) in &ours {
        for p in &peers {
            let acked = acks.get(&p.chain_id).copied();
            if acked.is_none_or(|a| a < *seq) {
                out.push(UnackedRedaction {
                    redaction_seq: *seq,
                    target_seq: *target,
                    peer_chain: p.chain_id.clone(),
                    peer_source: p.display_name.clone().unwrap_or_else(|| fingerprint(&p.chain_id)),
                    peer_acked_seq: acked,
                });
            }
        }
    }
    Ok(out)
}
