// SPDX-License-Identifier: Apache-2.0
use polis_core::{api::*, host::NoHost, MemoryApi};
use polis_llm::NoopSink;
use polis_memory::PolisHandle;
use polis_store::PolisStore;
use std::sync::Arc;

fn handle() -> PolisHandle {
    PolisHandle::new(
        Arc::new(PolisStore::open_in_memory().unwrap()),
        None,
        Arc::new(NoHost),
        Arc::new(NoopSink),
    )
}
fn scope(project: &str) -> Scope {
    Scope {
        project: Some(project.into()),
        principal: Some("human-a".into()),
        agent: Some("agent-a".into()),
        run: Some("run-a".into()),
        org: Some("org-a".into()),
        ..Default::default()
    }
}
fn ingest(h: &PolisHandle, project: &str, role: &str, body: &str, ts: i64) -> i64 {
    h.ingest(&IngestRequest {
        scope: scope(project),
        items: vec![IngestItem {
            body: body.into(),
            role: Some(role.into()),
            ts: Some(ts),
            ..Default::default()
        }],
    })
    .unwrap()
    .recorded[0]
}

#[test]
fn eligibility_precedes_top_k_for_every_scope_axis_and_preserves_roles() {
    let h = handle();
    let allowed = ingest(
        &h,
        "/allowed",
        "assistant",
        "the quartz identifier is correct",
        10,
    );
    for n in 0..40 {
        ingest(
            &h,
            "/other",
            "user",
            &format!("quartz quartz secret {n}"),
            11 + n,
        );
    }
    let scoped = scope("/allowed");
    let pack = h
        .search(&SearchRequest {
            q: Some("quartz".into()),
            limit: Some(1),
            scope: scoped.clone(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(pack.prompt_hits.len(), 1);
    assert_eq!(pack.prompt_hits[0].item.seq, allowed);
    assert_eq!(pack.prompt_hits[0].item.role.as_deref(), Some("assistant"));
    let grep = h
        .grep(&GrepRequest {
            literal: "quartz".into(),
            scope: scoped.clone(),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(grep[0].seq, Some(allowed));
    let delta = h
        .prompts(&PromptsRequest {
            scope: scoped.clone(),
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(delta[0].seq, allowed);
    let timeline = h.timeline(&Default::default(), &scoped).unwrap();
    assert_eq!(timeline.len(), 1);
    assert_eq!(timeline[0].event.seq, allowed);
    assert_eq!(h.stats(&scoped).unwrap().total_prompts, 1);
    let wrong = [
        Scope {
            principal: Some("human-b".into()),
            ..scoped.clone()
        },
        Scope {
            agent: Some("agent-b".into()),
            ..scoped.clone()
        },
        Scope {
            run: Some("run-b".into()),
            ..scoped.clone()
        },
        Scope {
            org: Some("org-b".into()),
            ..scoped.clone()
        },
    ];
    for scope in wrong {
        assert!(h
            .search(&SearchRequest {
                q: Some("quartz".into()),
                scope,
                ..Default::default()
            })
            .unwrap()
            .prompt_hits
            .is_empty());
    }
}

#[test]
fn roles_are_validated_before_any_partial_batch_and_role_aliases_dedupe_separately() {
    let h = handle();
    let result = h.ingest(&IngestRequest {
        items: vec![
            IngestItem {
                body: "would have been recorded".into(),
                ..Default::default()
            },
            IngestItem {
                body: "unknown".into(),
                role: Some("guest".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    assert!(result.is_err());
    assert_eq!(h.store.max_ledger_seq().unwrap(), 0);
    let a = ingest(&h, "/allowed", "human", "same words", 1);
    let b = ingest(&h, "/allowed", "ai", "same words", 2);
    assert_ne!(a, b);
    let user = h
        .search(&SearchRequest {
            q: Some("same words".into()),
            scope: scope("/allowed"),
            filter: EvidenceFilter {
                roles: vec!["human".into()],
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert_eq!(user.prompt_hits.len(), 1);
    assert_eq!(user.prompt_hits[0].item.role.as_deref(), Some("user"));
    assert!(h.verify().unwrap().ok);
}

#[test]
fn exact_lookup_and_time_filters_do_not_cross_namespaces_or_inclusive_end_boundary() {
    let h = handle();
    let seq = ingest(&h, "/allowed", "assistant", "a dated quartz answer", 10);
    let found = h.evidence(&polis_core::diagnostics::EvidenceRequest {
        seq,
        scope: scope("/other"),
        ..Default::default()
    });
    assert!(found.unwrap().item.is_none());
    let pack = h
        .search(&SearchRequest {
            q: Some("quartz".into()),
            scope: scope("/allowed"),
            filter: EvidenceFilter {
                before: Some(10),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    assert!(pack.prompt_hits.is_empty());
}

struct Vectors;
impl polis_embed::Embedder for Vectors {
    fn model_id(&self) -> String {
        "test/scoped".into()
    }
    fn dim(&self) -> usize {
        2
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|text| {
                if text.contains("distant") {
                    vec![0.0, 1.0]
                } else {
                    vec![1.0, 0.0]
                }
            })
            .collect())
    }
}

#[test]
fn semantic_top_k_respects_project_before_scoring_and_cache_is_store_specific() {
    let h = handle();
    let seq = ingest(&h, "/allowed", "assistant", "distant eligible passage", 10);
    for n in 0..30 {
        ingest(
            &h,
            "/other",
            "assistant",
            &format!("closest forbidden passage {n}"),
            n + 20,
        );
    }
    assert!(polis_embed::index_tick(&h.store, &Vectors, 100) > 0);
    let filter = polis_store::principals::ScopeFilter::from_scope(&scope("/allowed"));
    let hits =
        polis_embed::semantic_search_scoped_checked(&h.store, &Vectors, "closest", 1, &filter)
            .unwrap()
            .unwrap();
    assert_eq!(hits.len(), 1);
    let seqs = h.store.seqs_for_prompt_ids(&[hits[0].target_id]).unwrap();
    assert_eq!(seqs[&hits[0].target_id], seq);
    assert!(hits[0].score < 0.1);
    let second = handle();
    ingest(
        &second,
        "/allowed",
        "assistant",
        "closest eligible passage",
        10,
    );
    polis_embed::index_tick(&second.store, &Vectors, 100);
    let hits =
        polis_embed::semantic_search_scoped_checked(&second.store, &Vectors, "closest", 1, &filter)
            .unwrap()
            .unwrap();
    assert!(hits[0].score > 0.99);
    assert_ne!(h.store.cache_identity(), second.store.cache_identity());
}

#[test]
fn vector_cache_tracks_same_head_edits_and_each_read_snapshot() {
    let h = handle();
    let first = ingest(&h, "/allowed", "assistant", "closest eligible passage", 1);
    ingest(&h, "/allowed", "assistant", "distant eligible passage", 2);
    assert_eq!(polis_embed::index_tick(&h.store, &Vectors, 100), 2);
    let filter = polis_store::principals::ScopeFilter::from_scope(&scope("/allowed"));
    let before = h.store.read_snapshot().unwrap();
    let search = |store: &PolisStore| {
        polis_embed::semantic_search_scoped_checked(store, &Vectors, "closest", 10, &filter)
            .unwrap()
            .unwrap()
    };
    let initial = search(&before);
    assert!(initial[0].score > 0.99);
    let first_id = h
        .store
        .conn()
        .query_row(
            "SELECT prompt_id FROM ledger_events WHERE seq=?1",
            [first],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    let index_head = h.store.max_embedding_id().unwrap();
    let q = polis_core::vec::quantize(&[0.0, 1.0]);
    h.store
        .conn()
        .execute(
            "UPDATE embeddings SET vec=?1,scale=?2 WHERE target_kind='prompt' AND target_id=?3",
            rusqlite::params![polis_core::vec::pack(&q), q.scale, first_id],
        )
        .unwrap();
    assert_eq!(h.store.max_embedding_id().unwrap(), index_head);
    let updated = h.store.read_snapshot().unwrap();
    assert!(updated.embedding_revision().unwrap() > before.embedding_revision().unwrap());
    assert!(search(&updated).iter().all(|hit| hit.score < 0.1));
    assert!(
        search(&before)[0].score > 0.99,
        "an old read snapshot must retain its old vectors"
    );
    h.store
        .conn()
        .execute(
            "DELETE FROM embeddings WHERE target_kind='prompt' AND target_id=?1",
            [first_id],
        )
        .unwrap();
    assert_eq!(
        h.store.max_embedding_id().unwrap(),
        index_head,
        "delete did not remove the high-water row"
    );
    let deleted = h.store.read_snapshot().unwrap();
    assert_eq!(search(&deleted).len(), 1);
    assert_eq!(search(&updated).len(), 2);
}

#[test]
fn notes_pages_and_inherited_catalog_projects_are_scoped() {
    let h = handle();
    for project in ["/allowed", "/other"] {
        h.annotate(&AnnotateRequest {
            target_kind: "none".into(),
            text: format!("quartz annotation {project}"),
            scope: scope(project),
            ..Default::default()
        })
        .unwrap();
        h.browse(&BrowseRequest {
            url: format!("https://example.invalid{project}"),
            title: Some("quartz page".into()),
            text: format!("quartz contents {project}"),
            scope: scope(project),
            ..Default::default()
        })
        .unwrap();
    }
    let filter = polis_store::principals::ScopeFilter::from_scope(&scope("/allowed"));
    let notes = h
        .store
        .search_user_notes_scoped("quartz", 1, &filter)
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].text.ends_with("/allowed"));
    let pages = h.browse_search("quartz", 1, &scope("/allowed")).unwrap();
    assert_eq!(pages.len(), 1);
    assert!(pages[0].url.ends_with("/allowed"));
    let conn = h.store.conn();
    conn.execute("INSERT INTO class_nodes(id,kind,title,project_path,status,created_at,updated_at) VALUES('root','class','Root','/allowed','accepted',1,1)", []).unwrap();
    conn.execute("INSERT INTO class_nodes(id,parent_id,kind,title,status,created_at,updated_at) VALUES('child','root','class','Child','accepted',1,1)", []).unwrap();
    drop(conn);
    let filter = polis_store::principals::ScopeFilter {
        project: Some("/allowed".into()),
        ..Default::default()
    };
    assert!(h
        .store
        .get_class_node_scoped("child", &filter)
        .unwrap()
        .is_some());
}

#[test]
fn scoped_threads_map_and_run_metadata_only_refer_to_eligible_sources() {
    let h = handle();
    for (session, project) in [("visible", "/allowed"), ("secret", "/other")] {
        h.ingest(&IngestRequest {
            scope: scope(project),
            items: vec![IngestItem {
                body: format!("session body {session}"),
                session: Some(session.into()),
                ..Default::default()
            }],
        })
        .unwrap();
    }
    h.store
        .insert_session_link("session", "visible", "session", "secret", 1)
        .unwrap();
    let visible = h
        .thread("session", "visible", 20, &scope("/allowed"))
        .unwrap()
        .unwrap();
    assert_eq!(visible["messages"].as_array().unwrap().len(), 1);
    assert!(h
        .thread("session", "secret", 20, &scope("/allowed"))
        .unwrap()
        .is_none());
    let tree = h
        .thread_tree("session", "visible", &scope("/allowed"))
        .unwrap();
    assert!(tree["parent"].is_null());
    let map = h.map(&scope("/allowed")).unwrap();
    assert!(map.nodes.iter().any(|n| n.id == "thread:session:visible"));
    assert!(!serde_json::to_string(&map).unwrap().contains("secret"));
    let run = h.store.insert_class_run(0, 2).unwrap();
    h.store
        .conn()
        .execute(
            "UPDATE class_runs SET summary='secret namespace narrative' WHERE id=?1",
            [run],
        )
        .unwrap();
    let runs = h.list_runs(20, &scope("/allowed")).unwrap();
    assert_eq!(runs.len(), 1);
    assert!(!serde_json::to_string(&runs)
        .unwrap()
        .contains("secret namespace"));
}

#[test]
fn identical_capture_is_idempotent_only_inside_its_complete_namespace() {
    let h = handle();
    let base = scope("/allowed");
    let scopes = [
        base.clone(),
        Scope {
            project: Some("/other".into()),
            ..base.clone()
        },
        Scope {
            principal: Some("other-human".into()),
            ..base.clone()
        },
        Scope {
            agent: Some("other-agent".into()),
            ..base.clone()
        },
        Scope {
            org: Some("other-org".into()),
            ..base.clone()
        },
    ];
    for scope in scopes {
        let req = IngestRequest {
            scope,
            items: vec![IngestItem {
                body: "identical captured statement".into(),
                role: Some("assistant".into()),
                ..Default::default()
            }],
        };
        assert_eq!(h.ingest(&req).unwrap().recorded.len(), 1);
        assert_eq!(h.ingest(&req).unwrap().skipped, 1);
    }
    assert_eq!(h.store.max_ledger_seq().unwrap(), 5);
    assert!(h.verify().unwrap().ok);
    let req = IngestRequest {
        scope: base,
        items: vec![IngestItem {
            body: "must not be captured".into(),
            project: Some("/conflicting".into()),
            ..Default::default()
        }],
    };
    assert!(h.ingest(&req).is_err());
    assert_eq!(h.store.max_ledger_seq().unwrap(), 5);
}

#[test]
fn ledger_failure_rolls_back_source_and_lexical_indexes_atomically() {
    let h = handle();
    h.store.conn().execute_batch("CREATE TRIGGER injected_ledger_failure BEFORE INSERT ON ledger_events BEGIN SELECT RAISE(ABORT,'injected ledger failure'); END;").unwrap();
    let request = IngestRequest {
        scope: scope("/allowed"),
        items: vec![IngestItem {
            body: "atomic rollback sentinel".into(),
            ..Default::default()
        }],
    };
    assert!(h.ingest(&request).is_err());
    let conn = h.store.conn();
    let prompts: i64 = conn
        .query_row("SELECT COUNT(*) FROM prompts", [], |r| r.get(0))
        .unwrap();
    let lexical: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompts_fts WHERE prompts_fts MATCH 'sentinel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((prompts, lexical), (0, 0));
    conn.execute_batch("DROP TRIGGER injected_ledger_failure")
        .unwrap();
    drop(conn);
    assert_eq!(h.ingest(&request).unwrap().recorded.len(), 1);
    assert!(h.verify().unwrap().ok);
}

#[test]
fn identity_adoption_preserves_historical_duplicates_and_future_deduplication() {
    use polis_core::identity::{Principal, PrincipalKind};
    let h = handle();
    let request = IngestRequest {
        scope: Scope {
            run: Some("shared-run".into()),
            project: Some("/same".into()),
            ..Default::default()
        },
        items: vec![IngestItem {
            body: "historic repeated evidence".into(),
            ..Default::default()
        }],
    };
    assert_eq!(h.ingest(&request).unwrap().recorded.len(), 1);
    for (id, kind, parent) in [
        ("owner", PrincipalKind::Human, None),
        ("device", PrincipalKind::Device, Some("owner")),
    ] {
        h.store
            .upsert_principal(&Principal {
                principal_id: id.into(),
                kind,
                parent_id: parent.map(str::to_string),
                pubkey: None,
                display_name: None,
                created_at: 0,
            })
            .unwrap();
    }
    h.store.set_alias(h.store.author(), "device").unwrap();
    assert_eq!(h.ingest(&request).unwrap().recorded.len(), 1);
    let rows: i64 = h
        .store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM prompts WHERE principal_id='owner' AND device_id='device'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        rows, 2,
        "identity stamping must preserve both historical sources"
    );
    assert_eq!(h.ingest(&request).unwrap().skipped, 1);
    assert_eq!(h.store.max_ledger_seq().unwrap(), 2);
    assert!(h.verify().unwrap().ok);
}

#[test]
fn concurrent_connections_commit_one_source_and_one_citation_for_a_retry() {
    let path = std::env::temp_dir().join(format!(
        "polis-atomic-capture-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let first = PolisStore::open(&path).unwrap();
    let second = PolisStore::open(&path).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let workers = [first, second]
        .into_iter()
        .map(|store| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let h =
                    PolisHandle::new(Arc::new(store), None, Arc::new(NoHost), Arc::new(NoopSink));
                barrier.wait();
                h.ingest(&IngestRequest {
                    scope: scope("/allowed"),
                    items: vec![IngestItem {
                        body: "concurrent idempotent capture".into(),
                        role: Some("assistant".into()),
                        ..Default::default()
                    }],
                })
                .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let receipts = workers
        .into_iter()
        .map(|w| w.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(receipts.iter().map(|r| r.recorded.len()).sum::<usize>(), 1);
    assert_eq!(receipts.iter().map(|r| r.skipped).sum::<usize>(), 1);
    let store = PolisStore::open(&path).unwrap();
    assert_eq!(store.max_ledger_seq().unwrap(), 1);
    assert_eq!(
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM prompts", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(store.verify_ledger_chain().unwrap().ok);
    drop(store);
    let _ = std::fs::remove_file(path);
}
