// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Agent-written pattern statements over a node's items — derived, never ground truth.
//!
//! Lifted byte-for-byte from Redline's `Database` in Session A3 of the Polis
//! extraction; only `crate::` paths changed.

#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

#[allow(unused_imports)]
use rusqlite::{params, Connection, OptionalExtension, Row};

#[allow(unused_imports)]
use polis_core::types::*;
#[allow(unused_imports)]
use polis_core::{proposal::Proposal, query::MatchStage};
#[allow(unused_imports)]
use crate::PolisStore;
#[allow(unused_imports)]
use crate::prompts::PROMPT_TEXT;
#[allow(unused_imports)]
use crate::search::{GrepError, GREP_MIN_LITERAL};

impl PolisStore {
    /// Insert an observation + append its `observation` ledger event,
    /// atomically. Dedup on (node_id, summary) regardless of retirement —
    /// a retired pattern never resurfaces under the same wording. Returns
    /// the new row id, `None` when skipped. Empty cite_seqs is rejected here
    /// too (defense in depth behind the strict parser).
    pub fn insert_class_observation(
        &self,
        node_id: &str,
        summary: &str,
        cite_seqs: &[i64],
        actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        let conn = self.conn();
        Self::insert_class_observation_locked(&conn, node_id, summary, cite_seqs, actor)
    }

    /// `insert_class_observation` journaled under a gardener run (B2): the
    /// `observe` op's inverse retires the row.
    pub fn insert_class_observation_in_run(
        &self,
        run_id: i64,
        node_id: &str,
        summary: &str,
        cite_seqs: &[i64],
        actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        let conn = self.conn();
        let row = Self::insert_class_observation_locked(&conn, node_id, summary, cite_seqs, actor)?;
        if let Some(id) = row {
            let seq: Option<i64> = conn
                .query_row("SELECT created_seq FROM class_observations WHERE id = ?1", params![id], |r| r.get(0))
                .optional()?
                .flatten();
            Self::journal_op_locked(
                &conn,
                run_id,
                &crate::runs::OpRecord::applied(
                    "observe",
                    vec![crate::runs::subject::obs(id), crate::runs::subject::node(node_id)],
                    crate::runs::image::observe(id, node_id, seq.unwrap_or(0)),
                )
                .with_ledger_seq(seq),
            )?;
        }
        Ok(row)
    }

    /// The core, under an already-held lock.
    pub fn insert_class_observation_locked(
        conn: &rusqlite::Connection,
        node_id: &str,
        summary: &str,
        cite_seqs: &[i64],
        actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        if cite_seqs.is_empty() || summary.trim().is_empty() {
            return Ok(None);
        }
        let node_exists: i64 = conn.query_row(
            "SELECT COUNT(*) FROM class_nodes WHERE id = ?1 AND retired_by_run IS NULL",
            params![node_id],
            |r| r.get(0),
        )?;
        if node_exists == 0 {
            return Ok(None);
        }
        let dup: i64 = conn.query_row(
            "SELECT COUNT(*) FROM class_observations WHERE node_id = ?1 AND summary = ?2",
            params![node_id, summary],
            |r| r.get(0),
        )?;
        if dup > 0 {
            return Ok(None);
        }
        let cites = serde_json::to_string(cite_seqs).unwrap_or_else(|_| "[]".into());
        let now = polis_core::ledger::now_millis();
        conn.execute(
            "INSERT INTO class_observations
                (node_id, summary, cite_seqs, created_seq, pinned, dismissed, created_at)
             VALUES (?1, ?2, ?3, NULL, 0, 0, ?4)",
            params![node_id, summary, cites, now],
        )?;
        let row_id = conn.last_insert_rowid();
        // Field order is frozen — it is the payload-hash identity.
        let ph = polis_core::ledger::decision_payload_hash(&[
            ("node", node_id),
            ("summary", summary),
            ("cites", &cites),
        ]);
        let author = actor.to_string();
        let ev = Self::append_ledger_event_locked(
            conn,
            &polis_core::ledger::LedgerAppend {
                kind: polis_core::ledger::EventKind::Observation.as_str(),
                author: &author,
                ts: now,
                prompt_id: None,
                session_id: None,
                version_number: None,
                ref_kind: Some("class_node"),
                ref_id: Some(node_id),
                payload_hash: &ph,
            },
        )?;
        conn.execute(
            "UPDATE class_observations SET created_seq = ?2 WHERE id = ?1",
            params![row_id, ev.seq],
        )?;
        Ok(Some(row_id))
    }

    pub const OBSERVATION_COLS: &'static str = "id, node_id, summary, cite_seqs, created_seq, pinned, dismissed, created_at,
                    retired_at, retired_reason";

