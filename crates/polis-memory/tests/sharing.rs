// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Session E3's gates (plan §4.6), as tests: two homes on one machine
//! exchange signed segments through a folder and through a bare git repo;
//! union search finds a peer's prompt only under `include_shared`, labelled;
//! a rewritten segment is refused as forked; a foreign vector under another
//! model id is discarded and re-embedded; a redaction tombstones the body
//! on the other side after its next sync; an unknown key is refused without
//! TOFU; a subscription filters what lands.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use polis_core::api::{ForgetRequest, MemoryApi, RememberRequest, Scope};
use polis_core::host::NoHost;
use polis_embed::Embedder;
use polis_llm::NoopSink;
use polis_memory::envelope::{self, Bodies, BuildOptions, Policy};
use polis_memory::identity::{adopt, Identity};
use polis_memory::sharing::{self, ImportError, ImportOptions, ImportOutcome};
use polis_memory::sync::{sync, SyncOptions};
use polis_memory::transport::{FolderTransport, GitTransport, SegmentTransport, Subscription};
use polis_memory::{retrieval, PolisHandle};
use polis_store::principals::ScopeFilter;
use polis_store::PolisStore;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("polis-e3-{}-{}-{label}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A home: a store file, an identity, an adopted chain, a handle that
/// writes as the device.
struct Home {
    dir: PathBuf,
    store: Arc<PolisStore>,
    identity: Identity,
    handle: PolisHandle,
    login: &'static str,
}

fn home(seed: u8, device: &str, login: &'static str, embedder: Option<Arc<dyn Embedder>>) -> Home {
    let dir = tmpdir(device);
    let store = Arc::new(PolisStore::open(&dir.join("polis.db")).unwrap());
    let identity = Identity::from_seed([seed; 32], device);
    adopt(&store, &identity, login).unwrap();
    let handle = PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink))
        .with_identity(Some(Arc::new(identity.clone())))
        .with_embedder(embedder);
    Home { dir, store, identity, handle, login }
}

impl Home {
    fn remember(&self, text: &str) -> i64 {
        self.handle
            .remember(&RememberRequest { text: text.into(), as_user: true, ..Default::default() })
            .unwrap();
        // org-visible so the default policy ships the body
        let conn = self.store.conn();
        conn.execute("UPDATE prompts SET visibility = 'org' WHERE body = ?1", rusqlite::params![text]).unwrap();
        conn.query_row("SELECT id FROM prompts WHERE body = ?1", rusqlite::params![text], |r| r.get(0)).unwrap()
    }

    fn export(&self, from_seq: Option<i64>, vectors: bool) -> envelope::Envelope {
        let opts = BuildOptions { from_seq, policy: Policy::default(), org_id: None, include_vectors: vectors };
        envelope::build(&self.store, &self.identity, self.login, &opts).unwrap()
    }

    fn shared_search(&self, q: &str, include_shared: bool) -> Vec<polis_core::pack::ForeignHit> {
        let view = self.handle.view();
        let scope = ScopeFilter { include_shared, ..Default::default() };
        retrieval::build_answer_pack_scoped(&view, Some(q), None, 20, &scope).shared_hits
    }

    async fn sync_with(&self, t: &dyn SegmentTransport, tofu: bool) -> polis_memory::sync::SyncReport {
        let opts = SyncOptions { tofu, ..Default::default() };
        sync(&self.store, &self.identity, self.login, t, &opts).await
    }
}

fn tofu() -> ImportOptions {
    ImportOptions { tofu: true, force: false, local_model: None }
}

/// A deterministic embedder: a bag-of-words vector, so identical text is
/// identical, under a chosen model id.
struct Fake {
    model: &'static str,
}

impl Embedder for Fake {
    fn model_id(&self) -> String {
        self.model.into()
    }
    fn dim(&self) -> usize {
        16
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; 16];
                for w in t.split_whitespace() {
                    let h = polis_core::ledger::sha256_hex(w.to_ascii_lowercase().as_bytes());
                    let ix = usize::from_str_radix(&h[..2], 16).unwrap() % 16;
                    v[ix] += 1.0;
                }
                v
            })
            .collect())
    }
}

