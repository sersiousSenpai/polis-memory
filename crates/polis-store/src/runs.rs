// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The gardener's runs, reversible (Session B2; docs/ledger.md "Reversibility").
//!
//! Every op a run applies leaves a row in `class_run_ops` carrying exactly the
//! rows its inverse needs (`pre_image`, deflated JSON, sha256 in `pre_hash`).
//! Destructive ops never delete: they stamp `retired_by_run` on the row, every
//! reader filters the mark, and [`PolisStore::revert_run`] walks a run's ops
//! newest-first in one `BEGIN IMMEDIATE`, restoring each from its image. The
//! chain only ever grows — a revert appends `gardener_revert`; the run's own
//! events stay as the record of what was done and then undone.
//! [`PolisStore::vacuum_retired`] physically deletes retired rows past the
//! revert horizon; a run behind the horizon can no longer be reverted, and
//! says so.
//!
//! Pre-image shapes, per op (camelCase JSON; the writer is the apply path in
//! `catalog.rs` / `compaction.rs` / `observations.rs`, the reader is
//! [`PolisStore::revert_run`] — keep the two in step):
//!
//! | op | pre-image | inverse |
//! |---|---|---|
//! | `file` | `{linkId, nodeId, targetKind, targetId, revivedFromRun}` | delete the link (or re-mark it with the run it was revived from) |
//! | `create` | `{nodeId, parentId, title}` | retire the node |
//! | `promote` | `{nodeId, oldParent, newParent}` | restore the parent |
//! | `split` | `{nodeId, parentId, created: [{id, title, linkIds}]}` | move the links back, retire the created nodes |
//! | `merge` | `{target, oldTitle, oldParent, absorbed: [{id, parentId, title}], movedLinks: [{linkId, from}], leftoverLinks, children: [{id, oldParent}], observations}` | restore title/parent, move links back, un-retire the absorbed and their leftovers, re-parent the children |
//! | `collapse` | `{nodeId, digestId, retiredNodes, retiredLinks, retiredObservations, citationLinks}` | retire the digest + its citations, clear the subtree's marks |
//! | `supersede` | `{oldSeq, newSeq, eventSeq}` | delete the plain `supersessions` row |
//! | `compact` | `{promptId, eventSeq}` | `restore_prompt_body` (hash-verified against the archive) |
//! | `observe` | `{observationId, nodeId, eventSeq}` | retire the row |

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use polis_core::types::{ClassRun, RevertReceipt, RunOpView};

use crate::compaction::{deflate_body, inflate_body};
use crate::PolisStore;

/// The subject vocabulary. A later run whose subjects overlap an earlier
/// run's blocks reverting the earlier one.
pub mod subject {
    pub fn node(id: &str) -> String {
        format!("node:{id}")
    }
    pub fn link(id: i64) -> String {
        format!("link:{id}")
    }
    pub fn obs(id: i64) -> String {
        format!("obs:{id}")
    }
    pub fn prompt(id: i64) -> String {
        format!("prompt:{id}")
    }
    pub fn seq(n: i64) -> String {
        format!("seq:{n}")
    }
}

/// Run modes (`class_runs.mode`).
pub const MODE_ORGANIZE: &str = "organize";
pub const MODE_COMPACTION: &str = "compaction";
pub const MODE_OBSERVATIONS: &str = "observations";
pub const MODE_REVERT: &str = "revert";
pub const MODE_CURATION: &str = "curation";

// --- B3: the proposal work queue's policy (plan §5.1) --------------------
/// A proposal the verifier could not adjudicate is retried after this many
/// runs, by attempt: 1, then 2, then 4.
pub const PROPOSAL_BACKOFF_RUNS: [i64; 3] = [1, 2, 4];
/// After this many attempts a proposal is dropped as `expired`.
pub const PROPOSAL_TTL_ATTEMPTS: i64 = 3;
/// A proposal older than this on the LAKE's clock (newest event `ts` at
/// staging + seven lake-days) is dropped as `expired`.
pub const PROPOSAL_TTL_LAKE_MS: i64 = 7 * 24 * 3600 * 1000;

/// `polis_meta` key: the newest run id whose retired rows were vacuumed —
/// runs at or below it can no longer be reverted.
pub const REVERT_HORIZON_KEY: &str = "polis.revert.horizonRun";

