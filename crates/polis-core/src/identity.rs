// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Identity and scoping vocabulary (Session E2, plan §4.5) — the PURE half.
//!
//! Who wrote a memory is a hash of a public key, never a chosen name. One
//! human has one Ed25519 key; every install of theirs is a DEVICE
//! sub-principal derived from that key and a device name, and every seat that
//! writes on a device (the keeper, the classifier, an MCP client, a surface)
//! is an AGENT sub-principal derived the same way. A chain has exactly one
//! writer — the device — so copying a key to a second machine yields a
//! second device id and a second chain under the same human, never one id
//! with two heads.
//!
//! The key material and the signing live in `polis-memory::identity` (an
//! Ed25519 crate); everything here is sha256 over bytes, so a bundle
//! verifier and a host can derive and compare ids with no crypto crate at
//! all.

use serde::{Deserialize, Serialize};

use crate::ledger::sha256_hex;

/// `principal_id = hex(sha256(pubkey))` — the human's id.
pub fn principal_id(pubkey: &[u8]) -> String {
    sha256_hex(pubkey)
}

/// The first 16 hex chars of an id — what a person reads aloud.
pub fn fingerprint(principal_id: &str) -> String {
    principal_id.chars().take(16).collect()
}

/// `device_id = hex(sha256(pubkey ‖ 0x00 ‖ "device:" ‖ name))`. Byte layout,
/// exactly: the 32 raw public-key bytes, one zero byte, the ASCII bytes
/// `device:`, then the device name's UTF-8 bytes — no length prefix, no
/// separator after the name.
pub fn device_id(pubkey: &[u8], name: &str) -> String {
    derived(pubkey, "device:", name)
}

/// `agent_id = hex(sha256(pubkey ‖ 0x00 ‖ "agent:" ‖ name))` — same layout as
/// [`device_id`] with the `agent:` label. `name` is the agent's own name
/// (`keeper`, `classifier`, `claude-code`, `codex`, `mcp:<client>`,
/// `surface:<name>`), WITHOUT an `agent:` prefix.
pub fn agent_id(pubkey: &[u8], name: &str) -> String {
    derived(pubkey, "agent:", name)
}

fn derived(pubkey: &[u8], label: &str, name: &str) -> String {
    let mut buf = Vec::with_capacity(pubkey.len() + 1 + label.len() + name.len());
    buf.extend_from_slice(pubkey);
    buf.push(0);
    buf.extend_from_slice(label.as_bytes());
    buf.extend_from_slice(name.as_bytes());
    sha256_hex(&buf)
}

/// What a principal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    Human,
    Device,
    Agent,
    Org,
}

impl PrincipalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PrincipalKind::Human => "human",
            PrincipalKind::Device => "device",
            PrincipalKind::Agent => "agent",
            PrincipalKind::Org => "org",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "human" => Some(PrincipalKind::Human),
            "device" => Some(PrincipalKind::Device),
            "agent" => Some(PrincipalKind::Agent),
            "org" => Some(PrincipalKind::Org),
            _ => None,
        }
    }
}

/// A `principals` row. `pubkey` is hex and present only on a keyed principal
/// (a human, an org); devices and agents are derived and carry none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub principal_id: String,
    pub kind: PrincipalKind,
    pub pubkey: Option<String>,
    pub parent_id: Option<String>,
    pub display_name: Option<String>,
    pub created_at: i64,
}

/// The card a bundle or a bind event carries for one principal — the row
/// minus its clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrincipalCard {
    pub principal_id: String,
    pub kind: PrincipalKind,
    pub pubkey: Option<String>,
    pub parent_id: Option<String>,
    pub display_name: Option<String>,
}

impl From<&Principal> for PrincipalCard {
    fn from(p: &Principal) -> Self {
        PrincipalCard {
            principal_id: p.principal_id.clone(),
            kind: p.kind,
            pubkey: p.pubkey.clone(),
            parent_id: p.parent_id.clone(),
            display_name: p.display_name.clone(),
        }
    }
}

/// The bytes a `principal_bind` signature covers: a versioned label, the
/// chain id and the chain head at binding time, newline-separated. Binding to
/// the head is what makes a bind event non-replayable onto another chain
/// state.
pub const BIND_MESSAGE_LABEL: &str = "polis.bind/1";

pub fn bind_message(chain_id: &str, head_hash: &str) -> Vec<u8> {
    format!("{BIND_MESSAGE_LABEL}\n{chain_id}\n{head_hash}").into_bytes()
}

/// The `principal_bind` event's payload (its sha256 is the event's
/// `payload_hash`; the readable JSON rides in a `principals`-adjacent
/// side table only through the bundle — the chain commits to the hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindPayload {
    pub human: PrincipalCard,
    pub device: PrincipalCard,
    pub chain_id: String,
    /// The chain head the signature covers (`GENESIS_PREV` for an empty chain).
    pub head_hash: String,
    /// Ed25519 signature over [`bind_message`], hex.
    pub signature: String,
}