#[tokio::test]
async fn two_homes_exchange_segments_through_a_folder_and_union_search_labels_the_peer() {
    let a = home(1, "laptop-a", "alice", None);
    let b = home(2, "laptop-b", "bob", None);
    a.remember("alice chose rusqlite bundled for the store");
    b.remember("bob prefers trigram grep over regex");
    let folder = FolderTransport::new(tmpdir("folder"));

    let ra = a.sync_with(&folder, true).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    assert!(ra.published.is_some(), "A publishes its chain");
    let rb = b.sync_with(&folder, true).await;
    assert!(rb.errors.is_empty(), "{:?}", rb.errors);
    assert_eq!(rb.imported.len(), 1, "B imports A's chain");
    assert!(matches!(rb.imported[0].outcome, ImportOutcome::Appended { .. }));
    assert!(rb.imported[0].trusted_now.is_some(), "TOFU printed A's fingerprint");
    let ra2 = a.sync_with(&folder, true).await;
    assert_eq!(ra2.imported.len(), 1, "A imports B's chain on its next sync");

    // union search: the peer's prompt only under include_shared, labelled, never in prompt_hits
    let hits = b.shared_search("rusqlite bundled", true);
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert!(hits[0].source.starts_with("shared:"), "labelled by source: {}", hits[0].source);
    assert_eq!(hits[0].chain_id, a.identity.device_id());
    assert_eq!(hits[0].text.as_deref(), Some("alice chose rusqlite bundled for the store"));
    assert!(b.shared_search("rusqlite bundled", false).is_empty(), "nothing shared without include_shared");
    let view = b.handle.view();
    let pack = retrieval::build_answer_pack_scoped(&view, Some("rusqlite bundled"), None, 20, &ScopeFilter { include_shared: true, ..Default::default() });
    assert!(pack.prompt_hits.iter().all(|h| h.item.body.as_deref() != Some("alice chose rusqlite bundled for the store")), "a peer's words never sit in prompt_hits");
    assert!(pack.arm_coverage.iter().any(|c| c.arm == polis_core::pack::Arm::Shared && c.ran && c.hits == 1));
    // the rendered block labels it as third-party
    let block = retrieval::context_block_scoped(&view, "rusqlite bundled", None, 4000, &ScopeFilter { include_shared: true, ..Default::default() });
    let text = block.text.clone().unwrap_or_default();
    assert!(text.contains("SHARED (third-party"), "{text}");
    // a second sync is a no-op on both sides
    let rb2 = b.sync_with(&folder, true).await;
    assert!(rb2.imported.is_empty() && rb2.published.is_none(), "{rb2:?}");
    assert_eq!(b.store.list_foreign_chains().unwrap().len(), 1);
}

#[tokio::test]
async fn two_homes_exchange_segments_through_a_bare_git_repo() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        eprintln!("no git on PATH — skipped");
        return;
    }
    let bare = tmpdir("bare.git");
    let out = std::process::Command::new("git").args(["init", "--quiet", "--bare", "-b", "main"]).arg(&bare).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let a = home(3, "desk-a", "alice", None);
    let b = home(4, "desk-b", "bob", None);
    a.remember("alice: the org node relays envelopes over https");
    b.remember("bob: a folder on a synced drive is enough for two machines");
    let ta = GitTransport::new(bare.to_string_lossy().to_string(), tmpdir("clone-a"));
    let tb = GitTransport::new(bare.to_string_lossy().to_string(), tmpdir("clone-b"));

    let ra = a.sync_with(&ta, true).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    assert!(ra.published.is_some());
    let rb = b.sync_with(&tb, true).await;
    assert!(rb.errors.is_empty(), "{:?}", rb.errors);
    assert_eq!(rb.imported.len(), 1, "B pulled A's segment from the remote");
    let ra2 = a.sync_with(&ta, true).await;
    assert!(ra2.errors.is_empty(), "{:?}", ra2.errors);
    assert_eq!(ra2.imported.len(), 1, "A pulled B's segment");
    assert_eq!(a.shared_search("synced drive", true).len(), 1);
    assert_eq!(b.shared_search("org node relays", true).len(), 1);
    // the remote holds both chains
    let cards = ta.chains(&Subscription::default()).await.unwrap();
    assert_eq!(cards.len(), 2, "{cards:?}");
}

