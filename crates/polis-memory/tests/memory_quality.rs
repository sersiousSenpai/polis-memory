// SPDX-License-Identifier: Apache-2.0
use polis_core::{
    api::{ContextRequest, EvidenceFilter, IngestItem, IngestRequest, SearchRequest},
    diagnostics::{DecisionRequest, EvidenceRequest, TraceRequest},
    host::NoHost,
    pack::{Arm, MAX_CONTEXT_BYTES},
    MemoryApi,
};
use polis_llm::NoopSink;
use polis_memory::{retrieval::build_answer_pack, Polis, PolisHandle};
use polis_store::PolisStore;
use std::sync::Arc;
fn handle(store: Arc<PolisStore>) -> PolisHandle {
    PolisHandle::new(store, None, Arc::new(NoHost), Arc::new(NoopSink))
}
fn ingest(api: &PolisHandle, body: String, role: &str, ts: i64) -> i64 {
    api.ingest(&IngestRequest {
        items: vec![IngestItem {
            body,
            role: Some(role.into()),
            ts: Some(ts),
            session: Some("session-a".into()),
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap()
    .recorded[0]
}

#[test]
fn dense_class_reaches_every_insertion_position_before_limiting() {
    for size in [40, 100, 317, 407, 1000] {
        let store = Arc::new(PolisStore::open_in_memory().unwrap());
        let api = handle(store.clone());
        store
            .seed_class_roots(&[("dense".into(), "Dense evidence".into(), None)])
            .unwrap();
        let mut seqs = Vec::new();
        for index in 0..size {
            let seq = ingest(
                &api,
                format!("unique{index:04} selects a distinct configuration at item {index}"),
                "user",
                1000 + index as i64,
            );
            store.conn().execute("INSERT INTO class_links(node_id,target_kind,target_id,status,created_at) VALUES('dense','prompt',?1,'accepted',1)",[seq.to_string()]).unwrap();
            seqs.push(seq);
        }
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        for (index, seq) in seqs.into_iter().enumerate() {
            let pack =
                build_answer_pack(&polis, Some(&format!("unique{index:04}")), Some("dense"), 1);
            assert!(
                pack.retrieval.errors.is_empty(),
                "{:?}",
                pack.retrieval.errors
            );
            assert_eq!(
                pack.node.as_ref().unwrap().links[0].link.target_id,
                seq.to_string(),
                "size {size}, insertion {index}"
            );
            assert!(serde_json::to_vec(&pack).unwrap().len() <= MAX_CONTEXT_BYTES);
        }
    }
}

#[test]
fn standalone_decision_has_direct_evidence_and_legacy_gap_is_explicit() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store.clone());
    let source = ingest(
        &api,
        "Use Helios as the selected build cache.".into(),
        "user",
        100,
    );
    let decision = api
        .decide(&DecisionRequest {
            source_seq: source,
            ..Default::default()
        })
        .unwrap()
        .seq
        .unwrap();
    assert_eq!(
        api.decide(&DecisionRequest {
            source_seq: source,
            ..Default::default()
        })
        .unwrap()
        .seq,
        Some(decision)
    );
    let pack = api
        .search(&SearchRequest {
            q: Some("Helios".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(pack
        .prompt_hits
        .iter()
        .any(|h| h.item.seq == decision && h.item.kind == "decision"));
    let evidence = api
        .evidence(&EvidenceRequest {
            seq: decision,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(evidence.status, "available");
    assert!(evidence.item.unwrap().body.unwrap().contains("Helios"));
    store
        .conn()
        .execute("DELETE FROM decision_evidence", [])
        .unwrap();
    store.run_migrations().unwrap();
    assert_eq!(
        api.evidence(&EvidenceRequest {
            seq: decision,
            ..Default::default()
        })
        .unwrap()
        .status,
        "available"
    );
    assert!(api.verify().unwrap().ok);
}

#[test]
fn small_context_budgets_and_assistant_attribution_survive_rendering() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store);
    ingest(
        &api,
        "Assistant-only Zorblax answer stored in the second session.".into(),
        "assistant",
        101,
    );
    for max in [0, 1, 16, 64, 256, 1200] {
        let block = api
            .context(&ContextRequest {
                q: "Zorblax".into(),
                max_tokens: Some(max),
                filter: EvidenceFilter {
                    roles: vec!["assistant".into()],
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert!(block.text.as_ref().is_none_or(|s| s.len() <= max));
        if max == 1200 {
            assert!(block
                .text
                .unwrap()
                .contains("[assistant; session session-a; ts 101]"));
        }
    }
}

struct Paraphrase;
impl polis_embed::Embedder for Paraphrase {
    fn model_id(&self) -> String {
        "fixture-paraphrase".into()
    }
    fn dim(&self) -> usize {
        2
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                if t.contains("railway") || t.contains("commuting") {
                    vec![1., 0.]
                } else {
                    vec![0., 1.]
                }
            })
            .collect())
    }
}
#[test]
fn semantic_only_evidence_interleaves_and_trace_has_no_source_text() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store.clone()).with_embedder(Some(Arc::new(Paraphrase)));
    for i in 0..10 {
        ingest(
            &api,
            format!("commuting literal noise {i}"),
            "user",
            100 + i,
        );
    }
    let target = ingest(
        &api,
        "railway is my preferred transit".into(),
        "assistant",
        200,
    );
    // Deliberately index just the paraphrase: lexical matches still exist.
    let pid = store
        .conn()
        .query_row(
            "SELECT prompt_id FROM ledger_events WHERE seq=?1",
            [target],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    let hash = store
        .conn()
        .query_row("SELECT body_hash FROM prompts WHERE id=?1", [pid], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    store
        .store_embeddings(
            "prompt",
            pid,
            "fixture-paraphrase",
            &hash,
            &[(
                polis_core::vec::Chunk {
                    ix: 0,
                    char_start: 0,
                    char_len: 32,
                    text: "railway is my preferred transit".into(),
                },
                polis_core::vec::quantize(&[1., 0.]),
            )],
        )
        .unwrap();
    let pack = api
        .search(&SearchRequest {
            q: Some("commuting".into()),
            limit: Some(3),
            ..Default::default()
        })
        .unwrap();
    assert!(
        pack.prompt_hits
            .iter()
            .any(|h| h.item.seq == target && h.arms.iter().any(|a| a.arm == Arm::Semantic)),
        "semantic-only hit was appended below the response cutoff"
    );
    let traces = api.traces(&TraceRequest::default()).unwrap();
    assert_eq!(traces.len(), 1);
    let raw = serde_json::to_string(&traces).unwrap();
    assert!(!raw.contains("railway"));
    assert!(!raw.contains("commuting"));
    assert_eq!(traces[0].head_seq, pack.head_seq);
    assert_eq!(traces[0].snapshot_hash, pack.retrieval.snapshot_hash);
}

#[test]
fn snapshot_is_stable_and_stats_do_not_bleed_between_stores() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store.clone());
    ingest(&api, "snapshot first evidence".into(), "user", 100);
    let snapshot = store.read_snapshot().unwrap();
    let head = snapshot.snapshot_head().unwrap();
    ingest(&api, "snapshot later evidence".into(), "assistant", 101);
    assert_eq!(snapshot.snapshot_head().unwrap(), head);
    assert_eq!(
        snapshot
            .search_prompts_ranked_scoped("later", 10, &Default::default())
            .unwrap()
            .len(),
        0
    );
    let other = handle(Arc::new(PolisStore::open_in_memory().unwrap()));
    assert_eq!(api.stats(&Default::default()).unwrap().total_prompts, 2);
    assert_eq!(other.stats(&Default::default()).unwrap().total_prompts, 0);
}

#[test]
fn inspection_cursor_is_scoped_and_does_not_skip_budgeted_items() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store);
    for i in 0..6 {
        ingest(
            &api,
            format!("inspecting number {i} with distinct codevalue{i}"),
            "user",
            100 + i,
        );
    }
    let req = SearchRequest {
        q: Some("inspecting".into()),
        limit: Some(2),
        ..Default::default()
    };
    let pack = api.search(&req).unwrap();
    let cursor = pack.retrieval.continuation.unwrap();
    let page = api
        .search(&SearchRequest {
            cursor: Some(cursor.clone()),
            ..req.clone()
        })
        .unwrap();
    assert_eq!(page.retrieval.version, "inspection-v1");
    assert_eq!(page.prompt_hits.len(), 2);
    let mut all = page
        .prompt_hits
        .iter()
        .map(|h| h.item.seq)
        .collect::<Vec<_>>();
    let mut cursor = page.retrieval.continuation;
    while let Some(c) = cursor {
        let p = api
            .search(&SearchRequest {
                cursor: Some(c),
                ..req.clone()
            })
            .unwrap();
        all.extend(p.prompt_hits.iter().map(|h| h.item.seq));
        cursor = p.retrieval.continuation;
    }
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 6);
    assert!(api
        .search(&SearchRequest {
            q: Some("changed query".into()),
            cursor: Some(cursor_for_test(&api, &req)),
            ..req
        })
        .is_err());
}
fn cursor_for_test(api: &PolisHandle, req: &SearchRequest) -> String {
    api.search(req).unwrap().retrieval.continuation.unwrap()
}

