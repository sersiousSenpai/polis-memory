// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Identity (Session E2, plan §4.5): the Ed25519 key, the device it names,
//! the bind event, and adoption of a store — legacy authors aliased, scope
//! columns stamped — so a human, their devices and their agents are ids
//! derived from one key and never chosen names.
//!
//! One human, many devices, one chain per device: `chain_id = device_id =
//! sha256(pubkey ‖ 0x00 ‖ "device:" ‖ name)`. Copying `identity.key` to a
//! second machine with a different device name yields a second device under
//! the same human — two chains, never one id with two heads.
//!
//! Files: `identity.key` (the 32-byte seed, hex, created 0600 BEFORE the
//! bytes land — the file is opened with mode 0o600, then written; on
//! Windows the parent directory's ACL is the boundary until the owner-only
//! ACL lands), `identity.pub` (the public key, hex).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use polis_core::identity::{
    agent_id, alias_target, bind_message, device_id, fingerprint, principal_id, AliasTarget, BindPayload, Principal, PrincipalCard,
    PrincipalKind, BUILTIN_AGENTS,
};
use polis_core::ledger::{now_millis, EventKind, LedgerAppend};
use polis_store::principals::StampReport;
use polis_store::PolisStore;

pub const KEY_FILE: &str = "identity.key";
pub const PUB_FILE: &str = "identity.pub";
/// The `polis_meta` key under which a device's bind payload (the readable
/// JSON the chain's `payload_hash` commits to) is kept for export.
pub const BIND_META_PREFIX: &str = "polis.identity.bind.";

/// The key and the device it is used from.
#[derive(Clone)]
pub struct Identity {
    signing: SigningKey,
    pub device_name: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity").field("principal", &fingerprint(&self.principal_id())).field("device", &self.device_name).finish()
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// The default device name: the hostname, else `device`.
pub fn default_device_name() -> String {
    for var in ["POLIS_DEVICE", "HOSTNAME", "COMPUTERNAME"] {
        if let Some(v) = std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            return v;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "device".to_string())
}

/// The human's login name, for the alias rule and the display name.
pub fn login_name() -> String {
    for var in ["USER", "USERNAME", "LOGNAME"] {
        if let Some(v) = std::env::var(var).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
            return v;
        }
    }
    "local".to_string()
}

impl Identity {
    /// A fresh key from the OS's randomness.
    pub fn generate(device_name: impl Into<String>) -> Result<Self, String> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| format!("os randomness: {e}"))?;
        Ok(Self::from_seed(seed, device_name))
    }

    pub fn from_seed(seed: [u8; 32], device_name: impl Into<String>) -> Self {
        Identity { signing: SigningKey::from_bytes(&seed), device_name: device_name.into() }
    }

    pub fn seed(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }

    pub fn pubkey(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn pubkey_hex(&self) -> String {
        hex_encode(&self.pubkey())
    }

    pub fn principal_id(&self) -> String {
        principal_id(&self.pubkey())
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.principal_id())
    }

    /// The device's id — the chain id.
    pub fn device_id(&self) -> String {
        device_id(&self.pubkey(), &self.device_name)
    }

    /// An agent under this device. `name` may carry the `agent:` prefix or
    /// not; the id is the same.
    pub fn agent_id(&self, name: &str) -> String {
        agent_id(&self.pubkey(), name.strip_prefix("agent:").unwrap_or(name))
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.signing.sign(msg).to_bytes()
    }

    pub fn sign_hex(&self, msg: &[u8]) -> String {
        hex_encode(&self.sign(msg))
    }

    /// Verify with a bare public key — what an importer with no key does.
    pub fn verify(pubkey: &[u8], msg: &[u8], signature: &[u8]) -> bool {
        let Ok(pk) = <[u8; 32]>::try_from(pubkey) else { return false };
        let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return false };
        let Ok(sig) = <[u8; 64]>::try_from(signature) else { return false };
        vk.verify(msg, &Signature::from_bytes(&sig)).is_ok()
    }

    pub fn verify_hex(pubkey_hex: &str, msg: &[u8], signature_hex: &str) -> bool {
        match (hex_decode(pubkey_hex), hex_decode(signature_hex)) {
            (Some(pk), Some(sig)) => Self::verify(&pk, msg, &sig),
            _ => false,
        }
    }

    pub fn human_card(&self, login: &str) -> PrincipalCard {
        PrincipalCard {
            principal_id: self.principal_id(),
            kind: PrincipalKind::Human,
            pubkey: Some(self.pubkey_hex()),
            parent_id: None,
            display_name: Some(login.to_string()),
        }
    }

    pub fn device_card(&self) -> PrincipalCard {
        PrincipalCard {
            principal_id: self.device_id(),
            kind: PrincipalKind::Device,
            pubkey: None,
            parent_id: Some(self.principal_id()),
            display_name: Some(self.device_name.clone()),
        }
    }

    // ---- files -----------------------------------------------------------

    pub fn key_path(dir: &Path) -> PathBuf {
        dir.join(KEY_FILE)
    }

    /// Load the key from `dir`, if one exists there.
    pub fn load(dir: &Path, device_name: impl Into<String>) -> Result<Option<Self>, String> {
        let path = Self::key_path(dir);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let bytes = hex_decode(&text).ok_or_else(|| format!("{} is not a hex seed", path.display()))?;
        let seed: [u8; 32] = bytes.try_into().map_err(|_| format!("{} is not a 32-byte seed", path.display()))?;
        Ok(Some(Self::from_seed(seed, device_name)))
    }

    /// Write `identity.key` (0600 before any byte lands) and `identity.pub`.
    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        write_private(&Self::key_path(dir), hex_encode(&self.seed()).as_bytes())?;
        std::fs::write(dir.join(PUB_FILE), self.pubkey_hex().as_bytes()).map_err(|e| format!("write {}: {e}", dir.join(PUB_FILE).display()))?;
        Ok(())
    }

    /// The key in `dir`, created if absent. `(identity, created)`.
    pub fn load_or_create(dir: &Path, device_name: impl Into<String>) -> Result<(Self, bool), String> {
        let name = device_name.into();
        if let Some(id) = Self::load(dir, name.clone())? {
            return Ok((id, false));
        }
        let id = Self::generate(name)?;
        id.save(dir)?;
        Ok((id, true))
    }
}