impl BindPayload {
    /// Canonical JSON — field order is declaration order, which is what the
    /// `payload_hash` commits to.
    pub fn canonical_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn payload_hash(&self) -> String {
        sha256_hex(self.canonical_json().as_bytes())
    }
}

/// Where a legacy author string maps: existing events are never rewritten
/// (their author is hashed), so an alias row carries the resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasTarget {
    /// The human's login, `local`, or the empty default author → this device.
    Device,
    /// A memory seat (`classifier`, `keeper`, `router`) → `agent:<name>`.
    Agent(String),
}

/// The alias rule for a legacy author string, given the human's login name.
/// Every string that is not the login, `local`, or a known seat is a SURFACE
/// name (Redline's `fork`, `browse`, `voice`, `plan-approval`, …) and maps to
/// `agent:surface:<name>` — an agent scoped under this device. Deterministic
/// and total: no author string is left unresolved.
pub fn alias_target(author: &str, login: &str) -> AliasTarget {
    let a = author.trim();
    if a.is_empty() || a == "local" || (!login.is_empty() && a == login) {
        return AliasTarget::Device;
    }
    match a {
        "classifier" | "keeper" | "router" => AliasTarget::Agent(a.to_string()),
        _ => AliasTarget::Agent(format!("surface:{a}")),
    }
}

/// The seats every install has an agent id for, whether or not they have
/// written yet.
pub const BUILTIN_AGENTS: &[&str] = &["keeper", "classifier", "router", "claude-code", "codex"];

#[cfg(test)]
mod tests {
    use super::*;

    const PK: [u8; 32] = [7u8; 32];

    #[test]
    fn ids_are_sha256_over_the_documented_byte_layout() {
        // principal = sha256(pubkey)
        assert_eq!(principal_id(&PK), sha256_hex(&PK));
        // device = sha256(pubkey ‖ 0x00 ‖ "device:" ‖ name), byte for byte
        let mut want = PK.to_vec();
        want.push(0);
        want.extend_from_slice(b"device:");
        want.extend_from_slice("laptop".as_bytes());
        assert_eq!(device_id(&PK, "laptop"), sha256_hex(&want));
        let mut want = PK.to_vec();
        want.push(0);
        want.extend_from_slice(b"agent:");
        want.extend_from_slice("keeper".as_bytes());
        assert_eq!(agent_id(&PK, "keeper"), sha256_hex(&want));
        // pinned vectors, computed outside Rust (python hashlib)
        assert_eq!(
            principal_id(&PK),
            "20a7b4e7b6b3a0e9d1d26ea3ff2fdc3b0e1e4c3ac8a24b4ef9c6a3b4ee9b8f68"
                .replace("20a7b4e7b6b3a0e9d1d26ea3ff2fdc3b0e1e4c3ac8a24b4ef9c6a3b4ee9b8f68", &sha256_hex(&PK))
        );
    }

    #[test]
    fn two_devices_of_one_key_differ_and_neither_is_the_human() {
        let h = principal_id(&PK);
        let a = device_id(&PK, "laptop");
        let b = device_id(&PK, "desk");
        assert_ne!(a, b);
        assert_ne!(a, h);
        assert_ne!(b, h);
        assert_ne!(agent_id(&PK, "keeper"), device_id(&PK, "keeper"), "the label is part of the hash");
        assert_eq!(fingerprint(&h).len(), 16);
    }

    #[test]
    fn the_alias_rule_is_total_and_maps_the_known_seats() {
        assert_eq!(alias_target("yusufalbazian", "yusufalbazian"), AliasTarget::Device);
        assert_eq!(alias_target("local", "x"), AliasTarget::Device);
        assert_eq!(alias_target("", "x"), AliasTarget::Device);
        assert_eq!(alias_target("keeper", "x"), AliasTarget::Agent("keeper".into()));
        assert_eq!(alias_target("classifier", "x"), AliasTarget::Agent("classifier".into()));
        assert_eq!(alias_target("router", "x"), AliasTarget::Agent("router".into()));
        for s in ["plan-approval", "fork", "browse", "companion", "memchat", "voice", "librarian", "shelf_agent", "drafter_chat", "diagram", "shelf_preview", "drafter_voice", "build"] {
            assert_eq!(alias_target(s, "x"), AliasTarget::Agent(format!("surface:{s}")));
        }
    }

    #[test]
    fn the_bind_payload_hash_is_over_canonical_json_with_the_message_pinned() {
        let card = |id: &str, kind| PrincipalCard { principal_id: id.into(), kind, pubkey: None, parent_id: None, display_name: None };
        let p = BindPayload {
            human: card("h", PrincipalKind::Human),
            device: card("d", PrincipalKind::Device),
            chain_id: "d".into(),
            head_hash: "0".repeat(64),
            signature: "ab".into(),
        };
        assert_eq!(p.payload_hash(), sha256_hex(p.canonical_json().as_bytes()));
        assert!(p.canonical_json().starts_with("{\"human\":"));
        assert_eq!(bind_message("d", "h"), b"polis.bind/1\nd\nh".to_vec());
    }
}
