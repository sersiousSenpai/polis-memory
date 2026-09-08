// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The signed segment envelope `polis.bundle/2` (Session E2; plan §4.6) —
//! export and VERIFY-ONLY import. Storing a foreign chain (the `foreign_*`
//! tables, subscriptions, transports) is Session E3; here an envelope is
//! built, signed, and checked from its bytes alone.
//!
//! Shape: a header line (canonical JSON of [`Header`]: the chain id, the
//! device + human cards, the org, the contiguous segment, the policy, the
//! payload's sha256), an Ed25519 signature over that line by the human's
//! key, and the payload (events at their redaction, prompts, notes,
//! principals, aliases, the bind payloads). A verifier re-derives every id
//! from the public key, recomputes every event hash and the linkage,
//! recomputes the payload hash, and checks the bind's signature — with no
//! store and no key of its own.

use polis_core::bundle::{canonical_of, BundleNote};
use polis_core::identity::{bind_message, device_id, principal_id, BindPayload, PrincipalCard, PrincipalKind};
use polis_core::ledger::{compute_entry_hash, sha256_hex, EventKind, LedgerEventRow};
use polis_store::PolisStore;
use serde::{Deserialize, Serialize};

use crate::identity::{bind_payload_for, hex_decode, Identity};

pub const ENVELOPE_SCHEMA: &str = "polis.bundle/2";

/// A contiguous run of one chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub from_seq: i64,
    pub to_seq: i64,
    /// The `prev_hash` of the first event — what the segment links onto.
    pub prev_hash_at_from: String,
    /// The `entry_hash` of the last event.
    pub head_hash: String,
}

/// How much of a prompt body ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Bodies {
    /// The plan's default: `full` for org-visible user prompts, `stub`
    /// otherwise.
    #[default]
    Auto,
    Full,
    Gist,
    Stub,
}

impl Bodies {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Bodies::Auto),
            "full" => Some(Bodies::Full),
            "gist" => Some(Bodies::Gist),
            "stub" => Some(Bodies::Stub),
            _ => None,
        }
    }
}

/// What the export was allowed to carry. Recorded in the header so a reader
/// knows what is absent by policy rather than by tampering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    /// Corpus roles whose bodies may ship at all (others are stubs).
    pub roles: Vec<String>,
    pub bodies: Bodies,
    /// The catalog is never exported by default (the org node publishes its own).
    pub tree: bool,
    /// Browse events are never shared by default.
    pub browse: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy { roles: vec!["user".to_string()], bodies: Bodies::Auto, tree: false, browse: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalRef {
    pub device: PrincipalCard,
    pub human: PrincipalCard,
}

/// The header line — the bytes the signature covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub schema: String,
    pub chain_id: String,
    pub principal: PrincipalRef,
    pub org_id: Option<String>,
    pub segment: Segment,
    pub policy: Policy,
    pub payload_sha256: String,
    pub exported_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Redaction {
    Full,
    Gist,
    Stub,
}

/// A prompt body at its redaction. `body_hash` is the identity the chain
/// commits to; a `full` text must hash to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadPrompt {
    pub id: i64,
    pub role: String,
    pub body_hash: String,
    pub redaction: Redaction,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub events: Vec<LedgerEventRow>,
    pub prompts: Vec<PayloadPrompt>,
    pub notes: Vec<BundleNote>,
    pub principals: Vec<PrincipalCard>,
    pub aliases: Vec<(String, String)>,
    pub binds: Vec<BindPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Envelope {
    pub header: Header,
    /// Ed25519 over [`Envelope::header_line`], hex.
    pub signature: String,
    pub payload: Payload,
}

impl Envelope {
    /// Canonical JSON of the header — field order is declaration order.
    pub fn header_line(&self) -> String {
        serde_json::to_string(&self.header).unwrap_or_default()
    }

    pub fn payload_json(&self) -> String {
        serde_json::to_string(&self.payload).unwrap_or_default()
    }
}

/// What to export.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// First seq to include (default 1 — the whole chain).
    pub from_seq: Option<i64>,
    pub policy: Policy,
    pub org_id: Option<String>,
}

