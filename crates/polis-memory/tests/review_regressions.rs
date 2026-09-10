// SPDX-License-Identifier: Apache-2.0
//! Independent provenance/lifecycle regressions from implementation review.
use polis_core::{
    api::{IngestItem, IngestRequest, SearchRequest},
    diagnostics::{DecisionRequest, EvidenceRequest},
    host::NoHost,
    MemoryApi,
};
use polis_llm::NoopSink;
use polis_memory::PolisHandle;
use polis_store::PolisStore;
use std::sync::Arc;

fn setup() -> (Arc<PolisStore>, PolisHandle) {
    let store = Arc::new(PolisStore::open_in_memory().unwrap());
    let api = PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink));
    (store, api)
}
fn prompt(api: &PolisHandle, text: &str) -> i64 {
    api.ingest(&IngestRequest {
        items: vec![IngestItem {
            body: text.into(),
            role: Some("user".into()),
            session: Some(text.len().to_string()),
            ..Default::default()
        }],
        ..Default::default()
    })
    .unwrap()
    .recorded[0]
}

#[test]
fn decision_evidence_keeps_a_supporting_passage_after_character_4000() {
    let (_, api) = setup();
    let body = format!(
        "{} Selected build cache: tailmarkerhelios.",
        "background context ".repeat(400)
    );
    let source = prompt(&api, &body);
    let decision = api
        .decide(&DecisionRequest {
            source_seq: source,
            ..Default::default()
        })
        .unwrap()
        .seq
        .unwrap();
    let exact = api
        .evidence(&EvidenceRequest {
            seq: decision,
            ..Default::default()
        })
        .unwrap();
    assert!(exact
        .item
        .unwrap()
        .body
        .unwrap()
        .contains("tailmarkerhelios"));
    let pack = api
        .search(&SearchRequest {
            q: Some("tailmarkerhelios".into()),
            ..Default::default()
        })
        .unwrap();
    let evidence = pack
        .prompt_hits
        .iter()
        .find(|hit| hit.item.seq == decision)
        .expect("decision candidate should remain directly reachable");
    assert!(evidence
        .item
        .body
        .as_deref()
        .unwrap()
        .contains("tailmarkerhelios"));
}

