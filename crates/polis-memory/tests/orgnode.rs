// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! E4's gate, in one process: three peers and an org node over a loopback
//! port. The node relays; every peer re-verifies; a forget on one peer
//! tombstones the body on the others after their next sync and the acks
//! come back to the emitter through the node; the node's own chain — its
//! catalog riding — verifies like any chain and lands as foreign catalog
//! rows; a rewritten segment is refused as forked; a network bind without
//! an operator's token is refused.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use polis_core::api::{ForgetRequest, MemoryApi, RememberRequest, Scope};
use polis_core::host::NoHost;
use polis_core::sync::SyncRelay;
use polis_llm::NoopSink;
use polis_memory::identity::{adopt, Identity};
use polis_memory::orgnode::OrgNode;
use polis_memory::sharing;
use polis_memory::sync::{sync, SyncOptions};
use polis_memory::transport::{OrgNodeTransport, SegmentTransport};
use polis_memory::{retrieval, PolisHandle};
use polis_server::standalone::{app, StandaloneAuth};
use polis_server::PolisState;
use polis_store::principals::ScopeFilter;
use polis_store::PolisStore;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("polis-e4-{}-{}-{label}", std::process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Peer {
    store: Arc<PolisStore>,
    identity: Identity,
    handle: PolisHandle,
    login: &'static str,
}

fn peer(seed: u8, device: &str, login: &'static str) -> Peer {
    let dir = tmpdir(device);
    let store = Arc::new(PolisStore::open(&dir.join("polis.db")).unwrap());
    let identity = Identity::from_seed([seed; 32], device);
    adopt(&store, &identity, login).unwrap();
    let handle = PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink)).with_identity(Some(Arc::new(identity.clone())));
    Peer { store, identity, handle, login }
}

impl Peer {
    fn remember(&self, text: &str) -> i64 {
        self.handle.remember(&RememberRequest { text: text.into(), as_user: true, ..Default::default() }).unwrap();
        let conn = self.store.conn();
        conn.execute("UPDATE prompts SET visibility = 'org' WHERE body = ?1", rusqlite::params![text]).unwrap();
        conn.query_row("SELECT id FROM prompts WHERE body = ?1", rusqlite::params![text], |r| r.get(0)).unwrap()
    }

    async fn sync_via(&self, t: &dyn SegmentTransport) -> polis_memory::sync::SyncReport {
        let opts = SyncOptions { tofu: true, ..Default::default() };
        sync(&self.store, &self.identity, self.login, t, &opts).await
    }

    fn shared(&self, q: &str) -> Vec<polis_core::pack::ForeignHit> {
        let scope = ScopeFilter { include_shared: true, ..Default::default() };
        retrieval::build_answer_pack_scoped(&self.handle.view(), Some(q), None, 20, &scope).shared_hits
    }
}

/// The org node, served on a random loopback port with a token.
struct Node {
    node: Arc<OrgNode>,
    base: String,
    token: String,
    _task: tokio::task::JoinHandle<()>,
}

async fn node(seed: u8, name: &'static str) -> Node {
    let dir = tmpdir("node");
    let store = Arc::new(PolisStore::open(&dir.join("polis.db")).unwrap());
    let identity = Identity::from_seed([seed; 32], format!("org:{name}"));
    adopt(&store, &identity, name).unwrap();
    let node = Arc::new(OrgNode::new(store.clone(), Arc::new(identity), name, dir.join("sync").join("org"), true));
    std::fs::create_dir_all(&node.segments_dir).unwrap();
    let handle = Arc::new(PolisHandle::new(store, None, Arc::new(NoHost), Arc::new(NoopSink)));
    let api: Arc<dyn MemoryApi> = handle;
    let state = PolisState::bare(api).with_sync(node.clone());
    let token = "org-secret".to_string();
    let router = app(state, StandaloneAuth { token: Some(token.clone()) });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Node { node, base, token, _task: task }
}

