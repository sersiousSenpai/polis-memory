use polis_core::{bundle::BundleScope, host::NoHost, ledger::LedgerAppend};
use polis_llm::NoopSink;
use polis_memory::{adjudicate::{adjudicate_file, Catalog, Verdict}, bundle::build_bundle, Polis};
use polis_store::PolisStore;

#[test]
fn writer_rejects_wrong_kind_and_scoped_bundle_keeps_prompt_and_page_seqs() {
    let store = PolisStore::open_in_memory().unwrap();
    for (n, kind) in [(1,"approval"), (2,"prompt"), (3,"browse_event")] {
        store.append_ledger_event(&LedgerAppend {kind, author:"test", ts:n, prompt_id:None,
            session_id:None, version_number:None, ref_kind:None, ref_id:None, payload_hash:"test"}).unwrap();
    }
    store.conn().execute_batch("INSERT INTO class_nodes(id,kind,title,status,created_at,updated_at)
        VALUES ('~general','root','General','accepted',1,1);
        INSERT INTO class_links(node_id,target_kind,target_id,status,created_at) VALUES
        ('~general','prompt','2','accepted',1), ('~general','browse_event','3','accepted',1);").unwrap();
    let catalog = Catalog::load(&store).unwrap();
    assert!(matches!(adjudicate_file(&catalog, &store, "~general", "browse_event", "2"), Verdict::Refuse(reason) if reason.contains("seq 2 is a prompt")));
    assert!(matches!(adjudicate_file(&catalog, &store, "~general", "prompt", "no-seq"), Verdict::Refuse(_)));
    assert_eq!(adjudicate_file(&catalog, &store, "~general", "browse_event", "3"), Verdict::Apply);
    assert_eq!(adjudicate_file(&catalog, &store, "~general", "decision", "1"), Verdict::Apply);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let bundle = build_bundle(&polis, &BundleScope::Class("~general".into())).unwrap();
    assert_eq!(bundle.events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![2,3]);
}