/// One row of a `--dry-run`: what the policy decided for each prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub seq: i64,
    pub prompt_id: i64,
    pub role: String,
    pub visibility: String,
    pub redaction: Redaction,
    pub bytes: usize,
}

fn decide(policy: &Policy, role: &str, visibility: &str, has_body: bool, has_gist: bool) -> Redaction {
    if !policy.roles.iter().any(|r| r == role) {
        return Redaction::Stub;
    }
    let gist_or_stub = || if has_gist { Redaction::Gist } else { Redaction::Stub };
    match policy.bodies {
        Bodies::Full => {
            if has_body {
                Redaction::Full
            } else {
                gist_or_stub()
            }
        }
        Bodies::Gist => gist_or_stub(),
        Bodies::Stub => Redaction::Stub,
        Bodies::Auto => {
            if role == "user" && visibility == "org" && has_body {
                Redaction::Full
            } else {
                Redaction::Stub
            }
        }
    }
}

struct PromptRow {
    id: i64,
    role: String,
    visibility: String,
    body: String,
    gist: Option<String>,
    body_hash: String,
}

fn prompt_rows(store: &PolisStore, ids: &[i64]) -> Result<Vec<PromptRow>, String> {
    let conn = store.conn();
    let mut out = Vec::with_capacity(ids.len());
    let mut stmt = conn
        .prepare(
            "SELECT id, COALESCE(role, 'user'), COALESCE(visibility, 'private'), body, gist, body_hash
             FROM prompts WHERE id = ?1",
        )
        .map_err(|e| e.to_string())?;
    for id in ids {
        let row = stmt
            .query_row(rusqlite::params![id], |r| {
                Ok(PromptRow { id: r.get(0)?, role: r.get(1)?, visibility: r.get(2)?, body: r.get(3)?, gist: r.get(4)?, body_hash: r.get(5)? })
            })
            .map_err(|e| e.to_string())?;
        out.push(row);
    }
    Ok(out)
}

fn segment_events(store: &PolisStore, from_seq: i64) -> Result<Vec<LedgerEventRow>, String> {
    let events = store.list_ledger_events_asc(from_seq.max(1) - 1, i64::MAX).map_err(|e| e.to_string())?;
    if events.is_empty() {
        return Err(format!("nothing to export from seq {from_seq}"));
    }
    Ok(events)
}

/// The policy's decision per prompt in the segment — `polis export --dry-run`.
pub fn decisions(store: &PolisStore, opts: &BuildOptions) -> Result<Vec<Decision>, String> {
    let events = segment_events(store, opts.from_seq.unwrap_or(1))?;
    let ids: Vec<i64> = events.iter().filter(|e| e.kind == "prompt").filter_map(|e| e.prompt_id).collect();
    let rows = prompt_rows(store, &ids)?;
    let mut out = Vec::new();
    for e in events.iter().filter(|e| e.kind == "prompt") {
        let Some(pid) = e.prompt_id else { continue };
        let Some(row) = rows.iter().find(|r| r.id == pid) else { continue };
        let redaction = decide(&opts.policy, &row.role, &row.visibility, !row.body.is_empty(), row.gist.as_deref().is_some_and(|g| !g.is_empty()));
        let bytes = match redaction {
            Redaction::Full => row.body.len(),
            Redaction::Gist => row.gist.as_deref().map(str::len).unwrap_or(0),
            Redaction::Stub => 0,
        };
        out.push(Decision { seq: e.seq, prompt_id: pid, role: row.role.clone(), visibility: row.visibility.clone(), redaction, bytes });
    }
    Ok(out)
}