fn transport(n: &Node) -> OrgNodeTransport {
    OrgNodeTransport::new(n.base.clone(), Some(n.token.clone()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_peers_relay_through_the_node_and_a_forget_propagates_with_acks() {
    let n = node(9, "acme").await;
    let a = peer(1, "alpha", "ann");
    let b = peer(2, "bravo", "bob");
    let c = peer(3, "charlie", "cy");
    let pid = a.remember("alpha decided the org node relays envelopes over https");
    b.remember("bravo prefers folder sync for the design team");
    c.remember("charlie keeps the git remote as the audit trail");
    let (ta, tb, tc) = (transport(&n), transport(&n), transport(&n));

    // Round 1: everyone publishes; A has nothing to fetch yet, B sees A, C sees A and B.
    let ra = a.sync_via(&ta).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    assert!(ra.published.is_some());
    let rb = b.sync_via(&tb).await;
    assert!(rb.errors.is_empty(), "{:?}", rb.errors);
    let rc = c.sync_via(&tc).await;
    assert!(rc.errors.is_empty(), "{:?}", rc.errors);
    // Round 2: A and B fetch what landed after them.
    a.sync_via(&ta).await;
    b.sync_via(&tb).await;
    for (p, others) in [(&a, ["bravo prefers", "charlie keeps"]), (&b, ["alpha decided", "charlie keeps"]), (&c, ["alpha decided", "bravo prefers"])] {
        let held = p.store.list_foreign_chains().unwrap();
        // Two peers plus the node's own chain (its bind; its catalog rides once it has one).
        assert!(held.len() >= 2, "{} holds {} chains", p.login, held.len());
        for q in others {
            let hits = p.shared(q);
            assert_eq!(hits.len(), 1, "{} looking for {q:?}: {hits:?}", p.login);
            assert!(hits[0].source.starts_with("shared:"), "labelled: {}", hits[0].source);
        }
        // Never as the user's own words: the local pack has no such prompt.
        let own = retrieval::build_answer_pack_scoped(&p.handle.view(), Some(others[0]), None, 20, &ScopeFilter::default());
        assert!(own.prompt_hits.is_empty(), "{}: a shared body leaked into prompt_hits", p.login);
        assert!(own.shared_hits.is_empty(), "without include_shared the shared arm is absent");
    }
    // The node holds every chain, verified on receipt, and says so.
    let chains = n.node.chains().unwrap();
    assert!(chains.len() >= 3, "{chains:?}");
    assert!(chains.iter().all(|c| !c.forked));

    // A forgets → its redaction event → the node relays → B and C tombstone on their next sync.
    a.handle.forget(&ForgetRequest { target_kind: "prompt".into(), target_id: pid.to_string(), confirm: "forget".into(), scope: Scope::default() }).unwrap();
    assert_eq!(a.store.own_redactions().unwrap().len(), 1);
    let ra = a.sync_via(&ta).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    let pending_before = sharing::unacknowledged_redactions(&a.store).unwrap();
    assert!(!pending_before.is_empty(), "no peer has acknowledged yet");
    let rb = b.sync_via(&tb).await;
    assert!(rb.errors.is_empty(), "{:?}", rb.errors);
    let rc = c.sync_via(&tc).await;
    assert!(rc.errors.is_empty(), "{:?}", rc.errors);
    for p in [&b, &c] {
        let row = p.store.foreign_prompt(&a.identity.device_id(), 2).unwrap().or_else(|| p.store.foreign_prompt(&a.identity.device_id(), 1).unwrap());
        let tomb = p.store.list_foreign_prompts(&a.identity.device_id(), 10).unwrap().iter().any(|r| r.tombstoned);
        assert!(tomb, "{}: A's body is tombstoned after the sync ({row:?})", p.login);
        assert!(p.shared("alpha decided").is_empty(), "{}: a tombstoned body is not served", p.login);
    }
    // The node saw the redaction and knows who moved past it: B and C published
    // (their acks carry A's head) — the relay lists them as acked.
    let red = n.node.redactions().unwrap();
    assert_eq!(red.len(), 1, "{red:?}");
    assert!(red[0].pending.is_empty(), "every subscriber moved past it: {red:?}");
    assert_eq!(red[0].acked_by.len(), 2, "{red:?}");
    // …and A learns of the acks through the node on its next sync, even for a
    // peer whose chain it might not hold.
    let ra = a.sync_via(&ta).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    assert!(ra.acks_relayed >= 2, "acks relayed: {}", ra.acks_relayed);
    let pending_after = sharing::unacknowledged_redactions(&a.store).unwrap();
    assert!(pending_after.is_empty(), "the redaction is acknowledged by every peer: {pending_after:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_nodes_own_catalog_rides_its_chain_and_lands_as_foreign_catalog_rows() {
    let n = node(19, "acme").await;
    let a = peer(11, "alpha", "ann");
    let ta = transport(&n);
    // The node's catalog: a class over a captured item on its own chain.
    let node_store = n.node.store.clone();
    let handle = PolisHandle::new(node_store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink)).with_identity(Some(n.node.identity.clone()));
    handle.remember(&RememberRequest { text: "the firm standardizes on trigram grep".into(), as_user: true, ..Default::default() }).unwrap();
    let seq: i64 = node_store.conn().query_row("SELECT MAX(seq) FROM ledger_events WHERE kind = 'prompt'", [], |r| r.get(0)).unwrap();
    {
        let conn = node_store.conn();
        let now = polis_core::ledger::now_millis();
        conn.execute("INSERT INTO class_nodes (id, parent_id, kind, title, status, pinned, created_at, updated_at) VALUES ('root-firm', NULL, 'node', 'firm', 'accepted', 0, ?1, ?1)", rusqlite::params![now]).unwrap();
        conn.execute("INSERT INTO class_nodes (id, parent_id, kind, title, status, pinned, created_at, updated_at) VALUES ('cn-search', 'root-firm', 'node', 'Search tooling', 'accepted', 0, ?1, ?1)", rusqlite::params![now]).unwrap();
        conn.execute("INSERT INTO class_links (node_id, target_kind, target_id, status, created_at) VALUES ('cn-search', 'prompt', ?1, 'accepted', ?2)", rusqlite::params![seq.to_string(), now]).unwrap();
    }
    let published = n.node.publish_own().unwrap().expect("the node's chain has events to publish");
    assert!(published.to_seq >= seq);
    assert!(n.node.publish_own().unwrap().is_none(), "nothing new: no second segment");

    // A syncs: the node's chain verifies like any chain and its catalog lands beside it.
    let ra = a.sync_via(&ta).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    let node_chain = n.node.identity.device_id();
    let imported = ra.imported.iter().find(|r| r.chain_id == node_chain).expect("the node's chain imported");
    assert_eq!(imported.tree, (2, 1), "two class nodes and one link rode the segment");
    let nodes = a.store.list_foreign_class_nodes(&node_chain).unwrap();
    assert_eq!(nodes.iter().map(|n| n.title.as_str()).collect::<Vec<_>>(), ["Search tooling", "firm"]);
    let links = a.store.list_foreign_class_links(&node_chain, "cn-search").unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(a.store.foreign_class_titles_for(&node_chain, seq).unwrap(), vec![(node_chain.clone(), "Search tooling".to_string())]);
    // The node's card names the org, not a login.
    let card = n.node.node().unwrap();
    assert_eq!(card.display_name.as_deref(), Some("acme"));
    assert_eq!(a.store.get_foreign_chain(&node_chain).unwrap().unwrap().display_name.as_deref(), Some("acme"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rewritten_segment_is_refused_as_forked_and_the_node_records_it() {
    let n = node(29, "acme").await;
    let a = peer(21, "alpha", "ann");
    let ta = transport(&n);
    a.remember("first history");
    let ra = a.sync_via(&ta).await;
    assert!(ra.errors.is_empty(), "{:?}", ra.errors);
    // The same key and device over a fresh store: seq 1.. with different bytes.
    let dir = tmpdir("alpha-rewritten");
    let store2 = Arc::new(PolisStore::open(&dir.join("polis.db")).unwrap());
    let identity2 = Identity::from_seed([21; 32], "alpha");
    adopt(&store2, &identity2, "ann").unwrap();
    // The rewritten chain's segment 1..N carries different hashes at seqs the node holds.
    let env = polis_memory::envelope::build(&store2, &identity2, "ann", &polis_memory::envelope::BuildOptions { from_seq: Some(1), policy: Default::default(), org_id: None, include_vectors: false }).unwrap();
    let err = ta.publish(&env).await.unwrap_err();
    assert!(err.to_string().contains("forked"), "{err}");
    let chains = n.node.chains().unwrap();
    let mine = chains.iter().find(|c| c.chain_id == a.identity.device_id()).unwrap();
    assert!(mine.forked, "the node marked the chain: {chains:?}");
    // The honest peer's later segments are refused too, until an operator resets the mark.
    a.remember("second history");
    let ra = a.sync_via(&ta).await;
    assert!(ra.errors.iter().any(|(_, e)| e.contains("forked")), "{:?}", ra.errors);
}

#[test]
fn an_org_node_on_a_network_needs_an_operators_token_file() {
    use polis_memory::cli::serve::check_org_bind;
    let net: std::net::SocketAddr = "0.0.0.0:7677".parse().unwrap();
    let local: std::net::SocketAddr = "127.0.0.1:7677".parse().unwrap();
    assert!(check_org_bind(true, &net, None).is_err());
    assert!(check_org_bind(true, &net, Some(std::path::Path::new("/tmp/t"))).is_ok());
    assert!(check_org_bind(true, &local, None).is_ok());
    assert!(check_org_bind(false, &net, None).is_ok(), "the plain daemon's rule is check_bind's");
}
