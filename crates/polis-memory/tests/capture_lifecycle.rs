// SPDX-License-Identifier: Apache-2.0
//! Capture's write-side guards: secrets are redacted before anything is
//! hashed or indexed (no byte of the value reaches the store file), and an
//! assistant turn that merely restates memory injected into its session is
//! not recorded again as fresh evidence.

use std::sync::Arc;

use polis_core::api::{CaptureRequest, IngestItem, IngestRequest, RememberRequest, Scope};
use polis_core::host::NoHost;
use polis_core::ledger::{body_hash, Origin};
use polis_core::MemoryApi;
use polis_llm::NoopSink;
use polis_memory::PolisHandle;
use polis_store::PolisStore;

fn handle() -> PolisHandle {
    PolisHandle::new(Arc::new(PolisStore::open_in_memory().unwrap()), None, Arc::new(NoHost), Arc::new(NoopSink)).with_scrub(true)
}

const KEY: &str = "sk-proj-AbCdEfGhIjKlMnOpQrStUvWx0123";
const AWS: &str = "AKIAIOSFODNN7EXAMPLE";
const PASSWORD: &str = "hunter2hunter2hunter2";

#[test]
fn secrets_never_reach_the_store_through_any_write_path() {
    let api = handle();
    let captured = api
        .capture(&CaptureRequest {
            body: format!("deploy with OPENAI_API_KEY={KEY} please"),
            origin: Origin::External,
            surface: "external".into(),
            session: Some("s1".into()),
            project: Some("/repo".into()),
        })
        .unwrap()
        .unwrap();
    api.remember(&RememberRequest { text: format!("the AWS key id is {AWS}"), as_user: true, ..Default::default() }).unwrap();
    api.ingest(&IngestRequest {
        items: vec![IngestItem { body: format!("I set DB_PASSWORD={PASSWORD} in .env"), role: Some("assistant".into()), session: Some("s1".into()), ..Default::default() }],
        scope: Scope::default(),
    })
    .unwrap();

    // The row, its hash on the chain, and the lexical index carry the marker.
    let store = &api.store;
    let body: String = store.conn().query_row("SELECT p.body FROM prompts p JOIN ledger_events le ON le.prompt_id = p.id WHERE le.seq = ?1", [captured], |r| r.get(0)).unwrap();
    assert_eq!(body, "deploy with OPENAI_API_KEY=[redacted:openai_key] please");
    let payload_hash: String = store.conn().query_row("SELECT payload_hash FROM ledger_events WHERE seq = ?1", [captured], |r| r.get(0)).unwrap();
    assert_eq!(payload_hash, body_hash(&body), "the chain commits to the scrubbed body");
    let fts: i64 = store.conn().query_row("SELECT count(*) FROM prompts_fts WHERE prompts_fts MATCH '\"redacted\"'", [], |r| r.get(0)).unwrap();
    assert_eq!(fts, 3);
    assert!(store.verify_ledger_chain().unwrap().ok);

    // No byte of any value is anywhere in the file: rows, FTS shadow
    // tables, the ledger, the meta counters.
    let dir = std::env::temp_dir().join(format!("polis-scrub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let copy = dir.join("copy.db");
    store.snapshot_to(&copy).unwrap();
    let bytes = std::fs::read(&copy).unwrap();
    for secret in [KEY, AWS, PASSWORD] {
        assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "{secret} reached the store file");
    }
    let _ = std::fs::remove_dir_all(&dir);

    let counts = api.redaction_counts();
    assert_eq!(counts, vec![("assigned_secret".to_string(), 1), ("aws_access_key".to_string(), 1), ("openai_key".to_string(), 1)]);
}

/// A library host's records are unchanged unless it opts in.
#[test]
fn scrubbing_is_off_for_a_library_host_until_it_opts_in() {
    let api = PolisHandle::new(Arc::new(PolisStore::open_in_memory().unwrap()), None, Arc::new(NoHost), Arc::new(NoopSink));
    let seq = api
        .capture(&CaptureRequest { body: format!("key {KEY}"), origin: Origin::External, surface: "external".into(), session: None, project: None })
        .unwrap()
        .unwrap();
    let body: String = api.store.conn().query_row("SELECT p.body FROM prompts p JOIN ledger_events le ON le.prompt_id = p.id WHERE le.seq = ?1", [seq], |r| r.get(0)).unwrap();
    assert_eq!(body, format!("key {KEY}"));
    assert!(api.redaction_counts().is_empty());
}

fn assistant(api: &PolisHandle, body: &str, session: &str) -> polis_core::api::IngestReceipt {
    api.ingest(&IngestRequest {
        items: vec![IngestItem { body: body.into(), role: Some("assistant".into()), session: Some(session.into()), run: Some(session.into()), ..Default::default() }],
        scope: Scope::default(),
    })
    .unwrap()
}

#[test]
fn an_assistant_echo_of_injected_memory_is_not_recorded_again() {
    let api = handle();
    let fact = "The Bluebird API port is 9090 after the March migration, per the infra review.";
    let source = api
        .ingest(&IngestRequest { items: vec![IngestItem { body: fact.into(), session: Some("old".into()), ..Default::default() }], scope: Scope::default() })
        .unwrap()
        .recorded[0];
    api.store.record_injection("now", &[source], polis_core::ledger::now_millis()).unwrap();

    // The whole reply restates the injected record: skipped, and counted.
    let echo = assistant(&api, "The Bluebird API port is 9090 after the March migration, per the infra review!", "now");
    assert_eq!((echo.recorded.len(), echo.skipped), (0, 1));
    assert_eq!(api.store.meta(polis_memory::ECHO_SKIPPED_KEY).unwrap().as_deref(), Some("1"));

    // A reply that says something new is evidence, even beside the echo.
    let new = assistant(&api, "I changed the health check to probe 9090 and added a retry with jitter to the client; tests pass.", "now");
    assert_eq!(new.recorded.len(), 1);

    // The same words in a session nothing was injected into are evidence.
    let elsewhere = assistant(&api, "The Bluebird API port is 9090 after the March migration, per the infra review!", "other");
    assert_eq!(elsewhere.recorded.len(), 1);

    // A user saying it is never an echo.
    let user = api
        .ingest(&IngestRequest {
            items: vec![IngestItem { body: "The Bluebird API port is 9090 after the March migration, per the infra review?".into(), session: Some("now".into()), ..Default::default() }],
            scope: Scope::default(),
        })
        .unwrap();
    assert_eq!(user.recorded.len(), 1);
}