/// A private file: opened with mode 0o600 so no byte is ever readable by
/// another user, then written. On Windows the parent directory's ACL is the
/// boundary until the owner-only ACL lands.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Adoption
// ---------------------------------------------------------------------------

/// What adopting a store did. Idempotent by construction: a second call
/// seeds nothing, aliases nothing, binds nothing and stamps nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptReport {
    pub human: String,
    pub device: String,
    pub device_name: String,
    pub principals_seeded: usize,
    /// `(author string, principal id)` pairs written this call.
    pub aliases_seeded: Vec<(String, String)>,
    /// The `principal_bind` event appended this call, or the one that already
    /// bound this device.
    pub bind_seq: Option<i64>,
    pub already_bound: bool,
    pub stamped: usize,
    /// Rows per table still carrying no scope after the sweep.
    pub unscoped: StampSummary,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StampSummary {
    pub prompts: usize,
    pub browse_events: usize,
    pub user_notes: usize,
    pub class_nodes: usize,
    pub class_observations: usize,
}

impl From<StampReport> for StampSummary {
    fn from(r: StampReport) -> Self {
        StampSummary { prompts: r.prompts, browse_events: r.browse_events, user_notes: r.user_notes, class_nodes: r.class_nodes, class_observations: r.class_observations }
    }
}

fn principal_row(card: &PrincipalCard, now: i64) -> Principal {
    Principal {
        principal_id: card.principal_id.clone(),
        kind: card.kind,
        pubkey: card.pubkey.clone(),
        parent_id: card.parent_id.clone(),
        display_name: card.display_name.clone(),
        created_at: now,
    }
}