#[test]
fn byte_budget_covers_oversized_shared_notes_and_catalog_metadata() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store.clone());
    ingest(&api, "budgetprobe source evidence".into(), "user", 100);
    let mut pack = api
        .search(&SearchRequest {
            q: Some("budgetprobe".into()),
            ..Default::default()
        })
        .unwrap();
    pack.query = Some("large".repeat(100_000));
    pack.shared_hits.push(polis_core::pack::ForeignHit {
        chain_id: "chain".into(),
        source: "peer".into(),
        seq: 1,
        role: "assistant".into(),
        redaction: "full".into(),
        text: Some("body".repeat(100_000)),
        project: None,
        arms: Vec::new(),
        score: 1.0,
    });
    polis_core::pack::enforce_pack_budget(&mut pack);
    assert!(serde_json::to_vec(&pack).unwrap().len() <= MAX_CONTEXT_BYTES);
    assert!(pack.truncated.iter().any(|s| s == "sharedHits"));
    assert!(pack.truncated.iter().any(|s| s == "query"));
}

#[test]
fn assistant_reply_is_found_from_its_question_without_returning_excluded_roles() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store);
    ingest(
        &api,
        "What port should the standalone daemon use?".into(),
        "user",
        100,
    );
    let answer = ingest(
        &api,
        "Use 7677 by default; 7676 is taken by the host app.".into(),
        "assistant",
        101,
    );
    for roles in [Vec::new(), vec!["assistant".into()]] {
        let pack = api
            .search(&SearchRequest {
                q: Some("standalone daemon port".into()),
                filter: EvidenceFilter {
                    roles: roles.clone(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .unwrap();
        assert!(pack
            .prompt_hits
            .iter()
            .any(|h| h.item.seq == answer && h.stage == "neighbor"));
        if !roles.is_empty() {
            assert!(pack
                .prompt_hits
                .iter()
                .all(|h| h.item.role.as_deref() == Some("assistant")));
        }
    }
}

#[test]
fn command_flag_inside_question_remains_searchable() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store);
    let seq = ingest(
        &api,
        "Use cargo build --frozen for the release pipeline.".into(),
        "user",
        100,
    );
    let pack = api
        .search(&SearchRequest {
            q: Some("What did we decide about --frozen?".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(pack.prompt_hits.iter().any(|h| h.item.seq == seq));
    assert!(pack.grep_hits.iter().any(|h| h.seq == Some(seq)));
}

#[test]
fn capture_preserves_session_in_returned_evidence() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store);
    let seq = api
        .capture(&polis_core::api::CaptureRequest {
            body: "capture-session-evidence".into(),
            origin: polis_core::ledger::Origin::External,
            surface: "external".into(),
            session: Some("agent-session".into()),
            project: None,
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        api.evidence(&EvidenceRequest {
            seq,
            ..Default::default()
        })
        .unwrap()
        .item
        .unwrap()
        .session_id
        .as_deref(),
        Some("agent-session")
    );
}

#[test]
fn semantic_only_page_has_text_and_a_resolvable_citation() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store.clone()).with_embedder(Some(Arc::new(Paraphrase)));
    let seq = polis_store::record::record_browse_event_at(
        &store,
        polis_store::record::BrowseEventInput {
            action: polis_store::record::BrowseAction::Navigate,
            browse_id: Some("page-only".into()),
            url: "https://example.test/rail".into(),
            title: Some("Regional railway".into()),
            text: "railway is the preferred transit route".into(),
            from_event_id: None,
            author: None,
        },
        100,
    )
    .unwrap()
    .unwrap();
    let id = store
        .conn()
        .query_row("SELECT id FROM browse_events", [], |r| r.get::<_, i64>(0))
        .unwrap();
    let hash = store
        .conn()
        .query_row("SELECT context_hash FROM browse_events", [], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    store
        .store_embeddings(
            "browse_event",
            id,
            "fixture-paraphrase",
            &hash,
            &[(
                polis_core::vec::Chunk {
                    ix: 0,
                    char_start: 0,
                    char_len: 36,
                    text: "railway is the preferred transit route".into(),
                },
                polis_core::vec::quantize(&[1., 0.]),
            )],
        )
        .unwrap();
    let pack = api
        .search(&SearchRequest {
            q: Some("commuting".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(pack.prompt_hits.is_empty());
    assert_eq!(pack.browse_hits.len(), 1);
    assert_eq!(pack.browse_hits[0].seq, Some(seq));
    assert!(pack.browse_hits[0].snippet.contains("railway"));
    assert_eq!(
        api.evidence(&EvidenceRequest {
            seq,
            ..Default::default()
        })
        .unwrap()
        .status,
        "available"
    );
}

#[test]
fn scoped_results_do_not_expose_a_successor_from_another_project() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = handle(store.clone());
    let scope = polis_core::Scope {
        project: Some("/allowed".into()),
        ..Default::default()
    };
    store
        .seed_class_roots(&[(
            "allowed-class".into(),
            "Allowed".into(),
            scope.project.clone(),
        )])
        .unwrap();
    let mut seqs = Vec::new();
    for project in ["/allowed", "/private"] {
        let seq = api
            .ingest(&IngestRequest {
                items: vec![IngestItem {
                    body: format!("settings for {project}"),
                    role: Some("user".into()),
                    ..Default::default()
                }],
                scope: polis_core::Scope {
                    project: Some(project.into()),
                    ..Default::default()
                },
            })
            .unwrap()
            .recorded[0];
        seqs.push(seq);
    }
    store
        .conn()
        .execute(
            "INSERT INTO supersessions(old_seq,new_seq,event_seq,created_at) VALUES(?1,?2,?2,1)",
            [seqs[0], seqs[1]],
        )
        .unwrap();
    store.conn().execute("INSERT INTO class_links(node_id,target_kind,target_id,status,created_at) VALUES('allowed-class','prompt',?1,'accepted',1)",[seqs[0].to_string()]).unwrap();
    let pack = api
        .search(&SearchRequest {
            q: Some("settings".into()),
            node: Some("allowed-class".into()),
            scope: scope.clone(),
            ..Default::default()
        })
        .unwrap();
    assert!(pack.prompt_hits.iter().all(|h| h.superseded_by.is_none()));
    assert!(pack
        .node
        .unwrap()
        .links
        .iter()
        .all(|l| l.superseded_by.is_none()));
    assert!(api
        .node("allowed-class", &scope)
        .unwrap()
        .unwrap()
        .links
        .iter()
        .all(|l| l.superseded_by.is_none()));
}