/// Build and sign an envelope for this device's chain from `from_seq` to
/// the head.
pub fn build(store: &PolisStore, identity: &Identity, login: &str, opts: &BuildOptions) -> Result<Envelope, String> {
    let from_seq = opts.from_seq.unwrap_or(1).max(1);
    let events = segment_events(store, from_seq)?;
    let first = events.first().expect("non-empty");
    let last = events.last().expect("non-empty");
    let segment = Segment { from_seq: first.seq, to_seq: last.seq, prev_hash_at_from: first.prev_hash.clone(), head_hash: last.entry_hash.clone() };

    let ids: Vec<i64> = {
        let mut v: Vec<i64> = events.iter().filter(|e| e.kind == "prompt").filter_map(|e| e.prompt_id).collect();
        v.sort();
        v.dedup();
        v
    };
    let mut prompts = Vec::with_capacity(ids.len());
    for row in prompt_rows(store, &ids)? {
        let redaction = decide(&opts.policy, &row.role, &row.visibility, !row.body.is_empty(), row.gist.as_deref().is_some_and(|g| !g.is_empty()));
        let text = match redaction {
            Redaction::Full => Some(row.body.clone()),
            Redaction::Gist => row.gist.clone(),
            Redaction::Stub => None,
        };
        prompts.push(PayloadPrompt { id: row.id, role: row.role, body_hash: row.body_hash, redaction, text });
    }

    let seqs: std::collections::HashSet<i64> = events.iter().map(|e| e.seq).collect();
    let notes: Vec<BundleNote> = if opts.policy.roles.iter().any(|r| r == "user") {
        let mut v: Vec<BundleNote> = store
            .list_user_notes(false, i64::MAX)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|n| n.seq.map(|s| seqs.contains(&s)).unwrap_or(false))
            .map(|n| BundleNote { id: n.id, seq: n.seq, target_kind: n.target_kind, target_id: n.target_id, text: n.text, starred: n.starred, created_at: n.created_at, updated_at: n.updated_at })
            .collect();
        v.sort_by_key(|n| n.id);
        v
    } else {
        Vec::new()
    };

    let principals: Vec<PrincipalCard> = store.list_principals().map_err(|e| e.to_string())?.iter().map(PrincipalCard::from).collect();
    let aliases = store.list_aliases().map_err(|e| e.to_string())?;
    let mut binds = Vec::new();
    for p in principals.iter().filter(|p| p.kind == PrincipalKind::Device) {
        if let Some(b) = bind_payload_for(store, &p.principal_id)? {
            binds.push(b);
        }
    }
    if !binds.iter().any(|b| b.chain_id == identity.device_id()) {
        return Err("this device is not bound to the chain — run `polis init`".into());
    }

    let payload = Payload { events, prompts, notes, principals, aliases, binds };
    let payload_json = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    let header = Header {
        schema: ENVELOPE_SCHEMA.to_string(),
        chain_id: identity.device_id(),
        principal: PrincipalRef { device: identity.device_card(), human: identity.human_card(login) },
        org_id: opts.org_id.clone(),
        segment,
        policy: opts.policy.clone(),
        payload_sha256: sha256_hex(payload_json.as_bytes()),
        exported_at: polis_core::ledger::now_millis(),
    };
    let header_line = serde_json::to_string(&header).map_err(|e| e.to_string())?;
    let signature = identity.sign_hex(header_line.as_bytes());
    Ok(Envelope { header, signature, payload })
}