/// Adopt a store under this identity: the human, the device and the builtin
/// agents become `principals` rows; every legacy author string the lake has
/// ever seen gets an alias (the login and `local` → this device; the memory
/// seats → their agents; any other name → `agent:surface:<name>` under this
/// device); a `principal_bind` event binds the key to the chain once; then
/// every unscoped row is stamped. Existing events are never rewritten.
///
/// A host runs this on every boot (Redline on attach); the CLI on `init`.
pub fn adopt(store: &PolisStore, identity: &Identity, login: &str) -> Result<AdoptReport, String> {
    let now = now_millis();
    let human = identity.principal_id();
    let device = identity.device_id();
    let mut report = AdoptReport { human: human.clone(), device: device.clone(), device_name: identity.device_name.clone(), ..Default::default() };
    let err = |e: rusqlite::Error| e.to_string();

    let before = store.list_principals().map_err(err)?.len();
    store.upsert_principal(&principal_row(&identity.human_card(login), now)).map_err(err)?;
    store.upsert_principal(&principal_row(&identity.device_card(), now)).map_err(err)?;
    let agent = |name: &str| -> Result<String, String> {
        let id = identity.agent_id(name);
        store
            .upsert_principal(&Principal {
                principal_id: id.clone(),
                kind: PrincipalKind::Agent,
                pubkey: None,
                parent_id: Some(device.clone()),
                display_name: Some(format!("agent:{name}")),
                created_at: now,
            })
            .map_err(err)?;
        Ok(id)
    };
    for name in BUILTIN_AGENTS {
        agent(name)?;
    }
    // Aliases for every author string ever recorded — the rule is total, so
    // nothing is left unresolved. The device's own id and agent ids are
    // principals already and need no alias.
    let existing: std::collections::HashSet<String> = store.list_aliases().map_err(err)?.into_iter().map(|(a, _)| a).collect();
    let mut authors = store.distinct_authors().map_err(err)?;
    for builtin in [login.to_string(), "local".to_string()] {
        if !authors.contains(&builtin) {
            authors.push(builtin);
        }
    }
    for author in authors {
        if author.trim().is_empty() || existing.contains(&author) {
            continue;
        }
        if store.get_principal(&author).map_err(err)?.is_some() {
            continue; // an id, not a legacy name
        }
        let target = match alias_target(&author, login) {
            AliasTarget::Device => device.clone(),
            AliasTarget::Agent(name) => agent(&name)?,
        };
        if store.set_alias(&author, &target).map_err(err)? {
            report.aliases_seeded.push((author, target));
        }
    }
    report.principals_seeded = store.list_principals().map_err(err)?.len().saturating_sub(before);

    // The bind: once per device. Signs the chain id and the head at binding
    // time, so the event cannot be replayed onto another chain state.
    match store.bind_seq_for(&device).map_err(err)? {
        Some(seq) => {
            report.bind_seq = Some(seq);
            report.already_bound = true;
        }
        None => {
            let (_, head_hash) = store.chain_head().map_err(err)?;
            let signature = identity.sign_hex(&bind_message(&device, &head_hash));
            let payload = BindPayload {
                human: identity.human_card(login),
                device: identity.device_card(),
                chain_id: device.clone(),
                head_hash,
                signature,
            };
            let payload_hash = payload.payload_hash();
            let row = store
                .append_event(&LedgerAppend {
                    kind: EventKind::PrincipalBind.as_str(),
                    author: &device,
                    ts: now,
                    prompt_id: None,
                    session_id: None,
                    version_number: None,
                    ref_kind: Some("principal"),
                    ref_id: Some(&device),
                    payload_hash: &payload_hash,
                })
                .map_err(|e| e.to_string())?;
            store.set_meta(&format!("{BIND_META_PREFIX}{device}"), &payload.canonical_json()).map_err(|e| e.to_string())?;
            report.bind_seq = Some(row.seq);
        }
    }

    report.stamped = store.stamp_unscoped().map_err(err)?.total();
    report.unscoped = store.unscoped_counts().map_err(err)?.into();
    Ok(report)
}

/// The bind payload a device's `principal_bind` event committed to, as kept
/// beside the chain (what an export ships so an importer can verify the
/// bind's signature without the store).
pub fn bind_payload_for(store: &PolisStore, device_id: &str) -> Result<Option<BindPayload>, String> {
    let Some(json) = store.meta(&format!("{BIND_META_PREFIX}{device_id}")).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    serde_json::from_str(&json).map(Some).map_err(|e| e.to_string())
}