#[test]
fn a_rewritten_segment_is_refused_as_forked_and_the_chain_is_marked() {
    let a = home(5, "fork-a", "alice", None);
    let b = home(6, "fork-b", "bob", None);
    a.remember("the first version of history");
    let env1 = a.export(None, false);
    let r = sharing::import(&b.store, &env1, &tofu()).unwrap();
    assert!(matches!(r.outcome, ImportOutcome::Appended { .. }));
    // A's history "rewritten": the same key + device name (same chain id) over a fresh store
    let a2 = Home {
        dir: tmpdir("fork-a2"),
        store: Arc::new(PolisStore::open(&tmpdir("fork-a2-db").join("polis.db")).unwrap()),
        identity: Identity::from_seed([5; 32], "fork-a"),
        handle: PolisHandle::new(Arc::new(PolisStore::open_in_memory().unwrap()), None, Arc::new(NoHost), Arc::new(NoopSink)),
        login: "alice",
    };
    adopt(&a2.store, &a2.identity, "alice").unwrap();
    let handle2 = PolisHandle::new(a2.store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink)).with_identity(Some(Arc::new(a2.identity.clone())));
    handle2.remember(&RememberRequest { text: "a different first version".into(), as_user: true, ..Default::default() }).unwrap();
    let env2 = envelope::build(&a2.store, &a2.identity, "alice", &BuildOptions::default()).unwrap();
    assert_eq!(env2.header.chain_id, env1.header.chain_id, "same chain id");
    let err = sharing::import(&b.store, &env2, &tofu()).unwrap_err();
    assert!(matches!(err, ImportError::Forked(_)), "{err}");
    let chain = b.store.get_foreign_chain(&env1.header.chain_id).unwrap().unwrap();
    assert!(chain.forked, "doctor sees the fork mark");
    assert!(chain.fork_detail.as_deref().unwrap_or("").contains("rewrote history"));
    // nothing newer from a forked chain lands
    let err2 = sharing::import(&b.store, &env2, &tofu()).unwrap_err();
    assert!(matches!(err2, ImportError::ForkedBefore(_)), "{err2}");
    drop(a2.dir);
}

#[test]
fn a_gap_is_refused_and_an_overlap_that_matches_is_a_no_op() {
    let a = home(7, "gap-a", "alice", None);
    let b = home(8, "gap-b", "bob", None);
    a.remember("one");
    let env_all = a.export(None, false);
    a.remember("two");
    a.remember("three");
    let head = a.store.chain_head().unwrap().0;
    let env_tail = a.export(Some(head), false);
    let err = sharing::import(&b.store, &env_tail, &tofu()).unwrap_err();
    assert!(matches!(err, ImportError::Gap(_)), "a first segment must start at 1: {err}");
    sharing::import(&b.store, &env_all, &tofu()).unwrap();
    let r = sharing::import(&b.store, &env_all, &tofu()).unwrap();
    assert!(matches!(r.outcome, ImportOutcome::NoOp), "{:?}", r.outcome);
    let err = sharing::import(&b.store, &env_tail, &tofu()).unwrap_err();
    assert!(matches!(err, ImportError::Gap(_)), "the middle is missing: {err}");
    let env_rest = a.export(Some(env_all.header.segment.to_seq + 1), false);
    let r = sharing::import(&b.store, &env_rest, &tofu()).unwrap();
    assert!(matches!(r.outcome, ImportOutcome::Appended { .. }));
    assert_eq!(b.store.get_foreign_chain(&env_all.header.chain_id).unwrap().unwrap().head_seq, head);
}

#[test]
fn a_foreign_vector_under_another_model_is_discarded_and_re_embedded_locally() {
    let m1: Arc<dyn Embedder> = Arc::new(Fake { model: "fake-m1" });
    let m2: Arc<dyn Embedder> = Arc::new(Fake { model: "fake-m2" });
    let a = home(9, "vec-a", "alice", Some(m1.clone()));
    a.remember("vectors travel only under the same model id");
    assert!(polis_embed::index_tick(&a.store, m1.as_ref(), 10) >= 1, "A embedded its prompt under m1");
    let env = a.export(None, true);
    assert!(!env.payload.vectors.is_empty(), "the export shipped vectors");

    // B runs another model: the vectors are discarded, the text re-embedded
    let b = home(10, "vec-b", "bob", Some(m2.clone()));
    let r = sharing::import(&b.store, &env, &ImportOptions { tofu: true, force: false, local_model: Some("fake-m2".into()) }).unwrap();
    assert_eq!(r.vectors_kept, 0);
    assert!(r.vectors_discarded >= 1, "{r:?}");
    assert!(!b.store.foreign_embedding_backlog("fake-m2", 10).unwrap().is_empty(), "queued for local re-embedding");
    assert!(polis_embed::index_tick(&b.store, m2.as_ref(), 10) >= 1, "the tick re-embedded the foreign text");
    assert!(b.store.foreign_embedding_backlog("fake-m2", 10).unwrap().is_empty());
    let hits = b.shared_search("vectors travel only under the same model id", true);
    assert_eq!(hits.len(), 1);
    assert!(hits[0].arms.iter().any(|h| h.arm == polis_core::pack::Arm::Semantic), "the semantic arm found it: {:?}", hits[0].arms);

    // C runs the same model: the vectors are kept as-is
    let c = home(11, "vec-c", "carol", Some(m1.clone()));
    let r = sharing::import(&c.store, &env, &ImportOptions { tofu: true, force: false, local_model: Some("fake-m1".into()) }).unwrap();
    assert!(r.vectors_kept >= 1, "{r:?}");
    assert_eq!(r.vectors_discarded, 0);
    assert!(c.store.foreign_embedding_backlog("fake-m1", 10).unwrap().is_empty(), "nothing left to embed");
}

