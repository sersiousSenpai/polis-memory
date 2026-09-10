// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use polis_core::types::{NoteOutcome, NoteWrite};
use polis_memory::{backup, envelope, identity, sharing};
use polis_store::PolisStore;

struct Home {
    dir: PathBuf,
    store: PolisStore,
    identity: identity::Identity,
}
fn home(seed: u8) -> Home {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "polis-note-share-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let store = PolisStore::open(&dir.join("polis.db")).unwrap();
    let identity = identity::Identity::from_seed([seed; 32], "note-peer");
    identity::adopt(&store, &identity, "fixture").unwrap();
    Home {
        dir,
        store,
        identity,
    }
}
fn note(home: &Home, id: Option<i64>, text: &str) -> polis_core::types::UserNote {
    match home
        .store
        .write_user_note(
            &NoteWrite {
                note_id: id,
                target_kind: Some("none".into()),
                target_id: None,
                text: Some(text.into()),
                starred: None,
            },
            &home.identity.device_id(),
        )
        .unwrap()
    {
        NoteOutcome::Written(note) => note,
        other => panic!("unexpected note outcome {other:?}"),
    }
}
fn export(home: &Home, from: Option<i64>) -> envelope::Envelope {
    envelope::build(
        &home.store,
        &home.identity,
        "fixture",
        &envelope::BuildOptions {
            from_seq: from,
            ..Default::default()
        },
    )
    .unwrap()
}
fn import(store: &PolisStore, env: &envelope::Envelope) {
    sharing::import(
        store,
        env,
        &sharing::ImportOptions {
            tofu: true,
            ..Default::default()
        },
    )
    .unwrap();
}
fn text(store: &PolisStore, chain: &str, id: i64) -> String {
    store
        .conn()
        .query_row(
            "SELECT text FROM foreign_notes WHERE chain_id=?1 AND note_id=?2",
            rusqlite::params![chain, id],
            |r| r.get(0),
        )
        .unwrap()
}
fn add_note_vector(store: &PolisStore, chain: &str, id: i64) {
    let rowid = store
        .conn()
        .query_row(
            "SELECT rowid FROM foreign_notes WHERE chain_id=?1 AND note_id=?2",
            rusqlite::params![chain, id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    let chunk = polis_core::vec::Chunk {
        ix: 0,
        char_start: 0,
        char_len: 4,
        text: "note".into(),
    };
    store
        .store_embeddings(
            "foreign_note",
            rowid,
            "fixture",
            "hash",
            &[(chunk, polis_core::vec::quantize(&[1., 0.]))],
        )
        .unwrap();
}
fn vector_count(store: &PolisStore) -> i64 {
    store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM embeddings WHERE target_kind='foreign_note'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn every_note_version_is_redacted_idempotently_on_peers_replay_and_managed_restore() {
    let source = home(31);
    let peer = home(32);
    let newer = home(33);
    let late = home(34);
    let first = note(&source, None, "private note first version");
    let initial = export(&source, None);
    assert_eq!(initial.payload.notes.len(), 1);
    import(&peer.store, &initial);
    let chain = source.identity.device_id();
    assert_eq!(text(&peer.store, &chain, first.id), first.text);
    add_note_vector(&peer.store, &chain, first.id);
    let snapshot = peer.dir.join("before-forget.db");
    peer.store.snapshot_to(&snapshot).unwrap();
    let second = note(&source, Some(first.id), "private note revised version");
    let updated = export(&source, None);
    import(&newer.store, &updated);
    assert_eq!(text(&newer.store, &chain, first.id), second.text);

    source
        .store
        .forget_user_note(first.id, &chain)
        .unwrap()
        .unwrap();
    source.store.conn().execute_batch("CREATE TRIGGER fail_redaction BEFORE INSERT ON ledger_events WHEN NEW.kind='redaction' BEGIN SELECT RAISE(ABORT,'interrupted redaction'); END;").unwrap();
    assert_eq!(
        sharing::flush_redactions(&source.store, &chain, &chain).unwrap(),
        0
    );
    assert_eq!(source.store.conn().query_row("SELECT COUNT(*) FROM capture_redaction_outbox WHERE delivered_at IS NULL AND attempts=1 AND error IS NOT NULL",[],|r|r.get::<_,i64>(0)).unwrap(),2);
    source
        .store
        .conn()
        .execute_batch("DROP TRIGGER fail_redaction")
        .unwrap();
    assert_eq!(
        sharing::flush_redactions(&source.store, &chain, &chain).unwrap(),
        2
    );
    let head = source.store.max_ledger_seq().unwrap();
    assert_eq!(
        sharing::flush_redactions(&source.store, &chain, &chain).unwrap(),
        0
    );
    assert_eq!(source.store.max_ledger_seq().unwrap(), head);
    let own = source.store.own_redactions().unwrap();
    assert_eq!(own.len(), 2);
    let redaction = own
        .iter()
        .find(|(_, _, target)| *target == first.seq.unwrap())
        .unwrap()
        .0;
    source
        .store
        .conn()
        .execute(
            "DELETE FROM polis_meta WHERE key=?1",
            [format!("polis.redaction.{redaction}")],
        )
        .unwrap();
    source
        .store
        .conn()
        .execute(
            "UPDATE capture_redaction_outbox SET delivered_at=NULL WHERE target_seq=?1",
            [first.seq.unwrap()],
        )
        .unwrap();
    assert_eq!(
        sharing::flush_redactions(&source.store, &chain, &chain).unwrap(),
        1
    );
    assert_eq!(
        source.store.max_ledger_seq().unwrap(),
        head,
        "retry repairs payload without another ledger event"
    );

    let forgotten = export(&source, None);
    assert!(forgotten.payload.notes.is_empty());
    envelope::verify(&forgotten).unwrap();
    import(&peer.store, &forgotten);
    import(&newer.store, &forgotten);
    import(&late.store, &forgotten);
    assert_eq!(text(&peer.store, &chain, first.id), "");
    assert_eq!(text(&newer.store, &chain, first.id), "");
    assert_eq!(vector_count(&peer.store), 0);
    // A late body import at an already-verified source seq must materialize
    // only a tombstone, even if no foreign note row existed at redaction time.
    import(&late.store, &initial);
    assert_eq!(text(&late.store, &chain, first.id), "");
    import(&late.store, &updated);
    assert_eq!(text(&late.store, &chain, first.id), "");
    // Replayed segments also purge a stale derived vector independently of
    // whether the source body has already been cleared.
    add_note_vector(&peer.store, &chain, first.id);
    import(&peer.store, &initial);
    import(&peer.store, &updated);
    assert_eq!(text(&peer.store, &chain, first.id), "");
    assert_eq!(vector_count(&peer.store), 0);

    let live = peer.dir.join("polis.db");
    drop(peer.store);
    backup::restore(&live, Some(&snapshot), &peer.dir).unwrap();
    let restored = PolisStore::open(&live).unwrap();
    assert_eq!(text(&restored, &chain, first.id), "");
    assert_eq!(vector_count(&restored), 0);
    assert_eq!(
        restored
            .conn()
            .query_row("SELECT COUNT(*) FROM foreign_redactions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0,
        "restore markers must not fabricate signed peer events"
    );
    import(&restored, &initial);
    assert_eq!(text(&restored, &chain, first.id), "");
    assert!(restored.verify_ledger_chain().unwrap().ok);
    assert!(source.store.verify_ledger_chain().unwrap().ok);
}

#[test]
fn signed_peer_cannot_redact_another_chain_or_attach_uncommitted_redaction_payloads() {
    let victim = home(41);
    let attacker = home(42);
    let peer = home(43);
    let kept = note(&victim, None, "the victim note remains readable");
    import(&peer.store, &export(&victim, None));
    let target = envelope::RedactionPayload {
        chain_id: victim.identity.device_id(),
        seq: kept.seq.unwrap(),
        event_seq: 0,
    };
    let event = attacker
        .store
        .append_ledger_event(&polis_core::ledger::LedgerAppend {
            kind: "redaction",
            author: &attacker.identity.device_id(),
            ts: 1,
            prompt_id: None,
            session_id: None,
            version_number: None,
            ref_kind: None,
            ref_id: None,
            payload_hash: &target.payload_hash(),
        })
        .unwrap();
    let payload = envelope::RedactionPayload {
        event_seq: event.seq,
        ..target
    };
    attacker
        .store
        .set_meta(
            &format!("polis.redaction.{}", event.seq),
            &serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
    let signed = export(&attacker, None);
    assert!(sharing::import(
        &peer.store,
        &signed,
        &sharing::ImportOptions {
            tofu: true,
            ..Default::default()
        }
    )
    .is_err());
    assert_eq!(
        text(&peer.store, &victim.identity.device_id(), kept.id),
        kept.text
    );
    assert!(peer
        .store
        .trust_get(&attacker.identity.principal_id())
        .unwrap()
        .is_none());

    let uncommitted = home(44);
    let local = note(&uncommitted, None, "ordinary note event");
    let mut env = export(&uncommitted, None);
    env.payload.redactions.push(envelope::RedactionPayload {
        chain_id: uncommitted.identity.device_id(),
        seq: local.seq.unwrap(),
        event_seq: local.seq.unwrap(),
    });
    env.header.payload_sha256 = polis_core::ledger::sha256_hex(env.payload_json().as_bytes());
    env.signature = uncommitted.identity.sign_hex(env.header_line().as_bytes());
    assert!(sharing::import(
        &peer.store,
        &env,
        &sharing::ImportOptions {
            tofu: true,
            ..Default::default()
        }
    )
    .is_err());
}

#[test]
fn signed_note_payloads_must_match_their_immutable_source() {
    let source = home(51);
    let peer = home(52);
    note(&source, None, "committed note body");
    let original = export(&source, None);
    for mutation in 0..4 {
        let mut env = original.clone();
        match mutation {
            0 => env.payload.notes[0].text = "substituted note body".into(),
            1 => env.payload.notes[0].id += 100,
            2 => env.payload.notes[0].seq = None,
            3 => env.payload.notes[0].text.clear(),
            _ => unreachable!(),
        }
        env.header.payload_sha256 = polis_core::ledger::sha256_hex(env.payload_json().as_bytes());
        env.signature = source.identity.sign_hex(env.header_line().as_bytes());
        assert!(
            sharing::import(
                &peer.store,
                &env,
                &sharing::ImportOptions {
                    tofu: true,
                    ..Default::default()
                }
            )
            .is_err(),
            "note payload mutation {mutation} must be refused"
        );
        assert!(peer
            .store
            .trust_get(&source.identity.principal_id())
            .unwrap()
            .is_none());
    }
    import(&peer.store, &original);
}
