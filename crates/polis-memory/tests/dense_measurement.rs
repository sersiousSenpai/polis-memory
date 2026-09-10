use polis_core::{
    api::{IngestItem, IngestRequest},
    host::NoHost,
    MemoryApi,
};
use polis_llm::NoopSink;
use polis_memory::{retrieval::build_answer_pack, Polis, PolisHandle};
use polis_store::PolisStore;
use std::sync::Arc;
// Run explicitly to reproduce the before/after dense-link measurement.
#[test]
#[ignore]
fn measure_dense_link_reachability() {
    let mut results = Vec::new();
    for size in [40, 100, 317, 407, 1000] {
        let store = Arc::new(PolisStore::open_in_memory().unwrap());
        let api = PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink));
        store
            .seed_class_roots(&[("dense".into(), "Dense evidence".into(), None)])
            .unwrap();
        let mut seqs = Vec::new();
        for index in 0..size {
            let seq = api
                .ingest(&IngestRequest {
                    items: vec![IngestItem {
                        body: format!(
                            "unique{index:04} selects a distinct configuration at item {index}"
                        ),
                        role: Some("user".into()),
                        ts: Some(1000 + index as i64),
                        session: Some("session-a".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .unwrap()
                .recorded[0];
            store.conn().execute("INSERT INTO class_links(node_id,target_kind,target_id,status,created_at) VALUES('dense','prompt',?1,'accepted',1)",[seq.to_string()]).unwrap();
            seqs.push(seq);
        }
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let mut reachable = 0;
        let started = std::time::Instant::now();
        for (index, seq) in seqs.into_iter().enumerate() {
            let pack =
                build_answer_pack(&polis, Some(&format!("unique{index:04}")), Some("dense"), 1);
            if pack
                .node
                .as_ref()
                .is_some_and(|n| n.links.iter().any(|l| l.link.target_id == seq.to_string()))
            {
                reachable += 1;
            }
        }
        results.push(serde_json::json!({"links":size,"queries":size,"reachable":reachable,"elapsedMs":started.elapsed().as_millis()}));
    }
    println!("DENSE_RESULTS={}", serde_json::json!(results));
}