#[tokio::test]
async fn a_redaction_tombstones_the_body_on_the_other_side_and_is_acknowledged_back() {
    let a = home(12, "red-a", "alice", None);
    let b = home(13, "red-b", "bob", None);
    let pid = a.remember("a secret alice will forget");
    a.remember("a fact alice keeps");
    let folder = FolderTransport::new(tmpdir("red-folder"));
    a.sync_with(&folder, true).await;
    let rb = b.sync_with(&folder, true).await;
    assert_eq!(rb.imported.len(), 1);
    assert_eq!(b.shared_search("secret alice", true).len(), 1, "B holds the body");

    // A forgets: the body is tombstoned locally and a redaction event is appended
    a.handle
        .forget(&ForgetRequest { target_kind: "prompt".into(), target_id: pid.to_string(), confirm: "forget".into(), scope: Scope::default() })
        .unwrap();
    let ours = a.store.own_redactions().unwrap();
    assert_eq!(ours.len(), 1, "{ours:?}");
    assert_eq!(ours[0].1, a.identity.device_id());

    // A publishes the redaction and, on the same sync, imports B's chain —
    // whose first segment acknowledged A only up to the seq B had seen then,
    // so doctor on A lists the redaction as not yet acknowledged by B.
    let ra = a.sync_with(&folder, true).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    assert_eq!(ra.imported.len(), 1, "A now holds B's chain");
    let unacked = sharing::unacknowledged_redactions(&a.store).unwrap();
    assert_eq!(unacked.len(), 1, "{unacked:?}");
    assert_eq!(unacked[0].peer_chain, b.identity.device_id());

    let rb = b.sync_with(&folder, true).await;
    assert!(rb.errors.is_empty(), "{:?}", rb.errors);
    assert_eq!(rb.imported.len(), 1);
    assert_eq!(rb.imported[0].rows.tombstoned, 1, "{:?}", rb.imported[0].rows);
    // the OR fallback still surfaces the other prompt for "alice"; the forgotten body itself is gone
    let after = b.shared_search("secret alice", true);
    assert!(after.iter().all(|h| !h.text.as_deref().unwrap_or("").contains("secret")), "the body is gone on B: {after:?}");
    let row = b.store.foreign_prompt(&a.identity.device_id(), ours[0].2).unwrap().unwrap();
    assert!(row.tombstoned);
    assert_eq!(row.text.as_deref(), Some("[forgotten]"));
    assert_eq!(b.shared_search("fact alice keeps", true).len(), 1, "the other body stays");

    // An ack is the peer's NEXT segment head past the redaction: B writes
    // something, publishes, and A's next import sees the acknowledgement.
    b.remember("bob carries on");
    let rb = b.sync_with(&folder, true).await;
    assert!(rb.published.is_some(), "{rb:?}");
    let ra = a.sync_with(&folder, true).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    let unacked = sharing::unacknowledged_redactions(&a.store).unwrap();
    assert!(unacked.is_empty(), "{unacked:?}");
}