#[test]
fn a_corrupted_decision_projection_cannot_attribute_unrelated_source_text() {
    let (store, api) = setup();
    let original = prompt(&api, "Selected technology is Cedar.");
    let unrelated = prompt(
        &api,
        "Unrelated private juniperword that never supported the decision.",
    );
    let decision = api
        .decide(&DecisionRequest {
            source_seq: original,
            ..Default::default()
        })
        .unwrap()
        .seq
        .unwrap();
    store
        .conn()
        .execute(
            "UPDATE decision_evidence SET source_seq = ?2 WHERE seq = ?1",
            rusqlite::params![decision, unrelated],
        )
        .unwrap();
    let exact = api
        .evidence(&EvidenceRequest {
            seq: decision,
            ..Default::default()
        })
        .unwrap();
    assert!(exact
        .item
        .as_ref()
        .and_then(|item| item.body.as_deref())
        .is_none_or(|text| !text.contains("juniperword")));
    let pack = api
        .search(&SearchRequest {
            q: Some("juniperword".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(!pack.prompt_hits.iter().any(|hit| hit.item.seq == decision));
}

#[test]
fn forgetting_a_decision_source_retires_cited_observations_and_link_notes() {
    let (store, api) = setup();
    let source = prompt(&api, "privateobservationword is the selected technology.");
    let decision = api
        .decide(&DecisionRequest {
            source_seq: source,
            ..Default::default()
        })
        .unwrap()
        .seq
        .unwrap();
    store
        .seed_class_roots(&[("root".into(), "Reviewed class".into(), None)])
        .unwrap();
    store.conn().execute("INSERT INTO class_links(node_id,target_kind,target_id,note,status,created_at) VALUES('root','decision',?1,'privateobservationword','accepted',1)",[decision.to_string()]).unwrap();
    store
        .insert_class_observation(
            "root",
            "Repeated privateobservationword decision",
            &[decision],
            "test",
        )
        .unwrap();
    let prompt_id: i64 = store
        .conn()
        .query_row(
            "SELECT prompt_id FROM ledger_events WHERE seq = ?1",
            [source],
            |r| r.get(0),
        )
        .unwrap();
    store
        .compact_prompt_body(prompt_id, "[forgotten]", "forget", "deterministic", "test")
        .unwrap();
    let exact = api
        .evidence(&EvidenceRequest {
            seq: decision,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(exact.status, "redacted");
    let pack = api
        .search(&SearchRequest {
            q: Some("privateobservationword".into()),
            node: Some("root".into()),
            ..Default::default()
        })
        .unwrap();
    // The caller's own query is allowed in the response; evidence copies are not.
    assert!(pack.prompt_hits.is_empty());
    let node = pack.node.unwrap();
    assert!(node.observations.is_empty());
    assert!(node.links.iter().all(|link| link.link.note.is_none()));
    assert!(store.verify_ledger_chain().unwrap().ok);
}

#[test]
fn forgetting_a_citation_uses_its_source_row_and_expires_copied_proposals() {
    let (store,api)=setup();
    let first=prompt(&api,"keep this first source");
    api.decide(&DecisionRequest {source_seq:first,..Default::default()}).unwrap();
    let target=prompt(&api,"erase secretproposalmarker");
    assert_ne!(target,store.conn().query_row("SELECT prompt_id FROM ledger_events WHERE seq=?1",[target],|r|r.get::<_,i64>(0)).unwrap());
    store.conn().execute("INSERT INTO class_proposals(op,title,summary,extra_json,rationale,status,created_at) VALUES('collapse','secretproposalmarker','secretproposalmarker',?1,'secretproposalmarker','proposed',1)",[format!("{{\"cite_seqs\":[{target}]}}")]).unwrap();
    api.forget(&polis_core::api::ForgetRequest {target_kind:"ledger_event".into(),target_id:target.to_string(),confirm:"forget".into(),..Default::default()}).unwrap();
    assert_eq!(api.evidence(&EvidenceRequest {seq:first,..Default::default()}).unwrap().status,"available");
    assert_eq!(api.evidence(&EvidenceRequest {seq:target,..Default::default()}).unwrap().status,"redacted");
    let row=store.conn().query_row("SELECT status,title,summary,extra_json,rationale FROM class_proposals",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?))).unwrap();
    assert_eq!(row,("expired".into(),None,None,None,None));
    assert!(api.verify().unwrap().ok);
}

#[test]
fn browse_forgetting_survives_restore_even_when_the_live_database_is_corrupt() {
    let dir=std::env::temp_dir().join(format!("polis-page-forget-{}-{}",std::process::id(),polis_core::ledger::now_millis()));
    std::fs::create_dir_all(&dir).unwrap();let live=dir.join("polis.db");let backups=dir.join("backups");
    let seq;let snapshot;
    {
        let store=Arc::new(PolisStore::open(&live).unwrap());
        let api=PolisHandle::new(store.clone(),None,Arc::new(NoHost),Arc::new(NoopSink));
        seq=polis_store::record::record_browse_event_at(&store,polis_store::record::BrowseEventInput {action:polis_store::record::BrowseAction::Navigate,browse_id:None,url:"https://example.test/privatepage".into(),title:Some("privatepage".into()),text:"privatepage exact body".into(),from_event_id:None,author:None},100).unwrap().unwrap();
        snapshot=polis_memory::backup::backup_now(&store,&backups).unwrap();
        let req=polis_core::api::ForgetRequest {target_kind:"ledger_event".into(),target_id:seq.to_string(),confirm:"forget".into(),..Default::default()};
        api.forget(&req).unwrap();assert!(api.forget(&req).unwrap().seq.is_none());
        assert_eq!(api.evidence(&EvidenceRequest {seq,..Default::default()}).unwrap().status,"redacted");
        assert!(api.search(&SearchRequest {q:Some("privatepage".into()),..Default::default()}).unwrap().browse_hits.is_empty());
        assert!(api.verify().unwrap().ok);
    }
    std::fs::write(&live,b"corrupt database").unwrap();
    polis_memory::backup::restore(&live,Some(&snapshot),&backups).unwrap();
    let store=Arc::new(PolisStore::open(&live).unwrap());let api=PolisHandle::new(store,None,Arc::new(NoHost),Arc::new(NoopSink));
    assert_eq!(api.evidence(&EvidenceRequest {seq,..Default::default()}).unwrap().status,"redacted");
    assert!(api.verify().unwrap().ok);drop(api);std::fs::remove_dir_all(dir).unwrap();
}