/// Why an envelope is refused — each a named reason, never a bare `false`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason", content = "detail")]
pub enum VerifyError {
    Schema(String),
    /// `sha256(pubkey) != human.principal_id`.
    IdMismatch,
    /// The device card does not derive from the key and its name, or is
    /// not the chain id.
    DeviceMismatch,
    /// The header signature does not verify with the human's key.
    BadSignature,
    /// No bind for this chain in the payload.
    NoBind,
    /// A bind is present but its signature does not verify.
    BadBindSignature,
    /// The payload's bytes do not hash to the header's `payload_sha256`.
    PayloadHash,
    /// An event's stored `entry_hash` disagrees with a recomputation.
    EventHash(i64),
    /// An event's `prev_hash` does not link to its predecessor.
    Linkage(i64),
    Segment(String),
    /// A `principal_bind` event in the segment commits to a payload the
    /// envelope does not carry.
    BindHashMismatch(i64),
    /// A `full` body does not hash to its `body_hash`, or a prompt event's
    /// hash disagrees with its row.
    BodyHash(i64),
    /// The segment is our own chain and disagrees with the local rows.
    Continuity(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::Schema(s) => write!(f, "schema: {s}"),
            VerifyError::IdMismatch => write!(f, "id mismatch: sha256(pubkey) is not the human's principal id"),
            VerifyError::DeviceMismatch => write!(f, "device mismatch: the device id does not derive from the key and its name, or is not the chain id"),
            VerifyError::BadSignature => write!(f, "bad signature: the header does not verify with the human's key"),
            VerifyError::NoBind => write!(f, "no bind: the payload carries no principal_bind for this chain"),
            VerifyError::BadBindSignature => write!(f, "bad bind signature"),
            VerifyError::PayloadHash => write!(f, "payload hash: the payload bytes do not match the signed header"),
            VerifyError::EventHash(s) => write!(f, "event hash: seq {s} does not recompute"),
            VerifyError::Linkage(s) => write!(f, "linkage: seq {s} does not link to its predecessor"),
            VerifyError::Segment(s) => write!(f, "segment: {s}"),
            VerifyError::BindHashMismatch(s) => write!(f, "bind hash: the principal_bind at seq {s} commits to a payload the envelope does not carry"),
            VerifyError::BodyHash(id) => write!(f, "body hash: prompt {id} does not hash to its body_hash"),
            VerifyError::Continuity(s) => write!(f, "continuity: {s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Verified {
    pub chain_id: String,
    pub human: String,
    pub device_name: String,
    pub from_seq: i64,
    pub to_seq: i64,
    pub head_hash: String,
    pub events: usize,
    pub prompts: usize,
    pub full_bodies: usize,
    pub binds: usize,
}

/// Verify an envelope from its bytes alone.
pub fn verify(env: &Envelope) -> Result<Verified, VerifyError> {
    let h = &env.header;
    if h.schema != ENVELOPE_SCHEMA {
        return Err(VerifyError::Schema(h.schema.clone()));
    }
    // 1. the human is their key
    let pubkey_hex = h.principal.human.pubkey.as_deref().ok_or(VerifyError::IdMismatch)?;
    let pubkey = hex_decode(pubkey_hex).ok_or(VerifyError::IdMismatch)?;
    if principal_id(&pubkey) != h.principal.human.principal_id || h.principal.human.kind != PrincipalKind::Human {
        return Err(VerifyError::IdMismatch);
    }
    // 2. the device derives from the key and its name, and is the chain
    let d = &h.principal.device;
    let name = d.display_name.as_deref().ok_or(VerifyError::DeviceMismatch)?;
    if d.kind != PrincipalKind::Device
        || d.parent_id.as_deref() != Some(h.principal.human.principal_id.as_str())
        || device_id(&pubkey, name) != d.principal_id
        || d.principal_id != h.chain_id
    {
        return Err(VerifyError::DeviceMismatch);
    }
    // 3. the signature over the header line
    if !Identity::verify_hex(pubkey_hex, env.header_line().as_bytes(), &env.signature) {
        return Err(VerifyError::BadSignature);
    }
    // 4. the payload is the one signed
    if sha256_hex(env.payload_json().as_bytes()) != h.payload_sha256 {
        return Err(VerifyError::PayloadHash);
    }
    // 5. a bind for this chain, signed by the same key
    let bind = env.payload.binds.iter().find(|b| b.chain_id == h.chain_id).ok_or(VerifyError::NoBind)?;
    if bind.device.principal_id != h.chain_id || bind.human.principal_id != h.principal.human.principal_id {
        return Err(VerifyError::NoBind);
    }
    if !Identity::verify_hex(pubkey_hex, &bind_message(&bind.chain_id, &bind.head_hash), &bind.signature) {
        return Err(VerifyError::BadBindSignature);
    }
    // 6. every event recomputes and links; the segment is what it says
    let mut events = env.payload.events.clone();
    events.sort_by_key(|e| e.seq);
    let first = events.first().ok_or_else(|| VerifyError::Segment("no events".into()))?;
    let last = events.last().expect("non-empty");
    if first.seq != h.segment.from_seq || last.seq != h.segment.to_seq {
        return Err(VerifyError::Segment(format!("events span {}..{} but the header says {}..{}", first.seq, last.seq, h.segment.from_seq, h.segment.to_seq)));
    }
    if first.prev_hash != h.segment.prev_hash_at_from {
        return Err(VerifyError::Linkage(first.seq));
    }
    let mut prev = first.prev_hash.clone();
    let mut prev_seq = first.seq - 1;
    for e in &events {
        if e.seq != prev_seq + 1 {
            return Err(VerifyError::Segment(format!("gap before seq {}", e.seq)));
        }
        if e.prev_hash != prev {
            return Err(VerifyError::Linkage(e.seq));
        }
        if compute_entry_hash(&e.prev_hash, &canonical_of(e)) != e.entry_hash {
            return Err(VerifyError::EventHash(e.seq));
        }
        prev = e.entry_hash.clone();
        prev_seq = e.seq;
    }
    if last.entry_hash != h.segment.head_hash {
        return Err(VerifyError::Segment("head hash disagrees with the last event".into()));
    }
    // 7. a bind event in the segment commits to a bind the envelope carries
    for e in events.iter().filter(|e| e.kind == EventKind::PrincipalBind.as_str() && e.ref_id.as_deref() == Some(h.chain_id.as_str())) {
        if !env.payload.binds.iter().any(|b| b.payload_hash() == e.payload_hash) {
            return Err(VerifyError::BindHashMismatch(e.seq));
        }
    }
    // 8. bodies: a full text hashes to its body_hash; a prompt event's hash is its row's
    let mut full_bodies = 0;
    for p in &env.payload.prompts {
        if let (Redaction::Full, Some(text)) = (p.redaction, p.text.as_deref()) {
            if sha256_hex(text.as_bytes()) != p.body_hash {
                return Err(VerifyError::BodyHash(p.id));
            }
            full_bodies += 1;
        }
        if let Some(e) = events.iter().find(|e| e.kind == "prompt" && e.prompt_id == Some(p.id)) {
            if e.payload_hash != p.body_hash {
                return Err(VerifyError::BodyHash(p.id));
            }
        }
    }
    Ok(Verified {
        chain_id: h.chain_id.clone(),
        human: h.principal.human.principal_id.clone(),
        device_name: name.to_string(),
        from_seq: h.segment.from_seq,
        to_seq: h.segment.to_seq,
        head_hash: h.segment.head_hash.clone(),
        events: events.len(),
        prompts: env.payload.prompts.len(),
        full_bodies,
        binds: env.payload.binds.len(),
    })
}

/// Verify, then — when the envelope's chain is one this store writes (its
/// device id is a `principals` row here) — check continuity against the
/// local rows: the segment must sit on our chain and agree with it,
/// event for event. Stores nothing (E3 imports foreign chains).
pub fn verify_against(store: &PolisStore, env: &Envelope) -> Result<Verified, VerifyError> {
    let v = verify(env)?;
    let ours = store.get_principal(&env.header.chain_id).map_err(|e| VerifyError::Continuity(e.to_string()))?.is_some_and(|p| p.kind == PrincipalKind::Device);
    if !ours {
        return Ok(v);
    }
    let local = store.list_ledger_events_asc(env.header.segment.from_seq - 1, env.payload.events.len() as i64).map_err(|e| VerifyError::Continuity(e.to_string()))?;
    if env.header.segment.from_seq > 1 {
        let before = store.list_ledger_events_asc(env.header.segment.from_seq - 2, 1).map_err(|e| VerifyError::Continuity(e.to_string()))?;
        match before.first() {
            Some(b) if b.entry_hash == env.header.segment.prev_hash_at_from => {}
            Some(b) => return Err(VerifyError::Continuity(format!("seq {} here has hash {}, the envelope links onto {} — forked", b.seq, b.entry_hash, env.header.segment.prev_hash_at_from))),
            None => return Err(VerifyError::Continuity(format!("this store has no seq {}", env.header.segment.from_seq - 1))),
        }
    }
    let mut remote = env.payload.events.clone();
    remote.sort_by_key(|e| e.seq);
    for r in &remote {
        match local.iter().find(|l| l.seq == r.seq) {
            Some(l) if l.entry_hash == r.entry_hash => {}
            Some(l) => return Err(VerifyError::Continuity(format!("seq {} differs: local {} vs envelope {} — forked", r.seq, l.entry_hash, r.entry_hash))),
            None => {} // past our head: an append we do not store yet (E3)
        }
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::adopt;
    use polis_core::api::{MemoryApi, RememberRequest, Scope};
    use polis_core::host::NoHost;
    use polis_llm::NoopSink;
    use std::sync::Arc;

    fn seeded(seed: u8, device: &str) -> (Arc<PolisStore>, Identity) {
        let store = Arc::new(PolisStore::open_in_memory().unwrap());
        let id = Identity::from_seed([seed; 32], device);
        // a legacy prompt before adoption, then adoption, then a write as the device
        polis_store::record::record_prompt(
            &store,
            polis_store::record::PromptInput {
                source: polis_core::ledger::PromptSource::Api,
                origin: polis_core::ledger::Origin::External,
                surface: "api".into(),
                role: polis_core::ledger::CorpusRole::User,
                session_id: None,
                claude_session_id: None,
                mission_id: None,
                project_path: None,
                body: "before the key existed".into(),
                thread: None,
                author: Some("yusuf".into()),
                model: None,
                model_source: None,
                user_text: None,
            },
        )
        .unwrap();
        adopt(&store, &id, "yusuf").unwrap();
        let handle = crate::PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink)).with_identity(Some(Arc::new(id.clone())));
        handle.remember(&RememberRequest { text: "we chose sqlite".into(), as_user: true, ..Default::default() }).unwrap();
        handle.remember(&RememberRequest { text: "an agent's note".into(), as_user: false, scope: Scope { agent: Some("mcp:test".into()), ..Default::default() }, ..Default::default() }).unwrap();
        (store, id)
    }

    #[test]
    fn a_signed_export_round_trips_and_the_default_policy_stubs_private_bodies() {
        let (store, id) = seeded(11, "laptop");
        let env = build(&store, &id, "yusuf", &BuildOptions::default()).unwrap();
        let v = verify(&env).unwrap();
        assert_eq!(v.chain_id, id.device_id());
        assert_eq!(v.from_seq, 1);
        assert_eq!(v.to_seq, store.chain_head().unwrap().0);
        assert_eq!(v.full_bodies, 0, "private rows ship as stubs by default");
        assert!(env.payload.prompts.iter().all(|p| p.redaction == Redaction::Stub && p.text.is_none()));
        assert!(env.payload.events.iter().any(|e| e.kind == "principal_bind"));
        // continuity against our own store
        verify_against(&store, &env).unwrap();
        // json round trip
        let text = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&text).unwrap();
        assert_eq!(verify(&back).unwrap(), v);
        // the dry run agrees with the payload
        let d = decisions(&store, &BuildOptions::default()).unwrap();
        assert_eq!(d.len(), env.payload.prompts.len());
        assert!(d.iter().all(|x| x.redaction == Redaction::Stub && x.bytes == 0));
    }