/// How many runs back a revert stays possible (the plan's `older_than_runs`).
pub const REVERT_HORIZON_RUNS: i64 = 50;

/// One op to journal.
pub struct OpRecord<'a> {
    pub op: &'a str,
    pub subject_ids: Vec<String>,
    pub pre_image: Option<Value>,
    pub post_image: Option<Value>,
    pub ledger_seq: Option<i64>,
    /// `applied | refused | expired`.
    pub outcome: &'a str,
    pub reason: Option<String>,
}

impl<'a> OpRecord<'a> {
    pub fn applied(op: &'a str, subject_ids: Vec<String>, pre_image: Value) -> Self {
        Self { op, subject_ids, pre_image: Some(pre_image), post_image: None, ledger_seq: None, outcome: "applied", reason: None }
    }
    pub fn with_ledger_seq(mut self, seq: Option<i64>) -> Self {
        self.ledger_seq = seq;
        self
    }
    pub fn with_post_image(mut self, post: Value) -> Self {
        self.post_image = Some(post);
        self
    }
    /// An op the adjudicator refused (B3): journaled with its reason, no
    /// image — nothing changed, and today's silent reject has a record.
    pub fn refused(op: &'a str, subject_ids: Vec<String>, reason: impl Into<String>) -> Self {
        Self { op, subject_ids, pre_image: None, post_image: None, ledger_seq: None, outcome: "refused", reason: Some(reason.into()) }
    }
    /// An op that ran out of retries or lake-days in the queue (B3).
    pub fn expired(op: &'a str, subject_ids: Vec<String>, reason: impl Into<String>) -> Self {
        Self { op, subject_ids, pre_image: None, post_image: None, ledger_seq: None, outcome: "expired", reason: Some(reason.into()) }
    }
}

/// What `revert_run` decided.
#[derive(Debug, Clone, PartialEq)]
pub enum RevertOutcome {
    Reverted(RevertReceipt),
    /// A guardrail verdict, never a fault: a later run touched the subjects,
    /// the run is past the horizon, it was already reverted, or an op has no
    /// image to restore from.
    Rejected(String),
}

/// What `vacuum_retired` deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VacuumReport {
    pub horizon_run: i64,
    pub nodes: usize,
    pub links: usize,
    pub observations: usize,
    /// Journal rows whose images were dropped.
    pub images: usize,
}

/// The rows a `retire_node_subtree` marked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetiredSet {
    pub nodes: Vec<String>,
    pub links: Vec<i64>,
    pub observations: Vec<i64>,
}

fn deflate_json(v: &Value) -> (Vec<u8>, String) {
    let text = v.to_string();
    let hash = polis_core::ledger::sha256_hex(text.as_bytes());
    let blob = deflate_body(&text).unwrap_or_else(|_| text.as_bytes().to_vec());
    (blob, hash)
}

fn inflate_json(blob: &[u8]) -> Option<Value> {
    let text = inflate_body(blob).ok()?;
    serde_json::from_str(&text).ok()
}

fn ids_of(v: &Value, key: &str) -> Vec<i64> {
    v.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect()).unwrap_or_default()
}

fn strs_of(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
        .unwrap_or_default()
}

fn str_of<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
    v.get(key).and_then(Value::as_str)
}