    pub fn row_to_observation(r: &rusqlite::Row) -> rusqlite::Result<polis_core::types::ClassObservation> {
        Ok(polis_core::types::ClassObservation {
            id: r.get(0)?,
            node_id: r.get(1)?,
            summary: r.get(2)?,
            cite_seqs: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            created_seq: r.get(4)?,
            pinned: r.get::<_, i64>(5)? != 0,
            dismissed: r.get::<_, i64>(6)? != 0,
            created_at: r.get(7)?,
            retired_at: r.get(8)?,
            retired_reason: r.get(9)?,
        })
    }

    /// A node's LIVE observations, newest first. `_include_dismissed` is
    /// ignored since B3: there is no dismiss — a pattern the pass no longer
    /// stands behind is RETIRED (`retired_at`), and retired rows are not
    /// served. Kept in the signature so callers did not have to move.
    pub fn list_class_observations(
        &self,
        node_id: &str,
        _include_dismissed: bool,
    ) -> rusqlite::Result<Vec<polis_core::types::ClassObservation>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM class_observations
             WHERE node_id = ?1 AND retired_by_run IS NULL AND retired_at IS NULL
             ORDER BY created_at DESC, id DESC",
            Self::OBSERVATION_COLS
        ))?;
        let rows = stmt.query_map(params![node_id], Self::row_to_observation)?;
        rows.collect()
    }

    /// Every live observation, oldest first — the re-validation pass's input
    /// and the health report's counts.
    pub fn list_live_observations(&self) -> rusqlite::Result<Vec<polis_core::types::ClassObservation>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM class_observations
             WHERE retired_by_run IS NULL AND retired_at IS NULL
             ORDER BY id ASC",
            Self::OBSERVATION_COLS
        ))?;
        let rows = stmt.query_map([], Self::row_to_observation)?;
        rows.collect()
    }

    /// Retire an observation (B3 re-validation): the pass returned `retire`,
    /// a cited seq was forgotten, or its node retired. A mark plus a
    /// compensating `observation` event (`action=retire`) — the row stays so
    /// the dedup guard keeps holding and the history stays readable. Returns
    /// the node id, `None` when the row is unknown or already retired.
    pub fn retire_observation(&self, id: i64, reason: &str, actor: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn();
        let node: Option<String> = conn
            .query_row(
                "SELECT node_id FROM class_observations
                 WHERE id = ?1 AND retired_at IS NULL AND retired_by_run IS NULL",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(node_id) = node else { return Ok(None) };
        let now = polis_core::ledger::now_millis();
        conn.execute(
            "UPDATE class_observations SET retired_at = ?2, retired_reason = ?3 WHERE id = ?1",
            params![id, now, reason],
        )?;
        let id_s = id.to_string();
        let ph = polis_core::ledger::decision_payload_hash(&[
            ("action", "retire"),
            ("observation", &id_s),
            ("node", &node_id),
            ("reason", reason),
        ]);
        let author = actor.to_string();
        Self::append_ledger_event_locked(
            &conn,
            &polis_core::ledger::LedgerAppend {
                kind: polis_core::ledger::EventKind::Observation.as_str(),
                author: &author,
                ts: now,
                prompt_id: None,
                session_id: None,
                version_number: None,
                ref_kind: Some("class_node"),
                ref_id: Some(&node_id),
                payload_hash: &ph,
            },
        )?;
        Ok(Some(node_id))
    }

    /// The seqs an observation cites that no longer stand (B3's deterministic
    /// retirement): a prompt whose body was forgotten, or a seq that is not
    /// in the ledger at all. Empty when every citation still holds.
    pub fn dead_citations(&self, cite_seqs: &[i64]) -> rusqlite::Result<Vec<i64>> {
        let conn = self.conn();
        let mut dead = Vec::new();
        for seq in cite_seqs {
            let row: Option<Option<String>> = conn
                .query_row(
                    "SELECT p.gist FROM ledger_events le
                     LEFT JOIN prompts p ON p.id = le.prompt_id
                     WHERE le.seq = ?1",
                    params![seq],
                    |r| r.get(0),
                )
                .optional()?;
            match row {
                None => dead.push(*seq),
                Some(Some(g)) if g == "[forgotten]" => dead.push(*seq),
                _ => {}
            }
        }
        Ok(dead)
    }

    /// Newest live observation timestamp per node — the keeper's
    /// freshness gate: a node whose newest observation postdates its last
    /// activity has nothing new to mine.
    pub fn newest_observation_per_node(
        &self,
    ) -> rusqlite::Result<std::collections::HashMap<String, i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT node_id, MAX(created_at) FROM class_observations
             WHERE retired_at IS NULL AND retired_by_run IS NULL GROUP BY node_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        rows.collect()
    }

    // `set_observation_dismissed` / `set_observation_pinned` are gone (B3,
    // plan §5.4): a pattern is retired by the pass's own re-validation, never
    // dismissed by a person, and nothing floats first by a pin.
}