    #[test]
    fn full_bodies_ship_when_asked_and_hash_to_the_chain() {
        let (store, id) = seeded(12, "laptop");
        let opts = BuildOptions { policy: Policy { bodies: Bodies::Full, ..Default::default() }, ..Default::default() };
        let env = build(&store, &id, "yusuf", &opts).unwrap();
        let v = verify(&env).unwrap();
        assert_eq!(v.full_bodies, 2, "the two user prompts; the agent note is a note row, not a prompt");
        // a segment from the middle links onto the chain
        let mid = BuildOptions { from_seq: Some(2), ..opts.clone() };
        let env2 = build(&store, &id, "yusuf", &mid).unwrap();
        assert_eq!(env2.header.segment.from_seq, 2);
        verify_against(&store, &env2).unwrap();
    }

    #[test]
    fn tampering_the_payload_a_wrong_key_and_a_missing_bind_each_fail_by_name() {
        let (store, id) = seeded(13, "laptop");
        let opts = BuildOptions { policy: Policy { bodies: Bodies::Full, ..Default::default() }, ..Default::default() };
        let good = build(&store, &id, "yusuf", &opts).unwrap();

        // a byte in a body
        let mut t = good.clone();
        let p = t.payload.prompts.iter_mut().find(|p| p.text.is_some()).unwrap();
        p.text = Some(p.text.take().unwrap() + "!");
        assert_eq!(verify(&t).unwrap_err(), VerifyError::PayloadHash);

        // a byte in an event (re-hash the payload so only the chain catches it)
        let mut t = good.clone();
        t.payload.events[0].author.push('x');
        t.header.payload_sha256 = sha256_hex(t.payload_json().as_bytes());
        t.signature = id.sign_hex(t.header_line().as_bytes());
        assert!(matches!(verify(&t).unwrap_err(), VerifyError::EventHash(1)));

        // the wrong key signed the header
        let other = Identity::from_seed([99; 32], "laptop");
        let mut t = good.clone();
        t.signature = other.sign_hex(t.header_line().as_bytes());
        assert_eq!(verify(&t).unwrap_err(), VerifyError::BadSignature);

        // a key that is not the human on the card
        let mut t = good.clone();
        t.header.principal.human.pubkey = Some(other.pubkey_hex());
        assert_eq!(verify(&t).unwrap_err(), VerifyError::IdMismatch);

        // the bind is gone
        let mut t = good.clone();
        t.payload.binds.clear();
        t.header.payload_sha256 = sha256_hex(t.payload_json().as_bytes());
        t.signature = id.sign_hex(t.header_line().as_bytes());
        assert_eq!(verify(&t).unwrap_err(), VerifyError::NoBind);

        // a forged bind
        let mut t = good.clone();
        t.payload.binds[0].signature = other.sign_hex(&bind_message(&t.payload.binds[0].chain_id, &t.payload.binds[0].head_hash));
        t.header.payload_sha256 = sha256_hex(t.payload_json().as_bytes());
        t.signature = id.sign_hex(t.header_line().as_bytes());
        assert_eq!(verify(&t).unwrap_err(), VerifyError::BadBindSignature);

        // a device name that does not derive to the chain id
        let mut t = good.clone();
        t.header.principal.device.display_name = Some("desk".into());
        t.signature = id.sign_hex(t.header_line().as_bytes());
        assert_eq!(verify(&t).unwrap_err(), VerifyError::DeviceMismatch);
    }

    #[test]
    fn a_forked_segment_of_our_own_chain_is_refused_on_continuity() {
        let (store, id) = seeded(14, "laptop");
        let env = build(&store, &id, "yusuf", &BuildOptions::default()).unwrap();
        // pretend the peer's copy diverged at seq 2: same seq, different bytes
        let mut t = env.clone();
        let e = t.payload.events.iter_mut().find(|e| e.seq == 2).unwrap();
        e.ts += 1;
        // rebuild the chain from seq 2 so the envelope itself verifies
        let mut prev = t.payload.events[0].entry_hash.clone();
        for e in t.payload.events.iter_mut().filter(|e| e.seq >= 2) {
            e.prev_hash = prev.clone();
            e.entry_hash = compute_entry_hash(&e.prev_hash, &canonical_of(e));
            prev = e.entry_hash.clone();
        }
        t.header.segment.head_hash = prev;
        t.header.payload_sha256 = sha256_hex(t.payload_json().as_bytes());
        t.signature = id.sign_hex(t.header_line().as_bytes());
        verify(&t).expect("internally consistent");
        assert!(matches!(verify_against(&store, &t).unwrap_err(), VerifyError::Continuity(m) if m.contains("forked")));
    }
}