#[test]
fn an_unknown_key_is_refused_without_tofu_and_a_disagreeing_trust_row_is_refused() {
    let a = home(14, "trust-a", "alice", None);
    let b = home(15, "trust-b", "bob", None);
    a.remember("hello from alice");
    let env = a.export(None, false);
    let err = sharing::import(&b.store, &env, &ImportOptions::default()).unwrap_err();
    assert!(matches!(err, ImportError::NotTrusted { .. }), "{err}");
    // an admin-distributed key that is NOT alice's, filed under alice's id (a hand-edited row)
    let wrong = Identity::from_seed([99; 32], "x");
    b.store
        .trust_set(&polis_store::foreign::TrustEntry { principal_id: a.identity.principal_id(), pubkey: wrong.pubkey_hex(), fingerprint: "bad".into(), source: "admin".into(), display_name: None, added_at: 1 })
        .unwrap();
    let err = sharing::import(&b.store, &env, &tofu()).unwrap_err();
    assert!(matches!(err, ImportError::KeyChanged { .. }), "{err}");
    b.store.trust_remove(&a.identity.principal_id()).unwrap();
    // a new key claiming alice's display name is a NEW principal, never merged into hers
    let r = sharing::import(&b.store, &env, &tofu()).unwrap();
    assert!(r.trusted_now.is_some());
    let impostor = home(16, "trust-a", "alice", None);
    impostor.remember("hello from someone else");
    let env2 = impostor.export(None, false);
    assert_ne!(env2.header.principal.human.principal_id, env.header.principal.human.principal_id);
    let err = sharing::import(&b.store, &env2, &ImportOptions::default()).unwrap_err();
    assert!(matches!(err, ImportError::NotTrusted { .. }), "{err}");
    // and a tampered card (alice's id with the impostor's key) never verifies
    let mut forged = env.clone();
    forged.header.principal.human.pubkey = Some(impostor.identity.pubkey_hex());
    let err = sharing::import(&b.store, &forged, &tofu()).unwrap_err();
    assert!(matches!(err, ImportError::Verify(envelope::VerifyError::IdMismatch)), "{err}");
}

#[test]
fn subscriptions_select_chains_and_a_project_filter_stubs_the_rest() {
    let a = home(17, "sub-a", "alice", None);
    let b = home(18, "sub-b", "bob", None);
    a.remember("alice on the redline project");
    {
        let conn = a.store.conn();
        conn.execute("UPDATE prompts SET project_path = '/repos/redline' WHERE body LIKE 'alice on the redline%'", []).unwrap();
    }
    a.remember("alice on something else");
    let env = a.export(None, false);
    b.store.subscribe(Some("carol"), None, None).unwrap();
    let err = sharing::import(&b.store, &env, &tofu()).unwrap_err();
    assert!(matches!(err, ImportError::NotSubscribed { .. }), "{err}");
    let r = sharing::import(&b.store, &env, &ImportOptions { tofu: true, force: true, local_model: None }).unwrap();
    assert!(matches!(r.outcome, ImportOutcome::Appended { .. }));
    b.store.forget_foreign_chain(&a.identity.device_id()).unwrap();
    // subscribe to alice by fingerprint prefix, restricted to one project: the other body is a stub
    let fp = polis_core::identity::fingerprint(&a.identity.principal_id());
    b.store.subscribe(Some(&fp[..8]), Some("/repos/redline"), None).unwrap();
    let r = sharing::import(&b.store, &env, &tofu()).unwrap();
    assert!(matches!(r.outcome, ImportOutcome::Appended { .. }));
    let rows = b.store.list_foreign_prompts(&a.identity.device_id(), 10).unwrap();
    let kept = rows.iter().find(|r| r.project.as_deref() == Some("/repos/redline")).unwrap();
    assert_eq!(kept.redaction, "full");
    let stubbed = rows.iter().find(|r| r.project.is_none()).unwrap();
    assert_eq!(stubbed.redaction, "stub");
    assert!(stubbed.text.is_none());
}

#[test]
fn importing_our_own_chain_verifies_continuity_and_stores_nothing() {
    let a = home(19, "own-a", "alice", None);
    a.remember("mine");
    let env = a.export(None, false);
    let r = sharing::import(&a.store, &env, &tofu()).unwrap();
    assert!(matches!(r.outcome, ImportOutcome::OwnChain));
    assert!(a.store.list_foreign_chains().unwrap().is_empty());
    let (chains, _, events, prompts, _) = a.store.foreign_counts().unwrap();
    assert_eq!((chains, events, prompts), (0, 0, 0));
}

#[test]
fn the_export_ships_the_policy_defaults_and_a_dry_run_names_every_decision() {
    let a = home(20, "pol-a", "alice", None);
    a.remember("org visible");
    a.handle.remember(&RememberRequest { text: "private by default".into(), as_user: true, ..Default::default() }).unwrap();
    let opts = BuildOptions { from_seq: None, policy: Policy::default(), org_id: None, include_vectors: false };
    let d = envelope::decisions(&a.store, &opts).unwrap();
    assert!(d.iter().any(|x| x.redaction == envelope::Redaction::Full && x.visibility == "org"));
    assert!(d.iter().any(|x| x.redaction == envelope::Redaction::Stub && x.visibility == "private"));
    let p = Policy::default();
    assert_eq!(p.roles, vec!["user".to_string()]);
    assert_eq!(p.bodies, Bodies::Auto);
    assert!(!p.tree && !p.browse);
}

