use polis_core::{ledger::LedgerAppend, types::LedgerFilters};
use polis_store::{record::{record_browse_event, BrowseAction, BrowseEventInput}, PolisStore};

#[test]
fn page_filing_and_filter_use_seq_even_when_a_row_id_collides() {
    let store = PolisStore::open_in_memory().unwrap();
    for n in 1..=2 {
        store.append_ledger_event(&LedgerAppend { kind:"prompt", author:"test", ts:n,
            prompt_id:None, session_id:None, version_number:None, ref_kind:None, ref_id:None, payload_hash:"test" }).unwrap();
    }
    let seq = record_browse_event(&store, BrowseEventInput { action:BrowseAction::Navigate,
        browse_id:Some("tab".into()), url:"https://example.test".into(), title:None,
        text:"page".into(), from_event_id:None, author:None }).unwrap().unwrap();
    assert_eq!(seq, 3);
    {
        let conn = store.conn();
        conn.execute_batch("INSERT INTO class_nodes(id,kind,title,status,created_at,updated_at) VALUES
            ('correct','node','Correct page','accepted',1,1), ('decoy','node','Wrong page','accepted',1,1);
            INSERT INTO class_links(node_id,target_kind,target_id,status,created_at) VALUES
            ('decoy','browse_event','1','accepted',1), ('correct','browse_event','3','accepted',1);").unwrap();
    }
    let all = store.query_ledger_events(&LedgerFilters::default()).unwrap();
    let page = all.iter().find(|e| e.event.seq == seq).unwrap();
    assert_eq!(page.event.ref_id.as_deref(), Some("1"));
    assert_eq!(page.class_title.as_deref(), Some("Correct page"));
    let filed = store.query_ledger_events(&LedgerFilters { class_node:Some("correct".into()), ..Default::default() }).unwrap();
    assert_eq!(filed.iter().map(|e| e.event.seq).collect::<Vec<_>>(), vec![3]);
    let decoy = store.query_ledger_events(&LedgerFilters { class_node:Some("decoy".into()), ..Default::default() }).unwrap();
    assert!(!decoy.iter().any(|e| e.event.seq == seq));
    // Scope joins resolve the link's seq back to its page row before checking
    // identity. The old embedding-target shortcut compared seq 3 with row 1.
    store.conn().execute_batch("UPDATE browse_events SET principal_id='person';
        UPDATE class_nodes SET principal_id='person';
        UPDATE class_links SET note='copied page text' WHERE node_id='correct';").unwrap();
    let scoped = store.list_class_links_for_node_scoped("correct", &polis_store::principals::ScopeFilter {
        principal:Some("person".into()), ..Default::default()
    }).unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].target_id, "3");
    // Forgetting a page must scrub seq-keyed derived copies and refuse any
    // delayed organizer output which tries to reintroduce that capture.
    store.forget_browse_event(1, "person").unwrap();
    let note: Option<String> = store.conn().query_row("SELECT note FROM class_links WHERE node_id='correct'", [], |r| r.get(0)).unwrap();
    assert_eq!(note, None);
    let staged = store.stage_proposal(None, &polis_core::proposal::Proposal::File {
        parent_id:"correct".into(), sub_class:None, target_kind:"browse_event".into(), target_id:seq.to_string(), note:Some("stale copy".into()), rationale:None
    }).unwrap();
    assert!(matches!(staged, polis_core::types::StagedOutcome::Skipped));
}