/// The actor a write is stamped with: an agent id when the scope names an
/// agent (`agent:mcp:<client>`, a seat), else the device — never a chosen
/// name once an identity exists.
pub fn actor_for(identity: Option<&Arc<Identity>>, scope_agent: Option<&str>, fallback: &str) -> String {
    match identity {
        Some(id) => match scope_agent.map(str::trim).filter(|s| !s.is_empty()) {
            Some(a) => id.agent_id(a),
            None => id.device_id(),
        },
        None => fallback.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("polis-identity-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_key_round_trips_through_its_files_and_the_key_file_is_private() {
        let dir = tmpdir("files");
        let (a, created) = Identity::load_or_create(&dir, "laptop").unwrap();
        assert!(created);
        let (b, created) = Identity::load_or_create(&dir, "laptop").unwrap();
        assert!(!created);
        assert_eq!(a.principal_id(), b.principal_id());
        assert_eq!(a.device_id(), b.device_id());
        assert_eq!(std::fs::read_to_string(dir.join(PUB_FILE)).unwrap(), a.pubkey_hex());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "identity.key must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signatures_verify_with_the_bare_public_key_and_fail_on_any_change() {
        let id = Identity::from_seed([1u8; 32], "box");
        let sig = id.sign(b"hello");
        assert!(Identity::verify(&id.pubkey(), b"hello", &sig));
        assert!(!Identity::verify(&id.pubkey(), b"hellp", &sig));
        let other = Identity::from_seed([2u8; 32], "box");
        assert!(!Identity::verify(&other.pubkey(), b"hello", &sig));
        assert!(Identity::verify_hex(&id.pubkey_hex(), b"hello", &id.sign_hex(b"hello")));
        assert_eq!(id.agent_id("agent:keeper"), id.agent_id("keeper"), "the prefix is not part of the name");
    }

    #[test]
    fn one_key_two_device_names_is_two_devices_under_one_human() {
        let a = Identity::from_seed([5u8; 32], "laptop");
        let b = Identity::from_seed([5u8; 32], "desk");
        assert_eq!(a.principal_id(), b.principal_id());
        assert_ne!(a.device_id(), b.device_id());
        assert_eq!(a.device_card().parent_id.as_deref(), Some(a.principal_id().as_str()));
    }

    #[test]
    fn adoption_seeds_aliases_binds_once_and_stamps_then_is_a_no_op() {
        let store = PolisStore::open_in_memory().unwrap();
        // legacy history: prompts by the login, a keeper compaction-shaped
        // event, a surface prompt
        for (author, body) in [("yusuf", "first"), ("fork", "second"), ("keeper", "third")] {
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
                    body: body.into(),
                    thread: None,
                    author: Some(author.into()),
                    model: None,
                    model_source: None,
                    user_text: None,
                },
            )
            .unwrap();
        }
        let id = Identity::from_seed([8u8; 32], "laptop");
        let r = adopt(&store, &id, "yusuf").unwrap();
        assert_eq!(r.human, id.principal_id());
        assert_eq!(r.device, id.device_id());
        assert!(!r.already_bound);
        let bind = r.bind_seq.unwrap();
        let mut aliases: Vec<String> = r.aliases_seeded.iter().map(|(a, _)| a.clone()).collect();
        aliases.sort();
        assert_eq!(aliases, ["fork", "keeper", "local", "yusuf"]);
        assert_eq!(store.resolve_author("yusuf").unwrap().as_deref(), Some(id.device_id().as_str()));
        assert_eq!(store.resolve_author("keeper").unwrap().as_deref(), Some(id.agent_id("keeper").as_str()));
        assert_eq!(store.resolve_author("fork").unwrap().as_deref(), Some(id.agent_id("surface:fork").as_str()));
        assert_eq!(r.stamped, 3);
        assert_eq!(r.unscoped, StampSummary::default());
        // the chain verifies with the bind as head, and its payload is kept
        let verdict = store.verify_ledger_chain().unwrap();
        assert!(verdict.ok);
        assert_eq!(store.chain_head().unwrap().0, bind);
        let payload = bind_payload_for(&store, &id.device_id()).unwrap().unwrap();
        assert!(Identity::verify_hex(&id.pubkey_hex(), &bind_message(&payload.chain_id, &payload.head_hash), &payload.signature));
        // idempotent
        let again = adopt(&store, &id, "yusuf").unwrap();
        assert!(again.already_bound);
        assert_eq!(again.bind_seq, Some(bind));
        assert!(again.aliases_seeded.is_empty());
        assert_eq!(again.principals_seeded, 0);
        assert_eq!(again.stamped, 0);
        assert_eq!(store.chain_head().unwrap().0, bind, "a second adoption appends nothing");
    }

    #[test]
    fn the_actor_is_the_device_or_the_named_agent_or_the_legacy_fallback() {
        let id = Arc::new(Identity::from_seed([3u8; 32], "box"));
        assert_eq!(actor_for(Some(&id), None, "x"), id.device_id());
        assert_eq!(actor_for(Some(&id), Some("mcp:claude"), "x"), id.agent_id("mcp:claude"));
        assert_eq!(actor_for(Some(&id), Some("agent:keeper"), "x"), id.agent_id("keeper"));
        assert_eq!(actor_for(None, Some("keeper"), "legacy"), "legacy");
    }
}
