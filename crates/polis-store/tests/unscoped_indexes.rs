// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! E2's five partial "unscoped" indexes must actually exist. The first
//! version indexed `rowid`, which SQLite refuses ("no such column: rowid"),
//! and the `let _ =` migration line swallowed the error — a silent no-op
//! found by Redline's real-DB attach test, which counts schema objects.
//! Every `ALTER`/`CREATE` in `Migration::additive` is `let _ =` on purpose
//! (idempotence), so this is the test that keeps those five honest.

#[test]
fn the_five_unscoped_partial_indexes_exist_after_migration() {
    let store = polis_store::PolisStore::open_in_memory().expect("in-memory store");
    let conn = store.conn();
    let mut stmt = conn
        .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%_unscoped' ORDER BY name")
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let names: Vec<&str> = rows.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "idx_browse_events_unscoped",
            "idx_class_nodes_unscoped",
            "idx_class_observations_unscoped",
            "idx_prompts_unscoped",
            "idx_user_notes_unscoped",
        ],
        "a partial index the migration silently failed to create"
    );
    for (name, sql) in &rows {
        assert!(sql.contains("WHERE principal_id IS NULL"), "{name} lost its partial predicate: {sql}");
    }
}