impl PolisStore {
    /// Journal one op under `run_id`; returns its `op_ix`. Under an already
    /// held lock — the apply paths call it inside their own statements.
    pub fn journal_op_locked(conn: &Connection, run_id: i64, rec: &OpRecord<'_>) -> rusqlite::Result<i64> {
        let ix: i64 = conn.query_row(
            "SELECT COALESCE(MAX(op_ix), 0) + 1 FROM class_run_ops WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )?;
        let subjects = serde_json::to_string(&rec.subject_ids).unwrap_or_else(|_| "[]".into());
        let (pre_blob, pre_hash) = match &rec.pre_image {
            Some(v) => {
                let (b, h) = deflate_json(v);
                (Some(b), Some(h))
            }
            None => (None, None),
        };
        let post_blob = rec.post_image.as_ref().map(|v| deflate_json(v).0);
        conn.execute(
            "INSERT INTO class_run_ops
                (run_id, op_ix, op, subject_ids, outcome, reason, pre_image, pre_hash,
                 post_image, ledger_seq, reverted_by_run)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
            params![run_id, ix, rec.op, subjects, rec.outcome, rec.reason, pre_blob, pre_hash, post_blob, rec.ledger_seq],
        )?;
        Ok(ix)
    }

    /// `journal_op_locked` under the store's own lock.
    pub fn journal_op(&self, run_id: i64, rec: &OpRecord<'_>) -> rusqlite::Result<i64> {
        let conn = self.conn();
        Self::journal_op_locked(&conn, run_id, rec)
    }

    /// A run row of the given mode, `running`.
    pub fn insert_class_run_with(&self, mode: &str, seq_from: Option<i64>, seq_to: Option<i64>) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO class_runs (started_at, status, seq_from, seq_to, mode)
             VALUES (?1, 'running', ?2, ?3, ?4)",
            params![polis_core::ledger::now_millis(), seq_from, seq_to, mode],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn get_class_run(&self, id: i64) -> rusqlite::Result<Option<ClassRun>> {
        let conn = self.conn();
        conn.query_row(
            &format!("SELECT {} FROM class_runs WHERE id = ?1", Self::CLASS_RUN_COLS),
            params![id],
            Self::row_to_class_run,
        )
        .optional()
    }

    /// A run's journal, oldest op first, without the image blobs.
    pub fn list_run_ops(&self, run_id: i64) -> rusqlite::Result<Vec<RunOpView>> {
        let conn = self.conn();
        Self::list_run_ops_locked(&conn, run_id)
    }

    pub fn list_run_ops_locked(conn: &Connection, run_id: i64) -> rusqlite::Result<Vec<RunOpView>> {
        let mut stmt = conn.prepare(
            "SELECT run_id, op_ix, op, subject_ids, outcome, reason, pre_hash, ledger_seq, reverted_by_run
             FROM class_run_ops WHERE run_id = ?1 ORDER BY op_ix ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| {
            Ok(RunOpView {
                run_id: r.get(0)?,
                op_ix: r.get(1)?,
                op: r.get(2)?,
                subject_ids: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                outcome: r.get(4)?,
                reason: r.get(5)?,
                pre_hash: r.get(6)?,
                ledger_seq: r.get(7)?,
                reverted_by_run: r.get(8)?,
            })
        })?;
        rows.collect()
    }

    /// The vacuum horizon, if a vacuum has run.
    pub fn revert_horizon(&self) -> rusqlite::Result<Option<i64>> {
        let conn = self.conn();
        Ok(crate::meta::get(&conn, REVERT_HORIZON_KEY)?.and_then(|v| v.parse().ok()))
    }

    /// Retire a node and its whole subtree (nodes, their links, their
    /// observations) under `run_id` — the marked replacement for the old
    /// delete. `into` names where the sourcing went (a digest, a merge
    /// target). Returns exactly what was marked, for the journal.
    pub fn retire_node_subtree(
        conn: &Connection,
        id: &str,
        run_id: i64,
        into: Option<&str>,
    ) -> rusqlite::Result<RetiredSet> {
        let mut stack = vec![id.to_string()];
        let mut all = Vec::new();
        let mut guard = 0;
        while let Some(nid) = stack.pop() {
            guard += 1;
            if guard > 10_000 {
                break;
            }
            all.push(nid.clone());
            let mut stmt = conn.prepare("SELECT id FROM class_nodes WHERE parent_id = ?1 AND retired_by_run IS NULL")?;
            let kids: Vec<String> = stmt.query_map(params![nid], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<_>>()?;
            stack.extend(kids);
        }
        let mut out = RetiredSet::default();
        for nid in &all {
            let mut stmt = conn.prepare("SELECT id FROM class_links WHERE node_id = ?1 AND retired_by_run IS NULL")?;
            let links: Vec<i64> = stmt.query_map(params![nid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            for l in &links {
                conn.execute("UPDATE class_links SET retired_by_run = ?2 WHERE id = ?1", params![l, run_id])?;
            }
            out.links.extend(links);
            let mut stmt = conn.prepare("SELECT id FROM class_observations WHERE node_id = ?1 AND retired_by_run IS NULL")?;
            let obs: Vec<i64> = stmt.query_map(params![nid], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            drop(stmt);
            for o in &obs {
                conn.execute("UPDATE class_observations SET retired_by_run = ?2 WHERE id = ?1", params![o, run_id])?;
            }
            out.observations.extend(obs);
            conn.execute(
                "UPDATE class_nodes SET retired_by_run = ?2, retired_into = ?3 WHERE id = ?1 AND retired_by_run IS NULL",
                params![nid, run_id, into],
            )?;
            out.nodes.push(nid.clone());
        }
        Ok(out)
    }

    /// Clear the marks one run made on the given rows.
    fn unretire_locked(conn: &Connection, run_id: i64, set: &RetiredSet) -> rusqlite::Result<()> {
        for nid in &set.nodes {
            conn.execute(
                "UPDATE class_nodes SET retired_by_run = NULL, retired_into = NULL WHERE id = ?1 AND retired_by_run = ?2",
                params![nid, run_id],
            )?;
        }
        for l in &set.links {
            conn.execute("UPDATE class_links SET retired_by_run = NULL WHERE id = ?1 AND retired_by_run = ?2", params![l, run_id])?;
        }
        for o in &set.observations {
            conn.execute(
                "UPDATE class_observations SET retired_by_run = NULL WHERE id = ?1 AND retired_by_run = ?2",
                params![o, run_id],
            )?;
        }
        Ok(())
    }

    /// Undo one run (§5.2). One `BEGIN IMMEDIATE`; ops walked `op_ix`
    /// descending; each restored from its pre-image; `gardener_revert`
    /// appended; the run and its ops marked `reverted`. Refuses — as data,
    /// never a fault — when a later run touched the same subjects ("revert
    /// run N first"), when the run is past the vacuum horizon, when it was
    /// already reverted, or when an op cannot be inverted (a compacted body
    /// with no archive).
    pub fn revert_run(&self, run_id: i64, actor: &str) -> rusqlite::Result<RevertOutcome> {
        let conn = self.conn();
        let started = std::time::Instant::now();
        let Some(run) = conn
            .query_row("SELECT outcome FROM class_runs WHERE id = ?1", params![run_id], |r| r.get::<_, Option<String>>(0))
            .optional()?
        else {
            return Ok(RevertOutcome::Rejected(format!("no run #{run_id}")));
        };
        if matches!(run.as_deref(), Some("reverted") | Some("reverted_by_canary")) {
            return Ok(RevertOutcome::Rejected(format!("run #{run_id} was already reverted")));
        }
        if let Some(h) = crate::meta::get(&conn, REVERT_HORIZON_KEY)?.and_then(|v| v.parse::<i64>().ok()) {
            if run_id <= h {
                return Ok(RevertOutcome::Rejected(format!(
                    "run #{run_id} is past the revert horizon (retired rows vacuumed through run #{h})"
                )));
            }
        }
        let ops = Self::list_run_ops_locked(&conn, run_id)?;
        let applied: Vec<&RunOpView> = ops.iter().filter(|o| o.outcome == "applied").collect();
        if applied.is_empty() {
            return Ok(RevertOutcome::Rejected(format!("run #{run_id} has nothing to revert")));
        }
        // Precondition: no later run's applied ops share a subject.
        let mine: std::collections::HashSet<&str> = applied.iter().flat_map(|o| o.subject_ids.iter().map(String::as_str)).collect();
        let mut stmt = conn.prepare(
            "SELECT run_id, subject_ids FROM class_run_ops WHERE run_id > ?1 AND outcome = 'applied' ORDER BY run_id DESC",
        )?;
        let later: Vec<(i64, String)> = stmt.query_map(params![run_id], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        for (k, subjects) in later {
            let subs: Vec<String> = serde_json::from_str(&subjects).unwrap_or_default();
            if subs.iter().any(|s| mine.contains(s.as_str())) {
                return Ok(RevertOutcome::Rejected(format!("revert run #{k} first — it touched what run #{run_id} touched")));
            }
        }

        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        let by_run: i64 = {
            tx.execute(
                "INSERT INTO class_runs (started_at, status, mode) VALUES (?1, 'running', ?2)",
                params![polis_core::ledger::now_millis(), MODE_REVERT],
            )?;
            tx.last_insert_rowid()
        };
        let mut reverted = 0i64;
        for op in applied.iter().rev() {
            let blob: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT pre_image FROM class_run_ops WHERE run_id = ?1 AND op_ix = ?2",
                    params![run_id, op.op_ix],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let pre = blob.as_deref().and_then(inflate_json);
            if let Err(reason) = Self::invert_op_locked(&tx, run_id, by_run, &op.op, pre.as_ref()) {
                tx.rollback()?;
                return Ok(RevertOutcome::Rejected(format!("op {} ({}) of run #{run_id}: {reason}", op.op_ix, op.op)));
            }
            tx.execute(
                "UPDATE class_run_ops SET outcome = 'reverted', reverted_by_run = ?3 WHERE run_id = ?1 AND op_ix = ?2",
                params![run_id, op.op_ix, by_run],
            )?;
            reverted += 1;
        }
        let ts = polis_core::ledger::now_millis();
        let run_str = run_id.to_string();
        let ops_str = reverted.to_string();
        let by_str = by_run.to_string();
        let ph = polis_core::ledger::decision_payload_hash(&[("run", &run_str), ("ops_reverted", &ops_str), ("by_run", &by_str)]);
        let author = actor.to_string();
        let ev = crate::ledger::append_event(
            &tx,
            &polis_core::ledger::LedgerAppend {
                kind: polis_core::ledger::EventKind::GardenerRevert.as_str(),
                author: &author,
                ts,
                prompt_id: None,
                session_id: None,
                version_number: None,
                ref_kind: Some("class_run"),
                ref_id: Some(run_str.as_str()),
                payload_hash: &ph,
            },
        )?;
        tx.execute("UPDATE class_runs SET outcome = 'reverted' WHERE id = ?1", params![run_id])?;
        tx.execute(
            "UPDATE class_runs SET status = 'done', finished_at = ?2, outcome = 'done', ops = ?3,
                    summary = ?4, wall_ms = ?5, duration_ms = ?5
             WHERE id = ?1",
            params![by_run, ts, reverted, format!("reverted run #{run_id}: {reverted} op(s)"), started.elapsed().as_millis() as i64],
        )?;
        tx.commit()?;
        Ok(RevertOutcome::Reverted(RevertReceipt { run_id, reverted_ops: reverted, event_seq: ev.seq, revert_run_id: by_run }))
    }

    /// The inverse of one op, from its pre-image. `Err(reason)` aborts the
    /// whole revert (the caller rolls back).
    fn invert_op_locked(conn: &Connection, run_id: i64, by_run: i64, op: &str, pre: Option<&Value>) -> Result<(), String> {
        let pre = pre.ok_or_else(|| "no pre-image (vacuumed or never journaled)".to_string())?;
        let db = |e: rusqlite::Error| e.to_string();
        match op {
            "file" => {
                let link = pre.get("linkId").and_then(Value::as_i64).ok_or("pre-image lacks linkId")?;
                match pre.get("revivedFromRun").and_then(Value::as_i64) {
                    Some(r) => {
                        conn.execute("UPDATE class_links SET retired_by_run = ?2 WHERE id = ?1", params![link, r]).map_err(db)?;
                    }
                    None => {
                        conn.execute("DELETE FROM class_links WHERE id = ?1", params![link]).map_err(db)?;
                    }
                }
            }
            "create" => {
                let node = str_of(pre, "nodeId").ok_or("pre-image lacks nodeId")?;
                conn.execute(
                    "UPDATE class_nodes SET retired_by_run = ?2 WHERE id = ?1 AND retired_by_run IS NULL",
                    params![node, by_run],
                )
                .map_err(db)?;
            }
            "promote" => {
                let node = str_of(pre, "nodeId").ok_or("pre-image lacks nodeId")?;
                let old_parent = str_of(pre, "oldParent");
                conn.execute("UPDATE class_nodes SET parent_id = ?2 WHERE id = ?1", params![node, old_parent]).map_err(db)?;
            }
            "split" => {
                let node = str_of(pre, "nodeId").ok_or("pre-image lacks nodeId")?;
                for c in pre.get("created").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let cid = str_of(&c, "id").ok_or("split pre-image lacks a created id")?;
                    for l in ids_of(&c, "linkIds") {
                        conn.execute(
                            "UPDATE class_links SET node_id = ?2 WHERE id = ?1 AND node_id = ?3",
                            params![l, node, cid],
                        )
                        .map_err(db)?;
                    }
                    conn.execute(
                        "UPDATE class_nodes SET retired_by_run = ?2 WHERE id = ?1 AND retired_by_run IS NULL",
                        params![cid, by_run],
                    )
                    .map_err(db)?;
                }
            }
            "merge" => {
                let target = str_of(pre, "target").ok_or("pre-image lacks target")?;
                if let Some(t) = str_of(pre, "oldTitle") {
                    conn.execute("UPDATE class_nodes SET title = ?2 WHERE id = ?1", params![target, t]).map_err(db)?;
                }
                if pre.get("oldParent").is_some() {
                    conn.execute("UPDATE class_nodes SET parent_id = ?2 WHERE id = ?1", params![target, str_of(pre, "oldParent")]).map_err(db)?;
                }
                for m in pre.get("movedLinks").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let (Some(l), Some(from)) = (m.get("linkId").and_then(Value::as_i64), str_of(&m, "from")) else { continue };
                    conn.execute("UPDATE class_links SET node_id = ?2 WHERE id = ?1", params![l, from]).map_err(db)?;
                }
                for c in pre.get("children").and_then(Value::as_array).cloned().unwrap_or_default() {
                    let Some(cid) = str_of(&c, "id") else { continue };
                    conn.execute("UPDATE class_nodes SET parent_id = ?2 WHERE id = ?1", params![cid, str_of(&c, "oldParent")]).map_err(db)?;
                }
                let absorbed: Vec<String> = pre
                    .get("absorbed")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|x| str_of(x, "id").map(String::from)).collect())
                    .unwrap_or_default();
                let set = RetiredSet { nodes: absorbed, links: ids_of(pre, "leftoverLinks"), observations: ids_of(pre, "observations") };
                Self::unretire_locked(conn, run_id, &set).map_err(db)?;
            }
            "collapse" => {
                let digest = str_of(pre, "digestId").ok_or("pre-image lacks digestId")?;
                conn.execute(
                    "UPDATE class_nodes SET retired_by_run = ?2 WHERE id = ?1 AND retired_by_run IS NULL",
                    params![digest, by_run],
                )
                .map_err(db)?;
                for l in ids_of(pre, "citationLinks") {
                    conn.execute(
                        "UPDATE class_links SET retired_by_run = ?2 WHERE id = ?1 AND retired_by_run IS NULL",
                        params![l, by_run],
                    )
                    .map_err(db)?;
                }
                let set = RetiredSet {
                    nodes: strs_of(pre, "retiredNodes"),
                    links: ids_of(pre, "retiredLinks"),
                    observations: ids_of(pre, "retiredObservations"),
                };
                Self::unretire_locked(conn, run_id, &set).map_err(db)?;
            }
            "supersede" => {
                let old = pre.get("oldSeq").and_then(Value::as_i64).ok_or("pre-image lacks oldSeq")?;
                let ev = pre.get("eventSeq").and_then(Value::as_i64).ok_or("pre-image lacks eventSeq")?;
                conn.execute("DELETE FROM supersessions WHERE old_seq = ?1 AND event_seq = ?2", params![old, ev]).map_err(db)?;
            }
            "compact" => {
                let id = pre.get("promptId").and_then(Value::as_i64).ok_or("pre-image lacks promptId")?;
                let restored = Self::restore_prompt_body_locked(conn, id).map_err(db)?;
                if !restored {
                    return Err(format!("prompt {id} has no archived body to restore (forgotten, or compacted before the archive existed)"));
                }
            }
            "observe" => {
                let id = pre.get("observationId").and_then(Value::as_i64).ok_or("pre-image lacks observationId")?;
                conn.execute(
                    "UPDATE class_observations SET retired_by_run = ?2 WHERE id = ?1 AND retired_by_run IS NULL",
                    params![id, by_run],
                )
                .map_err(db)?;
            }
            other => return Err(format!("unknown op `{other}`")),
        }
        Ok(())
    }

    /// Physically delete retired rows older than the revert horizon (the
    /// newest run id minus `older_than_runs`) and drop the images of the
    /// journal rows behind it; record the horizon so `revert_run` refuses
    /// those runs by name. The gardener calls this — never a reader.
    pub fn vacuum_retired(&self, older_than_runs: i64) -> rusqlite::Result<VacuumReport> {
        let conn = self.conn();
        let max_run: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM class_runs", [], |r| r.get(0))?;
        let horizon = max_run - older_than_runs.max(0);
        let mut report = VacuumReport { horizon_run: horizon, ..Default::default() };
        if horizon <= 0 {
            return Ok(report);
        }
        let prior: i64 = crate::meta::get(&conn, REVERT_HORIZON_KEY)?.and_then(|v| v.parse().ok()).unwrap_or(0);
        if horizon <= prior {
            report.horizon_run = prior;
            return Ok(report);
        }
        let tx = rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        report.links = tx.execute("DELETE FROM class_links WHERE retired_by_run IS NOT NULL AND retired_by_run <= ?1", params![horizon])?;
        report.observations =
            tx.execute("DELETE FROM class_observations WHERE retired_by_run IS NOT NULL AND retired_by_run <= ?1", params![horizon])?;
        report.nodes = tx.execute("DELETE FROM class_nodes WHERE retired_by_run IS NOT NULL AND retired_by_run <= ?1", params![horizon])?;
        report.images = tx.execute(
            "UPDATE class_run_ops SET pre_image = NULL, post_image = NULL
             WHERE run_id <= ?1 AND (pre_image IS NOT NULL OR post_image IS NOT NULL)",
            params![horizon],
        )?;
        crate::meta::set(&tx, REVERT_HORIZON_KEY, &horizon.to_string())?;
        tx.commit()?;
        Ok(report)
    }
}

/// The pre-image builders the apply paths use (kept beside the reader so the
/// two shapes stay one).
pub mod image {
    use serde_json::{json, Value};

    pub fn file(link_id: i64, node_id: &str, target_kind: &str, target_id: &str, revived_from: Option<i64>) -> Value {
        json!({ "linkId": link_id, "nodeId": node_id, "targetKind": target_kind, "targetId": target_id, "revivedFromRun": revived_from })
    }
    pub fn create(node_id: &str, parent_id: Option<&str>, title: &str) -> Value {
        json!({ "nodeId": node_id, "parentId": parent_id, "title": title })
    }
    pub fn promote(node_id: &str, old_parent: Option<&str>, new_parent: Option<&str>) -> Value {
        json!({ "nodeId": node_id, "oldParent": old_parent, "newParent": new_parent })
    }
    pub fn supersede(old_seq: i64, new_seq: i64, event_seq: i64) -> Value {
        json!({ "oldSeq": old_seq, "newSeq": new_seq, "eventSeq": event_seq })
    }
    pub fn compact(prompt_id: i64, event_seq: i64) -> Value {
        json!({ "promptId": prompt_id, "eventSeq": event_seq })
    }
    pub fn observe(observation_id: i64, node_id: &str, event_seq: i64) -> Value {
        json!({ "observationId": observation_id, "nodeId": node_id, "eventSeq": event_seq })
    }
}


impl PolisStore {
    /// The canary reverted this run (B3 §5.3): stamp the outcome, the recall
    /// pair and the frozen verdict, and RELEASE the run's seq window — a
    /// reverted organize consumed nothing, so the next delta starts where the
    /// previous good run ended. The revert itself (marks, journal, the
    /// `gardener_revert` event) is `revert_run`'s; this is the bookkeeping
    /// beside it.
    pub fn mark_run_canary_reverted(
        &self,
        run_id: i64,
        before: f64,
        after: f64,
        canary_json: &str,
    ) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE class_runs
             SET outcome = 'reverted_by_canary', canary_before = ?2, canary_after = ?3, canary_json = ?4,
                 seq_from = NULL, seq_to = NULL
             WHERE id = ?1",
            params![run_id, before, after, canary_json],
        )?;
        Ok(())
    }

    /// Stamp a finished run's canary pair + verdict without reverting it.
    pub fn set_run_canary_json(&self, run_id: i64, before: f64, after: f64, canary_json: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE class_runs SET canary_before = ?2, canary_after = ?3, canary_json = ?4 WHERE id = ?1",
            params![run_id, before, after, canary_json],
        )?;
        Ok(())
    }
}
