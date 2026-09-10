// SPDX-License-Identifier: Apache-2.0
//! Independent lifecycle tests. Every fixture uses a new synthetic store.
use polis_core::{
    api::{
        AnnotateRequest, ForgetRequest, IngestItem, IngestRequest, Scope, SearchRequest,
        WriteReceipt,
    },
    diagnostics::EvidenceRequest,
    host::NoHost,
    types::{NoteOutcome, NoteWrite},
    MemoryApi,
};
use polis_llm::NoopSink;
use polis_memory::{backup, PolisHandle};
use polis_store::{runs::OpRecord, PolisStore};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

fn api(store: &Arc<PolisStore>) -> PolisHandle {
    PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink))
}
fn scope(project: &str) -> Scope {
    Scope {
        project: Some(project.into()),
        ..Default::default()
    }
}
fn filler(handle: &PolisHandle) -> i64 {
    handle
        .ingest(&IngestRequest {
            items: vec![IngestItem {
                body: "A separate source that must remain available.".into(),
                ..Default::default()
            }],
            scope: scope("/notes"),
        })
        .unwrap()
        .recorded[0]
}
fn annotate(
    handle: &PolisHandle,
    target_kind: &str,
    target_id: Option<String>,
    text: &str,
) -> WriteReceipt {
    handle
        .annotate(&AnnotateRequest {
            target_kind: target_kind.into(),
            target_id,
            text: text.into(),
            scope: scope("/notes"),
        })
        .unwrap()
}
fn edit(store: &PolisStore, id: i64, text: Option<&str>, starred: Option<bool>) -> i64 {
    match store
        .write_user_note(
            &NoteWrite {
                note_id: Some(id),
                text: text.map(str::to_string),
                starred,
                ..Default::default()
            },
            "local",
        )
        .unwrap()
    {
        NoteOutcome::Written(note) => note.seq.unwrap(),
        other => panic!("expected a new note event: {other:?}"),
    }
}
fn forget(handle: &PolisHandle, kind: &str, id: i64) {
    assert!(
        handle
            .forget(&ForgetRequest {
                target_kind: kind.into(),
                target_id: id.to_string(),
                confirm: "forget".into(),
                scope: scope("/notes")
            })
            .unwrap()
            .forgotten
    );
}
fn text(store: &PolisStore, id: i64) -> String {
    store
        .conn()
        .query_row("SELECT text FROM user_notes WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .unwrap()
}
fn hashes(store: &PolisStore) -> Vec<(i64, String)> {
    let conn = store.conn();
    let mut stmt = conn
        .prepare("SELECT seq,entry_hash FROM ledger_events ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn assert_original_hashes(store: &PolisStore, before: &[(i64, String)]) {
    let after = hashes(store);
    assert_eq!(&after[..before.len()], before);
    assert!(store.verify_ledger_chain().unwrap().ok);
}
fn assert_redacted(handle: &PolisHandle, seqs: &[i64]) {
    for seq in seqs {
        let evidence = handle
            .evidence(&EvidenceRequest {
                seq: *seq,
                scope: scope("/notes"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            evidence.status, "redacted",
            "note citation {seq} must retain its tombstone"
        );
        assert!(evidence
            .item
            .as_ref()
            .and_then(|item| item.body.as_ref())
            .is_none());
    }
}

#[test]
fn standalone_note_citation_forgets_all_edits_and_stars_without_confusing_row_ids() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    let unrelated_prompt = filler(&handle);
    let note = annotate(
        &handle,
        "none",
        None,
        "firstnoteprivateword original preference",
    );
    let id = note.id.unwrap();
    let first = note.seq.unwrap();
    assert_ne!(
        id, first,
        "ledger citations and note row IDs must differ in this fixture"
    );
    let other = annotate(&handle, "none", None, "othernoteprivateword must remain");
    let latest = edit(
        &store,
        id,
        Some("editednoteprivateword revised preference"),
        None,
    );
    let star = edit(&store, id, None, Some(true));
    let old = handle
        .evidence(&EvidenceRequest {
            seq: first,
            scope: scope("/notes"),
            ..Default::default()
        })
        .unwrap();
    assert_ne!(
        old.item.and_then(|item| item.body).as_deref(),
        Some("editednoteprivateword revised preference"),
        "a historic citation cannot borrow the current edited body"
    );
    let before = hashes(&store);
    forget(&handle, "ledger_event", first);
    assert!(text(&store, id).is_empty());
    assert_eq!(
        text(&store, other.id.unwrap()),
        "othernoteprivateword must remain"
    );
    assert_redacted(&handle, &[first, latest, star]);
    assert_eq!(
        handle
            .evidence(&EvidenceRequest {
                seq: unrelated_prompt,
                scope: scope("/notes"),
                ..Default::default()
            })
            .unwrap()
            .status,
        "available"
    );
    let targets: Vec<i64> = {
        let conn = store.conn();
        let mut stmt = conn
            .prepare("SELECT target_seq FROM capture_redaction_outbox ORDER BY target_seq")
            .unwrap();
        let rows = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows
    };
    assert_eq!(
        targets,
        vec![first, latest, star],
        "peers may hold any prior edited or starred version"
    );
    let found = handle
        .search(&SearchRequest {
            q: Some("editednoteprivateword".into()),
            scope: scope("/notes"),
            ..Default::default()
        })
        .unwrap();
    assert!(found.notes.is_empty());
    assert_original_hashes(&store, &before);
}

#[test]
fn targeted_note_history_maps_to_the_note_and_preserves_the_annotated_source() {
    for target_kind in ["ledger_event", "class_node", "session"] {
        let store = Arc::new(PolisStore::open_in_memory().unwrap());
        let handle = api(&store);
        let prompt = filler(&handle);
        store
            .seed_class_roots(&[(
                "notes-root".into(),
                "Useful class".into(),
                Some("/notes".into()),
            )])
            .unwrap();
        let target = match target_kind {
            "ledger_event" => prompt.to_string(),
            "class_node" => "notes-root".into(),
            _ => "session-kept".into(),
        };
        let first = annotate(
            &handle,
            target_kind,
            Some(target.clone()),
            "targetfirstprivateword original annotation",
        );
        let second = annotate(
            &handle,
            target_kind,
            Some(target),
            "targeteditedprivateword revised annotation",
        );
        assert_eq!(first.id, second.id);
        let star = edit(&store, first.id.unwrap(), None, Some(true));
        let before = hashes(&store);
        forget(&handle, "note", first.id.unwrap());
        assert!(text(&store, first.id.unwrap()).is_empty());
        assert_redacted(&handle, &[first.seq.unwrap(), second.seq.unwrap(), star]);
        assert_eq!(
            handle
                .evidence(&EvidenceRequest {
                    seq: prompt,
                    scope: scope("/notes"),
                    ..Default::default()
                })
                .unwrap()
                .status,
            "available"
        );
        assert!(store.get_class_node("notes-root").unwrap().is_some());
        assert_original_hashes(&store, &before);
    }
}

#[test]
fn wrong_scope_cannot_forget_a_note_by_row_or_historical_citation() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    filler(&handle);
    let note = annotate(&handle, "none", None, "scopenoteprivateword preserved");
    edit(
        &store,
        note.id.unwrap(),
        Some("scopenoteprivateword updated"),
        None,
    );
    let before = hashes(&store);
    for (kind, id) in [
        ("note", note.id.unwrap()),
        ("user_note", note.id.unwrap()),
        ("ledger_event", note.seq.unwrap()),
    ] {
        assert!(handle
            .forget(&ForgetRequest {
                target_kind: kind.into(),
                target_id: id.to_string(),
                confirm: "forget".into(),
                scope: scope("/elsewhere")
            })
            .is_err());
    }
    assert_eq!(
        text(&store, note.id.unwrap()),
        "scopenoteprivateword updated"
    );
    assert_eq!(hashes(&store), before);
    let tombstones: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM forgotten_captures", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tombstones, 0);
}

#[test]
fn forgetting_a_note_expires_dependent_proposals_and_purges_revert_copies() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    filler(&handle);
    let note = annotate(&handle, "none", None, "organizerprivateword source note");
    let first = note.seq.unwrap();
    let latest = edit(
        &store,
        note.id.unwrap(),
        Some("organizerprivateword changed source"),
        None,
    );
    store
        .seed_class_roots(&[(
            "digest".into(),
            "A derived class".into(),
            Some("/notes".into()),
        )])
        .unwrap();
    store.conn().execute("UPDATE class_nodes SET kind='digest',summary='organizerprivateword digest' WHERE id='digest'",[]).unwrap();
    store.conn().execute("INSERT INTO class_links(node_id,target_kind,target_id,note,status,created_at) VALUES('digest','note',?1,'organizerprivateword copied note','accepted',1)",[first.to_string()]).unwrap();
    store
        .insert_class_observation(
            "digest",
            "organizerprivateword copied observation",
            &[first],
            "fixture",
        )
        .unwrap();
    let run = store.insert_class_run(first, latest).unwrap();
    store
        .journal_op(
            run,
            &OpRecord::applied(
                "create",
                vec![format!("seq:{first}")],
                serde_json::json!({"title":"organizerprivateword inverse copy"}),
            )
            .with_post_image(serde_json::json!({"summary":"organizerprivateword post copy"})),
        )
        .unwrap();
    store
        .journal_op(
            run,
            &OpRecord::refused(
                "collapse",
                vec![format!("seq:{first}")],
                "organizerprivateword refused rationale",
            ),
        )
        .unwrap();
    store.conn().execute("INSERT INTO class_proposals(run_id,op,node_id,title,summary,extra_json,rationale,status,created_at,last_reason) VALUES(?1,'collapse','digest','organizerprivateword title','organizerprivateword summary',?2,'organizerprivateword rationale','proposed',1,'organizerprivateword retry')",rusqlite::params![run,serde_json::json!({"cite_seqs":[first],"copied":"organizerprivateword payload"}).to_string()]).unwrap();
    forget(&handle, "user_note", note.id.unwrap());
    let conn = store.conn();
    let proposal: (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status,title,summary,extra_json,rationale FROM class_proposals WHERE run_id=?1",
            [run],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(proposal, ("expired".into(), None, None, None, None));
    let copies:i64=conn.query_row("SELECT COUNT(*) FROM class_run_ops WHERE run_id=?1 AND (reason IS NOT NULL OR pre_image IS NOT NULL OR post_image IS NOT NULL)",[run],|r|r.get(0)).unwrap();
    assert_eq!(copies, 0);
    let link_note: Option<String> = conn
        .query_row(
            "SELECT note FROM class_links WHERE node_id='digest'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(link_note.is_none());
    let digest: Option<String> = conn
        .query_row(
            "SELECT summary FROM class_nodes WHERE id='digest'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(digest.is_none());
    let observations: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM class_observations WHERE summary LIKE '%organizerprivateword%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(observations, 0);
    drop(conn);
    assert!(store.list_due_proposals(run + 10).unwrap().is_empty());
    assert!(store.verify_ledger_chain().unwrap().ok);
}

#[test]
fn forgotten_notes_reject_text_star_and_targeted_rewrites() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    let target = filler(&handle);
    let note = annotate(
        &handle,
        "ledger_event",
        Some(target.to_string()),
        "rewriteprivateword original",
    );
    forget(&handle, "note", note.id.unwrap());
    let before = hashes(&store);
    for write in [
        NoteWrite {
            note_id: note.id,
            text: Some("rewriteprivateword resurrected".into()),
            ..Default::default()
        },
        NoteWrite {
            note_id: note.id,
            starred: Some(true),
            ..Default::default()
        },
    ] {
        let result = store.write_user_note(&write, "local");
        assert!(
            result.is_err() || matches!(result, Ok(NoteOutcome::Rejected(_))),
            "forgotten note writes must be rejected"
        );
    }
    assert!(handle
        .annotate(&AnnotateRequest {
            target_kind: "ledger_event".into(),
            target_id: Some(target.to_string()),
            text: "rewriteprivateword via target".into(),
            scope: scope("/notes")
        })
        .is_err());
    assert!(text(&store, note.id.unwrap()).is_empty());
    assert_eq!(hashes(&store), before);
}

#[test]
fn failed_note_ledger_append_rolls_back_new_and_existing_rows_and_history_mapping() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    filler(&handle);
    let note = annotate(&handle, "none", None, "atomicnoteprivateword preserved");
    let before = hashes(&store);
    let row_count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM user_notes", [], |r| r.get(0))
        .unwrap();
    let mapping_count: i64 = store
        .conn()
        .query_row("SELECT COUNT(*) FROM note_events", [], |r| r.get(0))
        .unwrap();
    store.conn().execute_batch("CREATE TRIGGER reject_note_event BEFORE INSERT ON ledger_events WHEN NEW.kind='note' BEGIN SELECT RAISE(ABORT,'fixture note ledger failure'); END;").unwrap();
    assert!(handle
        .annotate(&AnnotateRequest {
            target_kind: "none".into(),
            text: "failednoteprivateword new row".into(),
            scope: scope("/notes"),
            ..Default::default()
        })
        .is_err());
    assert!(store
        .write_user_note(
            &NoteWrite {
                note_id: note.id,
                text: Some("failednoteprivateword edit".into()),
                ..Default::default()
            },
            "local"
        )
        .is_err());
    assert_eq!(
        text(&store, note.id.unwrap()),
        "atomicnoteprivateword preserved"
    );
    assert_eq!(hashes(&store), before);
    assert_eq!(
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM user_notes", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        row_count
    );
    assert_eq!(
        store
            .conn()
            .query_row("SELECT COUNT(*) FROM note_events", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        mapping_count
    );
}

#[test]
fn a_stale_filing_reply_cannot_reintroduce_a_forgotten_note_or_its_subclass_title() {
    use polis_core::{proposal::Proposal, types::StagedOutcome};
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    filler(&handle);
    let note = annotate(
        &handle,
        "none",
        None,
        "stalenoteprivateword selected configuration",
    );
    let source = note.seq.unwrap();
    store
        .seed_class_roots(&[("root".into(), "Safe class".into(), Some("/notes".into()))])
        .unwrap();
    let in_flight = store.insert_class_run(source, source).unwrap();
    let reply = Proposal::File {
        parent_id: "root".into(),
        sub_class: Some("staletitleprivateword".into()),
        target_kind: "note".into(),
        target_id: source.to_string(),
        note: Some("stalenoteprivateword copied filing".into()),
        rationale: Some("stalenoteprivateword classifier rationale".into()),
    };
    forget(&handle, "ledger_event", source);
    let head = store.max_ledger_seq().unwrap();
    let later = store.insert_class_run(head, head).unwrap();
    for run in [in_flight, later] {
        assert!(
            matches!(
                store.stage_proposal(Some(run), &reply).unwrap(),
                StagedOutcome::Skipped
            ),
            "a stale source must be rejected even if re-staged under a later run"
        );
    }
    assert_eq!(
        store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM class_links WHERE note LIKE '%stalenoteprivateword%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM class_nodes WHERE title='staletitleprivateword'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn forged_note_mapping_cannot_redirect_citation_forgetting_to_an_unrelated_note() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    filler(&handle);
    let first = annotate(
        &handle,
        "none",
        None,
        "firstmappedprivateword original note",
    );
    let second = annotate(
        &handle,
        "none",
        None,
        "secondmappedprivateword unrelated note",
    );
    store
        .conn()
        .execute(
            "UPDATE note_events SET note_id=?2 WHERE seq=?1",
            rusqlite::params![first.seq, second.id],
        )
        .unwrap();
    let before = hashes(&store);
    let result = handle.forget(&ForgetRequest {
        target_kind: "ledger_event".into(),
        target_id: first.seq.unwrap().to_string(),
        confirm: "forget".into(),
        scope: scope("/notes"),
    });
    assert!(
        result.is_err(),
        "untrusted projection cannot redirect a destructive action"
    );
    assert_eq!(
        text(&store, first.id.unwrap()),
        "firstmappedprivateword original note"
    );
    assert_eq!(
        text(&store, second.id.unwrap()),
        "secondmappedprivateword unrelated note"
    );
    assert_eq!(hashes(&store), before);
}

fn scratch() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "polis-note-forget-{}-{}-{}",
        std::process::id(),
        polis_core::ledger::now_millis(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn delayed_organizer_summaries_observations_and_journals_cannot_restore_forgotten_text() {
    use polis_core::{
        proposal::Proposal,
        types::{ClassRunFinish, StagedOutcome},
    };
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    store
        .seed_class_roots(&[("root".into(), "Safe root".into(), Some("/notes".into()))])
        .unwrap();
    let note = annotate(&handle, "none", None, "delayedprivateword source");
    let seq = note.seq.unwrap();
    let run = store.insert_class_run(0, seq).unwrap();
    let collapse = Proposal::Collapse {
        node_id: "root".into(),
        summary: "delayedprivateword summary".into(),
        cite_seqs: vec![seq],
        rationale: None,
    };
    assert!(matches!(
        store.stage_proposal(Some(run), &collapse).unwrap(),
        StagedOutcome::Structural
    ));
    let proposal = store.list_class_proposals().unwrap()[0].id;
    forget(&handle, "ledger_event", seq);
    assert!(store
        .insert_class_observation("root", "delayedprivateword observation", &[seq], "test")
        .unwrap()
        .is_none());
    assert!(matches!(
        store
            .stage_proposal(
                Some(run),
                &Proposal::Create {
                    parent_id: "root".into(),
                    title: "delayedprivateword title".into(),
                    rationale: None
                }
            )
            .unwrap(),
        StagedOutcome::Skipped
    ));
    let later = store.insert_class_run(seq, seq + 1).unwrap();
    assert!(matches!(
        store.stage_proposal(Some(later), &collapse).unwrap(),
        StagedOutcome::Skipped
    ));
    store
        .defer_proposal(proposal, 1, later, "delayedprivateword verifier output")
        .unwrap();
    assert!(store
        .apply_class_proposal_in_run(proposal, "test", Some(later))
        .unwrap()
        .is_none());
    store
        .finish_class_run_with(
            run,
            &ClassRunFinish {
                status: "done".into(),
                summary: "delayedprivateword finished".into(),
                error: Some("delayedprivateword error".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        store
            .journal_op(
                run,
                &OpRecord::applied(
                    "create",
                    vec!["node:root".into()],
                    serde_json::json!({"title":"delayedprivateword image"})
                )
            )
            .unwrap(),
        0
    );
    let conn = store.conn();
    let summary: String = conn
        .query_row("SELECT summary FROM class_runs WHERE id=?1", [run], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(summary, "[forgotten source]");
    let reason: String = conn
        .query_row(
            "SELECT last_reason FROM class_proposals WHERE id=?1",
            [proposal],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "forgotten source");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM class_observations WHERE summary LIKE '%delayedprivateword%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn device_qualified_note_citations_resolve_and_targeted_notes_cannot_cross_scopes() {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let handle = api(&store);
    let note = annotate(
        &handle,
        "session",
        Some("shared-session-id".into()),
        "deviceprivateword annotation",
    );
    let chain = "a".repeat(64);
    store
        .conn()
        .execute(
            "UPDATE user_notes SET device_id=?1 WHERE id=?2",
            rusqlite::params![chain, note.id],
        )
        .unwrap();
    let request = EvidenceRequest {
        seq: note.seq.unwrap(),
        chain_id: Some(chain),
        scope: scope("/notes"),
    };
    assert_eq!(handle.evidence(&request).unwrap().status, "available");
    assert!(handle
        .annotate(&AnnotateRequest {
            target_kind: "session".into(),
            target_id: Some("shared-session-id".into()),
            text: "unauthorized replacement".into(),
            scope: scope("/other")
        })
        .is_err());
    assert_eq!(
        text(&store, note.id.unwrap()),
        "deviceprivateword annotation"
    );
    forget(&handle, "ledger_event", note.seq.unwrap());
    assert_eq!(handle.evidence(&request).unwrap().status, "redacted");
}

#[test]
fn legacy_note_histories_are_backfilled_before_forgetting_by_an_old_citation() {
    let dir = scratch();
    let live = dir.join("memory.db");
    let store = Arc::new(PolisStore::open(&live).unwrap());
    let handle = api(&store);
    let target = filler(&handle);
    let standalone = annotate(&handle, "none", None, "legacyfirstprivateword initial");
    let edited = edit(
        &store,
        standalone.id.unwrap(),
        Some("legacyeditedprivateword current"),
        None,
    );
    let targeted = annotate(
        &handle,
        "ledger_event",
        Some(target.to_string()),
        "legacytargetprivateword first",
    );
    let target_edit = annotate(
        &handle,
        "ledger_event",
        Some(target.to_string()),
        "legacytargetprivateword edited",
    );
    let first_hash: String = store
        .conn()
        .query_row(
            "SELECT entry_hash FROM ledger_events WHERE seq=?1",
            [standalone.seq],
            |r| r.get(0),
        )
        .unwrap();
    let original_hashes = hashes(&store);
    // Remove only the new mapping, then reopen at the old schema version.
    // The source ledger and current note rows are exactly a legacy store's.
    store
        .conn()
        .execute_batch("DROP TABLE note_events")
        .unwrap();
    store.set_meta("schema_version", "9").unwrap();
    drop(handle);
    drop(store);
    let store = Arc::new(PolisStore::open(&live).unwrap());
    let handle = api(&store);
    assert_eq!(
        hashes(&store),
        original_hashes,
        "note mapping migration must not alter or append ledger events"
    );
    let pairs: Vec<(i64, i64)> = {
        let conn = store.conn();
        let mut stmt = conn
            .prepare("SELECT seq,note_id FROM note_events ORDER BY seq")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows
    };
    for (seq, id) in [
        (standalone.seq.unwrap(), standalone.id.unwrap()),
        (edited, standalone.id.unwrap()),
        (targeted.seq.unwrap(), targeted.id.unwrap()),
        (target_edit.seq.unwrap(), targeted.id.unwrap()),
    ] {
        assert!(
            pairs.contains(&(seq, id)),
            "legacy mapping missing {seq} -> {id}"
        );
    }
    forget(&handle, "ledger_event", standalone.seq.unwrap());
    assert_eq!(
        text(&store, targeted.id.unwrap()),
        "legacytargetprivateword edited"
    );
    let registry = std::fs::read_to_string(format!("{}.forgotten", live.display())).unwrap();
    assert!(
        registry
            .lines()
            .any(|line| line == format!("user_note:{first_hash}")),
        "stable identity must use the first mapped event"
    );
    assert_redacted(&handle, &[standalone.seq.unwrap(), edited]);
    forget(&handle, "ledger_event", targeted.seq.unwrap());
    assert_redacted(&handle, &[targeted.seq.unwrap(), target_edit.seq.unwrap()]);
    assert_eq!(
        handle
            .evidence(&EvidenceRequest {
                seq: target,
                scope: scope("/notes"),
                ..Default::default()
            })
            .unwrap()
            .status,
        "available"
    );
    drop(handle);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn managed_restore_blocks_pre_edit_and_pre_forget_note_copies_even_with_corrupt_live_database() {
    for before_edit in [true, false] {
        for corrupt in [true, false] {
            let dir = scratch();
            let live = dir.join("memory.db");
            let backups = dir.join("backups");
            let store = Arc::new(PolisStore::open(&live).unwrap());
            let handle = api(&store);
            filler(&handle);
            let note = annotate(
                &handle,
                "none",
                None,
                "backupfirstprivateword first version",
            );
            let early = backup::backup_now(&store, &backups).unwrap();
            edit(
                &store,
                note.id.unwrap(),
                Some("backuplaterprivateword edited version"),
                None,
            );
            let late = backup::backup_now(&store, &backups).unwrap();
            let snapshot = if before_edit { early } else { late };
            let original = {
                let saved = PolisStore::open_read_only(&snapshot).unwrap();
                hashes(&saved)
            };
            forget(&handle, "ledger_event", note.seq.unwrap());
            let registry =
                std::fs::read_to_string(format!("{}.forgotten", live.display())).unwrap();
            assert!(registry.lines().any(|line| line.starts_with("user_note:")));
            assert!(!registry.contains("privateword"));
            drop(handle);
            drop(store);
            if corrupt {
                std::fs::write(&live, b"deliberately corrupted live SQLite fixture").unwrap();
            }
            backup::restore(&live, Some(&snapshot), &backups).unwrap();
            let restored = Arc::new(PolisStore::open(&live).unwrap());
            let handle = api(&restored);
            assert!(
                text(&restored, note.id.unwrap()).is_empty(),
                "restored note resurrected: before_edit={before_edit}, corrupt={corrupt}"
            );
            assert_redacted(&handle, &[note.seq.unwrap()]);
            assert_original_hashes(&restored, &original);
            let result = handle
                .search(&SearchRequest {
                    q: Some("backupfirstprivateword backuplaterprivateword".into()),
                    scope: scope("/notes"),
                    ..Default::default()
                })
                .unwrap();
            assert!(result.notes.is_empty());
            drop(handle);
            drop(restored);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
