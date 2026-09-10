// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The class catalog: nodes, links, proposals, runs, the staging/apply machinery and the map's edge readers.
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

/// Fresh node id for a created/staged node.
pub fn new_node_id() -> String {
    format!("cn-{}", uuid::Uuid::new_v4().simple())
}

impl PolisStore {
    /// Drop one queued proposal, returning its op so the host can note what
    /// was refused. The row delete is the store's; the friction note that
    /// Redline records beside it is the host's (`Database::reject_class_proposal`).
    pub fn delete_class_proposal(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Option<String>> {
        let op: Option<String> = conn
            .query_row(
                "SELECT op FROM class_proposals WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        conn.execute("DELETE FROM class_proposals WHERE id = ?1", params![id])?;
        Ok(op)
    }
}

impl PolisStore {
    pub fn row_to_class_node(r: &rusqlite::Row) -> rusqlite::Result<polis_core::types::ClassNode> {
        Ok(polis_core::types::ClassNode {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            kind: r.get(2)?,
            title: r.get(3)?,
            summary: r.get(4)?,
            project_path: r.get(5)?,
            ip_name: r.get(6)?,
            status: r.get(7)?,
            pinned: r.get::<_, i64>(8)? != 0,
            curated_by: r.get(9)?,
            created_at: r.get(10)?,
            updated_at: r.get(11)?,
        })
    }
}

impl PolisStore {
    /// Seed one root per (id, title, project_path), idempotent — a root that
    /// already exists (by id) is left untouched. Live on creation (B3: there is
    /// no `proposed` state any more). Returns how many were newly seeded.
    pub fn seed_class_roots(
        &self,
        rows: &[(String, String, Option<String>)],
    ) -> rusqlite::Result<usize> {
        let conn = self.conn();
        let now = polis_core::ledger::now_millis();
        let mut seeded = 0;
        for (id, title, project) in rows {
            let changed = conn.execute(
                "INSERT INTO class_nodes
                    (id, parent_id, kind, title, summary, project_path, ip_name,
                     status, pinned, curated_by, created_at, updated_at)
                 VALUES (?1, NULL, 'node', ?2, NULL, ?3, NULL, 'accepted', 0,
                         'classifier', ?4, ?4)
                 ON CONFLICT(id) DO NOTHING",
                params![id, title, project, now],
            )?;
            seeded += changed;
        }
        Ok(seeded)
    }

    /// Every class node (proposed + accepted), for tree building in Rust.
    pub fn list_class_nodes(&self) -> rusqlite::Result<Vec<polis_core::types::ClassNode>> {
        self.list_class_nodes_scoped(&Default::default())
    }

    pub fn list_class_nodes_scoped(&self, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Vec<polis_core::types::ClassNode>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "n", scope)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, parent_id, kind, title, summary, project_path, ip_name,
                    status, pinned, curated_by, created_at, updated_at
             FROM class_nodes n WHERE retired_by_run IS NULL{} ORDER BY title ASC", scoped.sql
        ))?;
        let binds: Vec<&dyn rusqlite::ToSql> = scoped.binds.iter().map(|v| v.as_ref()).collect();
        let rows = stmt.query_map(binds.as_slice(), PolisStore::row_to_class_node)?;
        rows.collect()
    }

    pub fn get_class_node(&self, id: &str) -> rusqlite::Result<Option<polis_core::types::ClassNode>> {
        self.get_class_node_scoped(id, &Default::default())
    }

    pub fn get_class_node_scoped(&self, id: &str, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Option<polis_core::types::ClassNode>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "n", scope)?;
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&id];
        binds.extend(scoped.binds.iter().map(|v| v.as_ref()));
        conn.query_row(
            &format!("SELECT id, parent_id, kind, title, summary, project_path, ip_name,
                    status, pinned, curated_by, created_at, updated_at
             FROM class_nodes n WHERE id = ?1 AND retired_by_run IS NULL{}", scoped.sql),
            binds.as_slice(),
            PolisStore::row_to_class_node,
        )
        .optional()
    }

    /// The links on one node (accepted + proposed).
    pub fn list_class_links_for_node(
        &self,
        node_id: &str,
    ) -> rusqlite::Result<Vec<polis_core::types::ClassLink>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, node_id, target_kind, target_id, note, status, created_at
             FROM class_links WHERE node_id = ?1 AND retired_by_run IS NULL ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![node_id], |r| {
            Ok(polis_core::types::ClassLink {
                id: r.get(0)?,
                node_id: r.get(1)?,
                target_kind: r.get(2)?,
                target_id: r.get(3)?,
                note: r.get(4)?,
                status: r.get(5)?,
                created_at: r.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// One node's direct children, straight off `idx_class_nodes_parent`. The
    /// batched replacement for "read every class node, then filter in Rust" —
    /// the shape `build_node_view` used to pay on every descent.
    pub fn list_class_children(
        &self,
        parent_id: &str,
    ) -> rusqlite::Result<Vec<polis_core::types::ClassNode>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, kind, title, summary, project_path, ip_name,
                    status, pinned, curated_by, created_at, updated_at
             FROM class_nodes WHERE parent_id = ?1 AND retired_by_run IS NULL ORDER BY title ASC",
        )?;
        let rows = stmt.query_map(params![parent_id], PolisStore::row_to_class_node)?;
        rows.collect()
    }

    /// Children of MANY parents in one query — the batched form of
    /// `list_class_children`, same `link_previews_for_seqs` idiom.
    ///
    /// The answer pack fetched a node's grandchildren by calling the singular
    /// version once per child, so a node with 40 children took 40 round trips
    /// through the connection mutex to build a list the pack then truncates.
    /// One query, one lock acquisition.
    pub fn list_class_children_for_parents(
        &self,
        parent_ids: &[String],
    ) -> rusqlite::Result<Vec<polis_core::types::ClassNode>> {
        if parent_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn();
        let mut out = Vec::new();
        // Chunked so a very wide node can't build a statement past SQLite's
        // variable limit.
        for chunk in parent_ids.chunks(400) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, parent_id, kind, title, summary, project_path, ip_name,
                        status, pinned, curated_by, created_at, updated_at
                 FROM class_nodes WHERE parent_id IN ({marks}) AND retired_by_run IS NULL
                 ORDER BY parent_id ASC, title ASC"
            ))?;
            let refs: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(refs.as_slice(), PolisStore::row_to_class_node)?;
            for r in rows {
                out.push(r?);
            }
        }
        Ok(out)
    }

    pub fn list_class_children_scoped(&self, parent_id: &str, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Vec<polis_core::types::ClassNode>> {
        Ok(self.list_class_nodes_scoped(scope)?.into_iter().filter(|n| n.parent_id.as_deref() == Some(parent_id)).collect())
    }

    pub fn list_class_children_for_parents_scoped(&self, parents: &[String], scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Vec<polis_core::types::ClassNode>> {
        Ok(self.list_class_nodes_scoped(scope)?.into_iter().filter(|n| n.parent_id.as_ref().is_some_and(|id| parents.contains(id))).collect())
    }

    pub fn list_class_links_for_node_scoped(&self, node_id: &str, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Vec<polis_core::types::ClassLink>> {
        if self.get_class_node_scoped(node_id, scope)?.is_none() { return Ok(Vec::new()); }
        let links = self.list_class_links_for_node(node_id)?;
        if scope.is_empty() { return Ok(links); }
        let seqs: Vec<i64> = links.iter().filter(|l| matches!(l.target_kind.as_str(), "prompt" | "decision" | "ledger" | "resolution" | "approval" | "review_verdict")).filter_map(|l| l.target_id.parse().ok()).collect();
        let eligible = self.eligible_seqs(&seqs, scope)?;
        let targets = self.eligible_embedding_targets(scope)?;
        Ok(links.into_iter().filter(|l| match l.target_kind.as_str() {
            "prompt" | "decision" | "ledger" | "resolution" | "approval" | "review_verdict" => l.target_id.parse::<i64>().ok().is_some_and(|id| eligible.contains(&id)),
            "browse_event" => l.target_id.parse::<i64>().ok().is_some_and(|id| targets.contains(&("browse_event".into(), id))),
            _ => false,
        }).collect())
    }

    /// Stage one parsed proposal. Additive proposals (`file` / `create`)
    /// become LIVE nodes / links at once (B3: the adjudication that admits
    /// them runs before this call — `polis_memory::adjudicate`); structural
    /// ones enter the `class_proposals` work queue, where the gardener
    /// adjudicates them run by run. Nothing waits for a human.
    pub fn stage_proposal(
        &self,
        run_id: Option<i64>,
        p: &polis_core::proposal::Proposal,
    ) -> rusqlite::Result<polis_core::types::StagedOutcome> {
        use polis_core::{proposal::Proposal, types::StagedOutcome};
        let mut connection = self.conn();
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let conn = &tx;
        let outcome = (|| -> rusqlite::Result<polis_core::types::StagedOutcome> {
            if let Some(run) = run_id {
                if Self::run_source_forgotten_locked(conn, run)? {
                    return Ok(StagedOutcome::Skipped);
                }
            }
            match p {
                Proposal::File {
                    target_kind,
                    target_id,
                    ..
                } => {
                    if let Ok(seq) = target_id.parse::<i64>() {
                        let forgotten = if target_kind == "browse_event" {
                            conn.query_row("SELECT EXISTS(SELECT 1 FROM forgotten_captures WHERE target_kind='browse_event' AND target_id=?1)", [seq], |r|r.get::<_,bool>(0))?
                        } else {
                            Self::source_forgotten_locked(conn, seq)?
                        };
                        if forgotten {
                            return Ok(StagedOutcome::Skipped);
                        }
                    }
                }
                Proposal::Supersede {
                    old_seq, new_seq, ..
                } if Self::source_forgotten_locked(conn, *old_seq)?
                    || Self::source_forgotten_locked(conn, *new_seq)? => {
                    return Ok(StagedOutcome::Skipped);
                }
                Proposal::Collapse { cite_seqs, .. } => {
                    for seq in cite_seqs {
                        if Self::source_forgotten_locked(conn, *seq)? {
                            return Ok(StagedOutcome::Skipped);
                        }
                    }
                }
                _ => {}
            }
            let now = polis_core::ledger::now_millis();
            let exists = |id: &str| -> rusqlite::Result<bool> {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM class_nodes WHERE id = ?1 AND retired_by_run IS NULL",
                    params![id],
                    |r| r.get(0),
                )?;
                Ok(n > 0)
            };
            // B2: every row this proposal creates is journaled under the run, so
            // `revert_run` can undo it (a `create` retires the node, a `file`
            // deletes — or re-retires — the link).
            let journal =
                |op: &str, subjects: Vec<String>, pre: serde_json::Value| -> rusqlite::Result<()> {
                    if let Some(run) = run_id {
                        Self::journal_op_locked(
                            conn,
                            run,
                            &crate::runs::OpRecord::applied(op, subjects, pre),
                        )?;
                    }
                    Ok(())
                };
            match p {
                Proposal::Create {
                    parent_id, title, ..
                } => {
                    if !exists(parent_id)? {
                        return Ok(StagedOutcome::Skipped);
                    }
                    // Don't re-propose an identical child.
                    let dup: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM class_nodes WHERE parent_id = ?1 AND title = ?2 AND retired_by_run IS NULL",
                    params![parent_id, title],
                    |r| r.get(0),
                )?;
                    if dup > 0 {
                        return Ok(StagedOutcome::Skipped);
                    }
                    let id = crate::catalog::new_node_id();
                    conn.execute(
                        "INSERT INTO class_nodes
                        (id, parent_id, kind, title, summary, project_path, ip_name,
                         status, pinned, curated_by, created_at, updated_at)
                     VALUES (?1, ?2, 'node', ?3, NULL, NULL, NULL, 'accepted', 0,
                             'classifier', ?4, ?4)",
                        params![id, parent_id, title, now],
                    )?;
                    journal(
                        "create",
                        vec![crate::runs::subject::node(&id)],
                        crate::runs::image::create(&id, Some(parent_id), title),
                    )?;
                    Ok(StagedOutcome::Node)
                }
                Proposal::File {
                    parent_id,
                    sub_class,
                    target_kind,
                    target_id,
                    note,
                    ..
                } => {
                    if !exists(parent_id)? {
                        return Ok(StagedOutcome::Skipped);
                    }
                    let mut created_node = false;
                    // Resolve (or stage) the node the link attaches to.
                    let target_node = match sub_class {
                        Some(sc) if !sc.trim().is_empty() => {
                            let existing: Option<String> = conn
                            .query_row(
                                "SELECT id FROM class_nodes WHERE parent_id = ?1 AND title = ?2 AND retired_by_run IS NULL LIMIT 1",
                                params![parent_id, sc.trim()],
                                |r| r.get(0),
                            )
                            .optional()?;
                            match existing {
                                Some(id) => id,
                                None => {
                                    let id = crate::catalog::new_node_id();
                                    conn.execute(
                                    "INSERT INTO class_nodes
                                        (id, parent_id, kind, title, summary, project_path,
                                         ip_name, status, pinned, curated_by, created_at, updated_at)
                                     VALUES (?1, ?2, 'node', ?3, NULL, NULL, NULL, 'accepted', 0,
                                             'classifier', ?4, ?4)",
                                    params![id, parent_id, sc.trim(), now],
                                )?;
                                    created_node = true;
                                    journal(
                                        "create",
                                        vec![crate::runs::subject::node(&id)],
                                        crate::runs::image::create(&id, Some(parent_id), sc.trim()),
                                    )?;
                                    id
                                }
                            }
                        }
                        _ => parent_id.clone(),
                    };
                    // The dedup index sees retired links too: an identical link a
                    // revert (or a collapse) retired is REVIVED rather than
                    // duplicated, and the journal remembers which run had
                    // retired it so a revert of this filing re-retires it.
                    let prior: Option<(i64, Option<i64>)> = conn
                        .query_row(
                            "SELECT id, retired_by_run FROM class_links
                         WHERE node_id = ?1 AND target_kind = ?2 AND target_id = ?3",
                            params![target_node, target_kind, target_id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    let link_id = match prior {
                        Some((_, None)) => None, // live already: nothing to do
                        Some((id, Some(retired_by))) => {
                            conn.execute(
                                "UPDATE class_links SET retired_by_run = NULL WHERE id = ?1",
                                params![id],
                            )?;
                            journal(
                                "file",
                                vec![
                                    crate::runs::subject::node(&target_node),
                                    crate::runs::subject::link(id),
                                ],
                                crate::runs::image::file(
                                    id,
                                    &target_node,
                                    target_kind,
                                    target_id,
                                    Some(retired_by),
                                ),
                            )?;
                            Some(id)
                        }
                        None => {
                            conn.execute(
                                "INSERT INTO class_links
                                (node_id, target_kind, target_id, note, status, created_at)
                             VALUES (?1, ?2, ?3, ?4, 'accepted', ?5)",
                                params![target_node, target_kind, target_id, note, now],
                            )?;
                            let id = conn.last_insert_rowid();
                            journal(
                                "file",
                                vec![
                                    crate::runs::subject::node(&target_node),
                                    crate::runs::subject::link(id),
                                ],
                                crate::runs::image::file(
                                    id,
                                    &target_node,
                                    target_kind,
                                    target_id,
                                    None,
                                ),
                            )?;
                            Some(id)
                        }
                    };
                    if link_id.is_none() && !created_node {
                        return Ok(StagedOutcome::Skipped);
                    }
                    Ok(StagedOutcome::Link { created_node })
                }
                Proposal::Promote {
                    node_id,
                    new_parent_id,
                    rationale,
                } => {
                    if !exists(node_id)? {
                        return Ok(StagedOutcome::Skipped);
                    }
                    self.insert_structural_locked(
                        conn,
                        run_id,
                        "promote",
                        Some(node_id),
                        new_parent_id.as_deref(),
                        None,
                        None,
                        None,
                        rationale.as_deref(),
                        now,
                    )?;
                    Ok(StagedOutcome::Structural)
                }
                Proposal::Split {
                    node_id,
                    into,
                    rationale,
                } => {
                    if !exists(node_id)? {
                        return Ok(StagedOutcome::Skipped);
                    }
                    let extra = serde_json::to_string(into).unwrap_or_else(|_| "[]".into());
                    self.insert_structural_locked(
                        conn,
                        run_id,
                        "split",
                        Some(node_id),
                        None,
                        None,
                        None,
                        Some(&extra),
                        rationale.as_deref(),
                        now,
                    )?;
                    Ok(StagedOutcome::Structural)
                }
                Proposal::Merge {
                    node_ids,
                    title,
                    parent_id,
                    rationale,
                } => {
                    // Every referenced node must exist.
                    for id in node_ids {
                        if !exists(id)? {
                            return Ok(StagedOutcome::Skipped);
                        }
                    }
                    let extra = serde_json::to_string(node_ids).unwrap_or_else(|_| "[]".into());
                    self.insert_structural_locked(
                        conn,
                        run_id,
                        "merge",
                        node_ids.first().map(String::as_str),
                        parent_id.as_deref(),
                        title.as_deref(),
                        None,
                        Some(&extra),
                        rationale.as_deref(),
                        now,
                    )?;
                    Ok(StagedOutcome::Structural)
                }
                Proposal::Collapse {
                    node_id,
                    summary,
                    cite_seqs,
                    rationale,
                } => {
                    if !exists(node_id)? {
                        return Ok(StagedOutcome::Skipped);
                    }
                    let extra = serde_json::to_string(cite_seqs).unwrap_or_else(|_| "[]".into());
                    self.insert_structural_locked(
                        conn,
                        run_id,
                        "collapse",
                        Some(node_id),
                        None,
                        None,
                        Some(summary),
                        Some(&extra),
                        rationale.as_deref(),
                        now,
                    )?;
                    Ok(StagedOutcome::Structural)
                }
                Proposal::Supersede {
                    old_seq,
                    new_seq,
                    rationale,
                } => {
                    // Light stage-time screen so garbage never reaches the review
                    // strip: both seqs must exist and be decision events, old
                    // before new. The full guardrails (head-of-chain redirect,
                    // at-most-once) run at apply.
                    if old_seq >= new_seq {
                        return Ok(StagedOutcome::Skipped);
                    }
                    for seq in [old_seq, new_seq] {
                        let kind: Option<String> = conn
                            .query_row(
                                "SELECT kind FROM ledger_events WHERE seq = ?1",
                                params![seq],
                                |r| r.get(0),
                            )
                            .optional()?;
                        match kind {
                            Some(k) if polis_core::types::DECISION_KINDS.contains(&k.as_str()) => {}
                            _ => return Ok(StagedOutcome::Skipped),
                        }
                    }
                    // Don't re-stage an identical pending supersession.
                    let extra =
                        serde_json::json!({ "old_seq": old_seq, "new_seq": new_seq }).to_string();
                    let dup: i64 = conn.query_row(
                        "SELECT COUNT(*) FROM class_proposals
                     WHERE op = 'supersede' AND status = 'proposed' AND extra_json = ?1",
                        params![extra],
                        |r| r.get(0),
                    )?;
                    if dup > 0 {
                        return Ok(StagedOutcome::Skipped);
                    }
                    self.insert_structural_locked(
                        conn,
                        run_id,
                        "supersede",
                        None,
                        None,
                        None,
                        None,
                        Some(&extra),
                        rationale.as_deref(),
                        now,
                    )?;
                    Ok(StagedOutcome::Structural)
                }
            }
        })()?;
        tx.commit()?;
        Ok(outcome)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn insert_structural_locked(
        &self,
        conn: &rusqlite::Connection,
        run_id: Option<i64>,
        op: &str,
        node_id: Option<&str>,
        parent_id: Option<&str>,
        title: Option<&str>,
        summary: Option<&str>,
        extra_json: Option<&str>,
        rationale: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        // Expiry on the lake's own clock (B3 §5.1): seven lake-days after the
        // newest event at staging, whatever wall-clock does meanwhile.
        let newest: i64 = conn.query_row("SELECT COALESCE(MAX(ts), 0) FROM ledger_events", [], |r| r.get(0))?;
        let expires = newest + crate::runs::PROPOSAL_TTL_LAKE_MS;
        conn.execute(
            "INSERT INTO class_proposals
                (run_id, op, node_id, parent_id, title, summary, extra_json,
                 rationale, status, created_at, attempts, next_after_run, expires_lake_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'proposed', ?9, 0, NULL, ?10)",
            params![run_id, op, node_id, parent_id, title, summary, extra_json, rationale, now, expires],
        )?;
        Ok(())
    }

    /// Flip any `proposed` node / link live. Since B3 staging writes live rows,
    /// so this finds nothing on a current store; it stays for rows written
    /// before the bump and for the harnesses that call it. Returns the node
    /// ids it flipped. `actor` lands in `curated_by`.
    pub fn accept_all_pending(&self, actor: &str) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let now = polis_core::ledger::now_millis();
        let author = actor.to_string();
        let mut stmt = conn.prepare("SELECT id FROM class_nodes WHERE status = 'proposed' AND retired_by_run IS NULL")?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        conn.execute(
            "UPDATE class_nodes SET status = 'accepted', curated_by = ?1, updated_at = ?2
             WHERE status = 'proposed' AND retired_by_run IS NULL",
            params![author, now],
        )?;
        conn.execute(
            "UPDATE class_links SET status = 'accepted' WHERE status = 'proposed' AND retired_by_run IS NULL",
            [],
        )?;
        Ok(ids)
    }

    /// Delete a single class link (a pointer into the lake) by id, returning its
    /// `(node_id, target_kind, target_id)` so the caller can record a compensating
    /// ledger event. Removing a pointer never touches lake data or the ledger, so
    /// this is the safe inverse of an accepted `file`: the append-only chain stays
    /// intact and the reversal is recorded as a new `class_curate` event rather
    /// than by rewriting history. `None` if no such link.
    pub fn delete_class_link(
        &self,
        link_id: i64,
    ) -> rusqlite::Result<Option<(String, String, String)>> {
        let conn = self.conn();
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT node_id, target_kind, target_id FROM class_links WHERE id = ?1",
                params![link_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if row.is_some() {
            conn.execute("DELETE FROM class_links WHERE id = ?1", params![link_id])?;
        }
        Ok(row)
    }

    // `delete_node_subtree` is gone (B2): the one destructive primitive is
    // `runs::retire_node_subtree`, which marks the rows under a run so the
    // run can be reverted; `vacuum_retired` deletes past the horizon.

    // The human curation surface — accept / reject / pin / rename on nodes and
    // links — is gone (B3, plan §5.4): nothing is held for a person, a node's
    // protection is its warmth or a user note, and a title a user dislikes is
    // annotated, not renamed. `remember` / `annotate` / `forget` / `revert_run`
    // are what survives.

    pub const PROPOSAL_COLS: &'static str = "id, run_id, op, node_id, parent_id, title, summary, extra_json,
                    rationale, status, created_at, attempts, next_after_run, expires_lake_ts";

    /// Every queued structural proposal (promote / split / merge / collapse /
    /// supersede), oldest first — the whole work queue, due or deferred.
    pub fn list_class_proposals(&self) -> rusqlite::Result<Vec<polis_core::types::ClassProposalRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM class_proposals WHERE status = 'proposed' ORDER BY id ASC",
            Self::PROPOSAL_COLS
        ))?;
        let rows = stmt.query_map([], Self::row_to_proposal)?;
        rows.collect()
    }

    /// The proposals DUE this run (B3): never deferred, or deferred to a run
    /// at or before this one. Oldest first.
    pub fn list_due_proposals(&self, run_id: i64) -> rusqlite::Result<Vec<polis_core::types::ClassProposalRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM class_proposals
             WHERE status = 'proposed' AND (next_after_run IS NULL OR next_after_run <= ?1)
             ORDER BY id ASC",
            Self::PROPOSAL_COLS
        ))?;
        let rows = stmt.query_map(params![run_id], Self::row_to_proposal)?;
        rows.collect()
    }

    /// A verifier could not adjudicate this proposal this run: count the
    /// attempt and park it until `next_after_run`, keeping the reason.
    pub fn defer_proposal(&self, id: i64, attempts: i64, next_after_run: i64, reason: &str) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE class_proposals SET attempts = ?2, next_after_run = ?3, last_reason = ?4 WHERE id = ?1 AND status='proposed'",
            params![id, attempts, next_after_run, reason],
        )?;
        Ok(())
    }

    /// The lake's newest event `ts` — the clock proposals expire on.
    pub fn lake_newest_ts(&self) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.query_row("SELECT COALESCE(MAX(ts), 0) FROM ledger_events", [], |r| r.get(0))
    }

    /// The work queue's depth — proposals waiting for a run (due or deferred).
    /// Nothing here waits for a person.
    pub fn count_pending_class_proposals(&self) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*) FROM class_proposals WHERE status = 'proposed'",
            [],
            |r| r.get(0),
        )
    }

    pub fn get_class_proposal(
        &self,
        id: i64,
    ) -> rusqlite::Result<Option<polis_core::types::ClassProposalRow>> {
        let conn = self.conn();
        conn.query_row(
            &format!("SELECT {} FROM class_proposals WHERE id = ?1", Self::PROPOSAL_COLS),
            params![id],
            Self::row_to_proposal,
        )
        .optional()
    }

    pub fn row_to_proposal(r: &rusqlite::Row) -> rusqlite::Result<polis_core::types::ClassProposalRow> {
        Ok(polis_core::types::ClassProposalRow {
            id: r.get(0)?,
            run_id: r.get(1)?,
            op: r.get(2)?,
            node_id: r.get(3)?,
            parent_id: r.get(4)?,
            title: r.get(5)?,
            summary: r.get(6)?,
            extra_json: r.get(7)?,
            rationale: r.get(8)?,
            status: r.get(9)?,
            created_at: r.get(10)?,
            attempts: r.get::<_, Option<i64>>(11)?.unwrap_or(0),
            next_after_run: r.get(12)?,
            expires_lake_ts: r.get(13)?,
        })
    }

    /// Apply (accept) a structural proposal: mutate the accepted tree and drop
    /// the proposal row. Returns the facts for the `taxonomy_reorg` ledger event.
    /// Promotion re-parents preserving id/links/pins/subtree; collapse creates a
    /// digest node citing exact ledger seqs and retires the cold subtree.
    /// `actor` is who applied it — the classifier's seat name on the
    /// auto-organize path, the local human on a review-strip accept — and lands
    /// in `curated_by` plus the `supersede` event's hashed `author`.
    ///
    /// Journaled (B2) under the proposal's own `run_id`, else a fresh
    /// `curation` run — see `apply_class_proposal_in_run`.
    pub fn apply_class_proposal(
        &self,
        id: i64,
        actor: &str,
    ) -> rusqlite::Result<Option<polis_core::types::AppliedReorg>> {
        self.apply_class_proposal_in_run(id, actor, None)
    }

    /// `apply_class_proposal`, journaled under `run` (B2): every op writes a
    /// `class_run_ops` row with the pre-image its inverse needs, and every
    /// destructive step is a retire-mark stamped with that run. The journal
    /// run is `run`, else the proposal's `run_id`, else a `curation` run made
    /// here — so nothing ever applies without a run it can be reverted through.
    pub fn apply_class_proposal_in_run(
        &self,
        id: i64,
        actor: &str,
        run: Option<i64>,
    ) -> rusqlite::Result<Option<polis_core::types::AppliedReorg>> {
        use crate::runs::{image, subject, OpRecord, MODE_CURATION};
        let p = match self.get_class_proposal(id)? {
            Some(p) => p,
            None => return Ok(None),
        };
        let (run, made_run) = match run.or(p.run_id) {
            Some(r) => (r, false),
            None => (self.insert_class_run_with(MODE_CURATION, None, None)?, true),
        };
        let mut connection = self.conn();
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let conn = &tx;
        let outcome = (|| -> rusqlite::Result<Option<polis_core::types::AppliedReorg>> {
            let live: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM class_proposals WHERE id=?1 AND status='proposed')",
                [id],
                |r| r.get(0),
            )?;
            if !live || Self::run_source_forgotten_locked(conn, run)? {
                return Ok(None);
            }
            if let Some(source_run) = p.run_id {
                if Self::run_source_forgotten_locked(conn, source_run)? {
                    return Ok(None);
                }
            }
            let now = polis_core::ledger::now_millis();
            let finish_curation = |conn: &rusqlite::Connection, ops: i64| -> rusqlite::Result<()> {
                if made_run {
                    conn.execute(
                    "UPDATE class_runs SET status = 'done', finished_at = ?2, outcome = 'done', ops = ?3,
                            summary = ?4 WHERE id = ?1",
                    params![run, polis_core::ledger::now_millis(), ops, format!("applied proposal #{id}")],
                )?;
                }
                Ok(())
            };
            let detail: String = match p.op.as_str() {
                "promote" => {
                    let node = p.node_id.clone().unwrap_or_default();
                    let old_parent: Option<String> = conn
                        .query_row(
                            "SELECT parent_id FROM class_nodes WHERE id = ?1",
                            params![node],
                            |r| r.get(0),
                        )
                        .optional()?
                        .flatten();
                    // new_parent may be NULL → promote to a root.
                    conn.execute(
                        "UPDATE class_nodes SET parent_id = ?2, updated_at = ?3 WHERE id = ?1",
                        params![node, p.parent_id, now],
                    )?;
                    Self::journal_op_locked(
                        conn,
                        run,
                        &OpRecord::applied(
                            "promote",
                            vec![subject::node(&node)],
                            image::promote(&node, old_parent.as_deref(), p.parent_id.as_deref()),
                        ),
                    )?;
                    format!("→ parent {}", p.parent_id.as_deref().unwrap_or("(root)"))
                }
                "collapse" => {
                    let node = p.node_id.clone().unwrap_or_default();
                    // Whether the branch may collapse is the adjudicator's call
                    // (B3: `auto_collapse_safe` ∧ items ≥ 5 ∧ cold ∧ no note —
                    // warmth, never a pin); by the time a collapse reaches this
                    // apply it has been admitted.
                    // Parent + title of the cold branch, for the digest placement.
                    let (parent, title): (Option<String>, String) = conn.query_row(
                        "SELECT parent_id, title FROM class_nodes WHERE id = ?1",
                        params![node],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    let digest_id = crate::catalog::new_node_id();
                    let digest_title = p
                        .title
                        .clone()
                        .unwrap_or_else(|| format!("{title} (digest)"));
                    conn.execute(
                        "INSERT INTO class_nodes
                        (id, parent_id, kind, title, summary, project_path, ip_name,
                         status, pinned, curated_by, created_at, updated_at)
                     VALUES (?1, ?2, 'digest', ?3, ?4, NULL, NULL, 'accepted', 0,
                             ?5, ?6, ?6)",
                        params![digest_id, parent, digest_title, p.summary, actor, now],
                    )?;
                    // Citation links to the exact ledger seqs.
                    let mut citations: Vec<i64> = Vec::new();
                    if let Some(extra) = &p.extra_json {
                        if let Ok(seqs) = serde_json::from_str::<Vec<i64>>(extra) {
                            for seq in &seqs {
                                let changed = conn.execute(
                                    "INSERT INTO class_links
                                    (node_id, target_kind, target_id, note, status, created_at)
                                 VALUES (?1, 'ledger', ?2, NULL, 'accepted', ?3)
                                 ON CONFLICT(node_id, target_kind, target_id) DO NOTHING",
                                    params![digest_id, seq.to_string(), now],
                                )?;
                                if changed == 1 {
                                    citations.push(conn.last_insert_rowid());
                                }
                            }
                        }
                    }
                    // Retire the cold subtree (its sourcing now lives in the
                    // digest's citations, one hop away). Marks, not deletes: the
                    // revert clears them.
                    let set = Self::retire_node_subtree(conn, &node, run, Some(&digest_id))?;
                    let mut subjects: Vec<String> =
                        vec![subject::node(&node), subject::node(&digest_id)];
                    subjects.extend(
                        set.nodes
                            .iter()
                            .filter(|n| *n != &node)
                            .map(|n| subject::node(n)),
                    );
                    Self::journal_op_locked(
                        conn,
                        run,
                        &OpRecord::applied(
                            "collapse",
                            subjects,
                            serde_json::json!({
                                "nodeId": node, "digestId": digest_id,
                                "retiredNodes": set.nodes, "retiredLinks": set.links,
                                "retiredObservations": set.observations, "citationLinks": citations,
                            }),
                        ),
                    )?;
                    format!("digest {digest_id}")
                }
                "merge" => {
                    let ids: Vec<String> = p
                        .extra_json
                        .as_deref()
                        .and_then(|e| serde_json::from_str(e).ok())
                        .unwrap_or_default();
                    if ids.is_empty() {
                        self.drop_proposal_locked(conn, id)?;
                        finish_curation(conn, 0)?;
                        return Ok(None);
                    }
                    let target = ids[0].clone();
                    let (old_title, old_parent): (String, Option<String>) = conn.query_row(
                        "SELECT title, parent_id FROM class_nodes WHERE id = ?1",
                        params![target],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    if let Some(t) = &p.title {
                        conn.execute(
                            "UPDATE class_nodes SET title = ?2, updated_at = ?3 WHERE id = ?1",
                            params![target, t, now],
                        )?;
                    }
                    if let Some(parent) = &p.parent_id {
                        conn.execute(
                            "UPDATE class_nodes SET parent_id = ?2, updated_at = ?3 WHERE id = ?1",
                            params![target, parent, now],
                        )?;
                    }
                    let mut absorbed: Vec<serde_json::Value> = Vec::new();
                    let mut moved: Vec<serde_json::Value> = Vec::new();
                    let mut leftover: Vec<i64> = Vec::new();
                    let mut children: Vec<serde_json::Value> = Vec::new();
                    let mut observations: Vec<i64> = Vec::new();
                    let mut subjects: Vec<String> = vec![subject::node(&target)];
                    for other in ids.iter().skip(1) {
                        let row: Option<(Option<String>, String)> = conn
                        .query_row(
                            "SELECT parent_id, title FROM class_nodes WHERE id = ?1 AND retired_by_run IS NULL",
                            params![other],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                        let Some((oparent, otitle)) = row else {
                            continue;
                        };
                        subjects.push(subject::node(other));
                        // Move links onto the target; a duplicate the target
                        // already holds stays behind as a MARKED row (it used to
                        // be silently deleted), so the revert can bring it back.
                        let mut stmt = conn.prepare("SELECT id FROM class_links WHERE node_id = ?1 AND retired_by_run IS NULL")?;
                        let links: Vec<i64> = stmt
                            .query_map(params![other], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?;
                        drop(stmt);
                        for l in links {
                            let changed = conn.execute(
                                "UPDATE OR IGNORE class_links SET node_id = ?2 WHERE id = ?1",
                                params![l, target],
                            )?;
                            if changed == 1 {
                                moved.push(serde_json::json!({ "linkId": l, "from": other }));
                            } else {
                                conn.execute(
                                    "UPDATE class_links SET retired_by_run = ?2 WHERE id = ?1",
                                    params![l, run],
                                )?;
                                leftover.push(l);
                            }
                            subjects.push(subject::link(l));
                        }
                        // Merged-away nodes retire their observations (re-derived;
                        // ledger `observation` events remain as history).
                        let mut stmt = conn.prepare("SELECT id FROM class_observations WHERE node_id = ?1 AND retired_by_run IS NULL")?;
                        let obs: Vec<i64> = stmt
                            .query_map(params![other], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?;
                        drop(stmt);
                        for o in &obs {
                            conn.execute(
                                "UPDATE class_observations SET retired_by_run = ?2 WHERE id = ?1",
                                params![o, run],
                            )?;
                        }
                        observations.extend(obs);
                        let mut stmt = conn.prepare("SELECT id FROM class_nodes WHERE parent_id = ?1 AND retired_by_run IS NULL")?;
                        let kids: Vec<String> = stmt
                            .query_map(params![other], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?;
                        drop(stmt);
                        for k in &kids {
                            conn.execute(
                            "UPDATE class_nodes SET parent_id = ?2, updated_at = ?3 WHERE id = ?1",
                            params![k, target, now],
                        )?;
                            children.push(serde_json::json!({ "id": k, "oldParent": other }));
                        }
                        conn.execute(
                        "UPDATE class_nodes SET retired_by_run = ?2, retired_into = ?3 WHERE id = ?1",
                        params![other, run, target],
                    )?;
                        absorbed.push(serde_json::json!({ "id": other, "parentId": oparent, "title": otitle }));
                    }
                    Self::journal_op_locked(
                        conn,
                        run,
                        &OpRecord::applied(
                            "merge",
                            subjects,
                            serde_json::json!({
                                "target": target, "oldTitle": old_title, "oldParent": old_parent,
                                "absorbed": absorbed, "movedLinks": moved, "leftoverLinks": leftover,
                                "children": children, "observations": observations,
                            }),
                        ),
                    )?;
                    format!("merged {} into {target}", ids.len())
                }
                "split" => {
                    let node = p.node_id.clone().unwrap_or_default();
                    let parent: Option<String> = conn.query_row(
                        "SELECT parent_id FROM class_nodes WHERE id = ?1",
                        params![node],
                        |r| r.get(0),
                    )?;
                    let parts: Vec<polis_core::proposal::SplitPart> = p
                        .extra_json
                        .as_deref()
                        .and_then(|e| serde_json::from_str(e).ok())
                        .unwrap_or_default();
                    let mut made = 0;
                    let mut created: Vec<serde_json::Value> = Vec::new();
                    let mut subjects: Vec<String> = vec![subject::node(&node)];
                    for part in &parts {
                        let nid = crate::catalog::new_node_id();
                        conn.execute(
                            "INSERT INTO class_nodes
                            (id, parent_id, kind, title, summary, project_path, ip_name,
                             status, pinned, curated_by, created_at, updated_at)
                         VALUES (?1, ?2, 'node', ?3, NULL, NULL, NULL, 'accepted', 0,
                                 ?4, ?5, ?5)",
                            params![nid, parent, part.title, actor, now],
                        )?;
                        let mut moved_ids: Vec<i64> = Vec::new();
                        for lid in &part.link_ids {
                            let changed = conn.execute(
                                "UPDATE OR IGNORE class_links SET node_id = ?2
                             WHERE id = ?1 AND node_id = ?3 AND retired_by_run IS NULL",
                                params![lid, nid, node],
                            )?;
                            if changed == 1 {
                                moved_ids.push(*lid);
                            }
                        }
                        subjects.push(subject::node(&nid));
                        created.push(serde_json::json!({ "id": nid, "title": part.title, "linkIds": moved_ids }));
                        made += 1;
                    }
                    Self::journal_op_locked(
                        conn,
                        run,
                        &OpRecord::applied(
                            "split",
                            subjects,
                            serde_json::json!({ "nodeId": node, "parentId": parent, "created": created }),
                        ),
                    )?;
                    format!("split into {made}")
                }
                "supersede" => {
                    let (old_seq, new_seq) = match p
                        .extra_json
                        .as_deref()
                        .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
                        .and_then(|v| {
                            Some((v.get("old_seq")?.as_i64()?, v.get("new_seq")?.as_i64()?))
                        }) {
                        Some(pair) => pair,
                        None => {
                            // Malformed payload — drop, never retry forever.
                            self.drop_proposal_locked(conn, id)?;
                            finish_curation(conn, 0)?;
                            return Ok(None);
                        }
                    };
                    let rationale = p.rationale.clone().unwrap_or_default();
                    match Self::apply_supersession_locked(
                        conn, old_seq, new_seq, &rationale, actor,
                    )? {
                        polis_core::types::SupersessionOutcome::Applied {
                            effective_old,
                            new_seq,
                            event_seq,
                        } => {
                            // The supersede ledger event was appended inside
                            // apply_supersession_locked — callers must NOT also
                            // record a taxonomy_reorg for this op.
                            Self::journal_op_locked(
                                conn,
                                run,
                                &OpRecord::applied(
                                    "supersede",
                                    vec![subject::seq(effective_old), subject::seq(new_seq)],
                                    image::supersede(effective_old, new_seq, event_seq),
                                )
                                .with_ledger_seq(Some(event_seq)),
                            )?;
                            format!("#{effective_old} → #{new_seq}")
                        }
                        polis_core::types::SupersessionOutcome::Rejected(msg) => {
                            tracing::info!(target: "redline::classmem", old_seq, new_seq, %msg,
                            "supersede proposal rejected at apply");
                            self.drop_proposal_locked(conn, id)?;
                            finish_curation(conn, 0)?;
                            return Ok(None);
                        }
                    }
                }
                _ => {
                    self.drop_proposal_locked(conn, id)?;
                    finish_curation(conn, 0)?;
                    return Ok(None);
                }
            };
            self.drop_proposal_locked(conn, id)?;
            finish_curation(conn, 1)?;
            Ok(Some(polis_core::types::AppliedReorg {
                op: p.op,
                node_id: p.node_id.unwrap_or_default(),
                detail,
            }))
        })()?;
        tx.commit()?;
        Ok(outcome)
    }

    pub fn drop_proposal_locked(&self, conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<()> {
        conn.execute("DELETE FROM class_proposals WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// `target_id → (node_id, class title)` for a page of link targets, in one
    /// query per chunk. The earliest accepted link wins (`MIN(cl.id)`), which
    /// is the precedence the per-row `ORDER BY cl.id LIMIT 1` probe had.
    pub fn filings_for_targets(
        conn: &Connection,
        kinds: &[&str],
        targets: &[String],
    ) -> rusqlite::Result<std::collections::HashMap<String, (String, String)>> {
        let mut out = std::collections::HashMap::new();
        if targets.is_empty() {
            return Ok(out);
        }
        let kind_marks = vec!["?"; kinds.len()].join(", ");
        for chunk in targets.chunks(400) {
            let target_marks = vec!["?"; chunk.len()].join(", ");
            let mut stmt = conn.prepare(&format!(
                "SELECT cl.target_id, cl.node_id, cn.title FROM class_links cl
                 JOIN class_nodes cn ON cn.id = cl.node_id
                 WHERE cl.status = 'accepted' AND cl.retired_by_run IS NULL
                   AND cn.retired_by_run IS NULL
                   AND cl.target_kind IN ({kind_marks})
                   AND cl.target_id IN ({target_marks})
                   AND cl.id = (SELECT MIN(earlier.id) FROM class_links earlier
                                WHERE earlier.status = 'accepted' AND earlier.retired_by_run IS NULL
                                  AND earlier.target_kind IN ({kind_marks})
                                  AND earlier.target_id = cl.target_id)"
            ))?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::new();
            for k in kinds {
                binds.push(k);
            }
            for t in chunk {
                binds.push(t);
            }
            for k in kinds {
                binds.push(k);
            }
            let rows = stmt.query_map(binds.as_slice(), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (r.get::<_, String>(1)?, r.get::<_, String>(2)?),
                ))
            })?;
            for row in rows {
                let (target, filing) = row?;
                out.insert(target, filing);
            }
        }
        Ok(out)
    }

    /// The corpus for the keeper's observation pass: a node's ledger-resolvable
    /// links as `(seq, kind, ts, snippet)`, newest first. Snippet is the prompt
    /// body head (or its gist once compacted); bodyless decision events yield
    /// `None` and are rendered by kind alone.
    pub fn node_link_items(
        &self,
        node_id: &str,
        limit: i64,
    ) -> rusqlite::Result<Vec<(i64, String, i64, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT le.seq, le.kind, le.ts,
                    COALESCE(NULLIF(substr(p.body, 1, 240), ''), p.gist)
             FROM class_links l
             JOIN ledger_events le ON CAST(l.target_id AS INTEGER) = le.seq
             LEFT JOIN prompts p ON p.id = le.prompt_id
             WHERE l.node_id = ?1
               AND l.target_kind IN ('prompt', 'decision', 'ledger')
               AND l.status = 'accepted' AND l.retired_by_run IS NULL
             ORDER BY le.ts DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![node_id, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect()
    }

    /// The seq the last completed classifier run consumed up to — the delta
    /// floor for the next run. 0 when no run has completed.
    pub fn last_run_seq_to(&self) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COALESCE(MAX(seq_to), 0) FROM class_runs WHERE status = 'done'",
            [],
            |r| r.get(0),
        )
    }

    /// The newest accepted links whose target is a decision event, as
    /// `(node id, node title, decision seq)` — the canary's Decision subjects:
    /// a filed decision is reachable through its class today (an unfiled one
    /// is not; the C-program's decision arm is what changes that).
    pub fn recent_decision_links(&self, limit: i64) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n.id, n.title, CAST(l.target_id AS INTEGER)
             FROM class_links l JOIN class_nodes n ON n.id = l.node_id
             WHERE l.status = 'accepted' AND n.status = 'accepted'
               AND l.retired_by_run IS NULL AND n.retired_by_run IS NULL
               AND l.target_kind IN ('decision', 'resolution', 'approval', 'review_verdict')
               AND l.target_id GLOB '[0-9]*'
             ORDER BY l.id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.max(1)], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Accepted non-root nodes that hold at least one accepted link, with
    /// their link counts, by id — the canary's ClassReach candidates (the
    /// caller picks its sample; the order here is stable so a seeded pick
    /// is reproducible).
    pub fn nodes_with_links(&self) -> rusqlite::Result<Vec<(polis_core::types::ClassNode, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n.id, n.parent_id, n.kind, n.title, n.summary, n.project_path, n.ip_name,
                    n.status, n.pinned, n.curated_by, n.created_at, n.updated_at,
                    (SELECT COUNT(*) FROM class_links l WHERE l.node_id = n.id AND l.status = 'accepted' AND l.retired_by_run IS NULL) AS links
             FROM class_nodes n
             WHERE n.status = 'accepted' AND n.parent_id IS NOT NULL AND n.retired_by_run IS NULL AND links > 0
             ORDER BY n.id ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                polis_core::types::ClassNode {
                    id: r.get(0)?,
                    parent_id: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    summary: r.get(4)?,
                    project_path: r.get(5)?,
                    ip_name: r.get(6)?,
                    status: r.get(7)?,
                    pinned: r.get::<_, i64>(8)? != 0,
                    curated_by: r.get(9)?,
                    created_at: r.get(10)?,
                    updated_at: r.get(11)?,
                },
                r.get(12)?,
            ))
        })?;
        rows.collect()
    }

    /// The accepted nodes whose accepted links point at a ledger seq, as
    /// `(node id, node title)`.
    pub fn nodes_linking_seq(&self, seq: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let s = seq.to_string();
        let mut stmt = conn.prepare(
            "SELECT n.id, n.title FROM class_links l JOIN class_nodes n ON n.id = l.node_id
             WHERE l.status = 'accepted' AND n.status = 'accepted'
               AND l.retired_by_run IS NULL AND n.retired_by_run IS NULL
               AND l.target_kind IN ('prompt', 'decision', 'ledger', 'resolution', 'approval', 'review_verdict')
               AND l.target_id = ?1
             ORDER BY l.id ASC",
        )?;
        let rows = stmt.query_map(params![s], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    pub fn insert_class_run(&self, seq_from: i64, seq_to: i64) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO class_runs (started_at, status, seq_from, seq_to, mode)
             VALUES (?1, 'running', ?2, ?3, ?4)",
            params![polis_core::ledger::now_millis(), seq_from, seq_to, crate::runs::MODE_ORGANIZE],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn finish_class_run(
        &self,
        id: i64,
        status: &str,
        claude_session_id: Option<&str>,
        summary: &str,
    ) -> rusqlite::Result<()> {
        self.finish_class_run_with(
            id,
            &polis_core::types::ClassRunFinish {
                status: status.to_string(),
                claude_session_id: claude_session_id.map(str::to_string),
                summary: summary.to_string(),
                outcome: Some(status.to_string()),
                ..Default::default()
            },
        )
    }

    /// `finish_class_run` with B1's accounting: what the run cost and did.
    pub fn finish_class_run_with(&self, id: i64, f: &polis_core::types::ClassRunFinish) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE class_runs SET status = ?2, finished_at = ?3, claude_session_id = ?4,
                    summary = CASE WHEN source_forgotten_at IS NULL THEN ?5 ELSE '[forgotten source]' END,
                    duration_ms = ?6, items = ?7, ops = ?8, model = ?9, outcome = ?10,
                    error = CASE WHEN source_forgotten_at IS NULL THEN ?11 ELSE NULL END,
                    mode = COALESCE(?12, mode), llm_calls = ?13, prompt_bytes = ?14,
                    tokens_in = ?15, tokens_out = ?16, wall_ms = COALESCE(?17, ?6),
                    canary_json = COALESCE(?18, canary_json)
             WHERE id = ?1",
            params![
                id,
                f.status,
                polis_core::ledger::now_millis(),
                f.claude_session_id,
                f.summary,
                f.duration_ms,
                f.items,
                f.ops,
                f.model,
                f.outcome,
                f.error,
                f.mode,
                f.llm_calls,
                f.prompt_bytes,
                f.tokens_in,
                f.tokens_out,
                f.wall_ms,
                f.canary_json
            ],
        )?;
        Ok(())
    }

    /// Stamp a run's canary recall before/after (B3's auto-revert writes
    /// these; B1 only measures).
    pub fn set_class_run_canary(&self, id: i64, before: Option<f64>, after: Option<f64>) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE class_runs SET canary_before = ?2, canary_after = ?3 WHERE id = ?1",
            params![id, before, after],
        )?;
        Ok(())
    }

    /// The newest `limit` runs, newest first — the efficacy table's input
    /// (organize p50/p90, error rate over the last 50).
    pub fn list_class_runs(&self, limit: i64) -> rusqlite::Result<Vec<polis_core::types::ClassRun>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM class_runs ORDER BY id DESC LIMIT ?1",
            Self::CLASS_RUN_COLS
        ))?;
        let rows = stmt.query_map(params![limit.max(1)], Self::row_to_class_run)?;
        rows.collect()
    }

    pub const CLASS_RUN_COLS: &'static str = "id, started_at, finished_at, status, seq_from, seq_to, claude_session_id, summary,
             duration_ms, items, ops, model, outcome, canary_before, canary_after, error,
             mode, llm_calls, prompt_bytes, tokens_in, tokens_out, wall_ms, canary_json";

    pub fn row_to_class_run(r: &rusqlite::Row) -> rusqlite::Result<polis_core::types::ClassRun> {
        Ok(polis_core::types::ClassRun {
            id: r.get(0)?,
            started_at: r.get(1)?,
            finished_at: r.get(2)?,
            status: r.get(3)?,
            seq_from: r.get(4)?,
            seq_to: r.get(5)?,
            claude_session_id: r.get(6)?,
            summary: r.get(7)?,
            duration_ms: r.get(8)?,
            items: r.get(9)?,
            ops: r.get(10)?,
            model: r.get(11)?,
            outcome: r.get(12)?,
            canary_before: r.get(13)?,
            canary_after: r.get(14)?,
            error: r.get(15)?,
            mode: r.get(16)?,
            llm_calls: r.get(17)?,
            prompt_bytes: r.get(18)?,
            tokens_in: r.get(19)?,
            tokens_out: r.get(20)?,
            wall_ms: r.get(21)?,
            canary_json: r.get(22)?,
        })
    }

    pub fn latest_class_run(&self) -> rusqlite::Result<Option<polis_core::types::ClassRun>> {
        let conn = self.conn();
        conn.query_row(
            &format!("SELECT {} FROM class_runs ORDER BY id DESC LIMIT 1", Self::CLASS_RUN_COLS),
            [],
            Self::row_to_class_run,
        )
        .optional()
    }

    /// Lake items (prompts + decision events) with `seq > since_seq`, oldest
    /// first — the classifier's delta input and the `/v1/memory/prompts` route.
    /// Bodies are truncated to keep the vector light.
    pub fn list_lake_items_since(
        &self,
        since_seq: i64,
        limit: i64,
    ) -> rusqlite::Result<Vec<polis_core::types::LakeItem>> {
        self.list_lake_items_since_scoped(since_seq, limit, &Default::default())
    }

    pub fn list_lake_items_since_scoped(&self, since_seq: i64, limit: i64, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Vec<polis_core::types::LakeItem>> {
        let conn = self.conn();
        let scoped = Self::ledger_scope_clause_locked(&conn, "le", scope)?.numbered(3);
        let role_guard = if scope.roles.iter().any(|r| r == "agent") { "" } else { " AND COALESCE(p.role, 'user') <> 'agent'" };
        // Surface browse-event content too (they carry no prompt row): a
        // `browse_event` ledger row joins `browse_events` by ref_id, so the
        // classifier sees the page text (as `body`) under a synthetic
        // `browse_event` surface and can file it under a class like any prompt.
        // User notes get the same treatment (P3): a `note` event joins its
        // CURRENT `user_notes` row — standalone rows by id, targeted rows by
        // target — under a synthetic `note` surface, so the user's own words
        // become classifiable lake items (a strong curation signal; filing
        // authority stays with project_path/surface).
        //
        // Machine bookkeeping stays OUT of the feed: router_verdict (the
        // shadow router's own record), moot_turn and the work_* lifecycle
        // acts carry no prompt row and no body, so downstream they'd render
        // under the "[decision references …]" fallback and read to the
        // classifier as pseudo-decisions. They remain on the chain and in the
        // ledger views — they just never feed classification.
        //
        // `role = 'agent'` joins them, permanently. Redline's own constructed
        // prefaces are not things the user thought about, and feeding them to
        // the classifier taught the taxonomy to describe Redline's instruction
        // text. `system` rows STAY: a `<task-notification>` reports work the
        // user's own session actually did, which is a real event in their
        // history even though they didn't type it.
        //
        // The body expression resolves the gist. `COALESCE(p.body, …)` was
        // wrong in a way that returned no error: compaction sets `body = ''`
        // rather than NULL, so a compacted prompt handed the classifier an
        // EMPTY body — 224 rows that read as content-free rather than as
        // summarized. `NULLIF(p.body, '')` is the fix, and the same expression
        // is `PROMPT_TEXT` everywhere else it's needed.
        let mut stmt = conn.prepare(&format!(
            "SELECT le.seq, le.ts, le.kind, le.ref_kind, le.ref_id, le.session_id,
                    COALESCE(p.surface, CASE WHEN le.ref_kind = 'browse_event'
                                             THEN 'browse_event' END,
                             CASE WHEN le.kind = 'note' THEN 'note' END),
                    p.origin, p.role, p.mission_id, p.project_path,
                    COALESCE(NULLIF(p.body, ''), p.gist, be.text, un.text),
                    p.thread_kind, p.thread_id, p.parent_session_id, p.model
             FROM ledger_events le
             LEFT JOIN prompts p ON le.prompt_id = p.id
             LEFT JOIN browse_events be
                    ON le.ref_kind = 'browse_event' AND le.ref_id = CAST(be.id AS TEXT)
             LEFT JOIN user_notes un
                    ON le.kind = 'note'
                   AND ((le.ref_kind = 'none' AND un.id = CAST(le.ref_id AS INTEGER))
                     OR (le.ref_kind <> 'none' AND un.target_kind = le.ref_kind
                         AND un.target_id = le.ref_id))
             WHERE le.seq > ?1
               AND le.kind NOT IN ('router_verdict', 'moot_turn',
                                   'work_file', 'work_claim', 'work_close')
               {}{}
             ORDER BY le.seq ASC
             LIMIT ?2", role_guard, scoped.sql
        ))?;
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&since_seq, &limit];
        binds.extend(scoped.binds.iter().map(|v| v.as_ref()));
        let rows = stmt.query_map(binds.as_slice(), |r| {
            let body: Option<String> = r.get(11)?;
            Ok(polis_core::types::LakeItem {
                seq: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                ref_kind: r.get(3)?,
                ref_id: r.get(4)?,
                session_id: r.get(5)?,
                surface: r.get(6)?,
                origin: r.get(7)?,
                role: r.get(8)?,
                mission_id: r.get(9)?,
                project_path: r.get(10)?,
                body: body.map(|b| {
                    if b.chars().count() > 4000 {
                        b.chars().take(4000).collect::<String>() + "…"
                    } else {
                        b
                    }
                }),
                thread_kind: r.get(12)?,
                thread_id: r.get(13)?,
                parent_session_id: r.get(14)?,
                model: r.get(15)?,
            })
        })?;
        rows.collect()
    }

    /// Every node plus its total link count (one query, no N+1) — backs the tree
    /// view's leaf-count badges.
    pub fn list_class_nodes_with_counts(
        &self,
    ) -> rusqlite::Result<Vec<(polis_core::types::ClassNode, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n.id, n.parent_id, n.kind, n.title, n.summary, n.project_path,
                    n.ip_name, n.status, n.pinned, n.curated_by, n.created_at, n.updated_at,
                    (SELECT COUNT(*) FROM class_links l WHERE l.node_id = n.id AND l.retired_by_run IS NULL) AS link_count
             FROM class_nodes n WHERE n.retired_by_run IS NULL ORDER BY n.title ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((PolisStore::row_to_class_node(r)?, r.get::<_, i64>(12)?))
        })?;
        rows.collect()
    }

    /// Accepted-class-node counts per root class (title → linked-item count
    /// rolled over the whole subtree) — the "class" axis of `/v1/context/stats`.
    /// Roots only; a repo-less lake yields just `~general`.
    pub fn class_link_counts_by_root(&self) -> rusqlite::Result<Vec<(String, i64)>> {
        let nodes = self.list_class_nodes_with_counts()?;
        // Map node → (parent, title, own link count).
        let mut parent: std::collections::HashMap<String, Option<String>> = std::collections::HashMap::new();
        let mut title: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut own: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for (n, c) in &nodes {
            parent.insert(n.id.clone(), n.parent_id.clone());
            title.insert(n.id.clone(), n.title.clone());
            own.insert(n.id.clone(), *c);
        }
        // Roll each node's own count up to its root.
        let root_of = |mut id: String| -> Option<String> {
            for _ in 0..64 {
                match parent.get(&id) {
                    Some(Some(p)) => id = p.clone(),
                    Some(None) => return Some(id),
                    None => return None,
                }
            }
            None
        };
        let mut totals: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for (id, c) in &own {
            if let Some(root) = root_of(id.clone()) {
                *totals.entry(root).or_insert(0) += *c;
            }
        }
        let mut out: Vec<(String, i64)> = totals
            .into_iter()
            .map(|(root, c)| (title.get(&root).cloned().unwrap_or(root), c))
            .collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Per-node DIRECT link activity: `node_id → (link_count, newest_ts?)`. The
    /// timestamp is the max `ledger_events.ts` among the node's own links that
    /// resolve to a ledger seq (prompt/decision/ledger). Rolled up into subtree
    /// stats by `classmem::subtree_stats` — the temporal facts that give "cold"
    /// a scope.
    pub fn node_direct_link_activity(
        &self,
    ) -> rusqlite::Result<std::collections::HashMap<String, (i64, Option<i64>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT l.node_id, COUNT(*),
                    MAX(CASE WHEN l.target_kind IN ('prompt','decision','ledger')
                        THEN (SELECT le.ts FROM ledger_events le
                              WHERE le.seq = CAST(l.target_id AS INTEGER))
                        ELSE NULL END)
             FROM class_links l WHERE l.retired_by_run IS NULL GROUP BY l.node_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?))
        })?;
        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (id, count, ts) = row?;
            map.insert(id, (count, ts));
        }
        Ok(map)
    }

    /// The lake's temporal envelope (oldest/newest ledger ts) — the reference
    /// frame coldness is measured against (never wall-clock).
    pub fn lake_envelope(&self) -> rusqlite::Result<polis_core::coldness::LakeEnvelope> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COALESCE(MIN(ts), 0), COALESCE(MAX(ts), 0) FROM ledger_events",
            [],
            |r| {
                Ok(polis_core::coldness::LakeEnvelope {
                    oldest: r.get(0)?,
                    newest: r.get(1)?,
                })
            },
        )
    }

    /// `(class node_id, session_id)` pairs reachable through accepted links —
    /// the raw material of the Map's derived `co-occurs` edge. Two keyspaces,
    /// the `query_ledger_events` discipline: seq-keyed targets resolve through
    /// `ledger_events.session_id`; `session` targets ARE the session id.
    /// DISTINCT, so a class citing one session five times contributes one pair.
    pub fn class_session_pairs(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT l.node_id, le.session_id
               FROM class_links l
               JOIN ledger_events le ON le.seq = CAST(l.target_id AS INTEGER)
              WHERE l.status = 'accepted' AND l.retired_by_run IS NULL
                AND l.target_kind IN ('prompt', 'decision', 'revision', 'note', 'ledger')
                AND le.session_id IS NOT NULL
             UNION
             SELECT DISTINCT l.node_id, l.target_id
               FROM class_links l
              WHERE l.status = 'accepted' AND l.retired_by_run IS NULL AND l.target_kind = 'session'
             ORDER BY 1, 2",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// For each seq: `(session_id, accepted class filing)` — how a decision
    /// lands on the Map (its class when filed, its session otherwise; never a
    /// raw-event node). Same seq-keyspace rule as `class_session_pairs`.
    pub fn resolve_map_endpoints(
        &self,
        seqs: &[i64],
    ) -> rusqlite::Result<std::collections::HashMap<i64, (Option<String>, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT le.session_id,
                    (SELECT cl.node_id FROM class_links cl
                      WHERE cl.status = 'accepted' AND cl.retired_by_run IS NULL
                        AND cl.target_kind IN ('prompt', 'decision', 'revision', 'note', 'ledger')
                        AND cl.target_id = CAST(le.seq AS TEXT)
                      ORDER BY cl.id ASC LIMIT 1)
             FROM ledger_events le WHERE le.seq = ?1",
        )?;
        let mut out = std::collections::HashMap::new();
        for &seq in seqs {
            if let Some(pair) = stmt
                .query_row(params![seq], |r| {
                    Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?))
                })
                .optional()?
            {
                out.insert(seq, pair);
            }
        }
        Ok(out)
    }
}

// --- B3: warmth without pins, and the health report's facts ------------------
impl PolisStore {
    /// Bump `last_recalled_at` for the nodes and links an answer pack served
    /// (the gardener flushes the in-memory recall log through this; the read
    /// path itself never writes). A later recall never lowers a stamp.
    pub fn bump_recalled(&self, nodes: &[(String, i64)], links: &[(i64, i64)]) -> rusqlite::Result<usize> {
        let conn = self.conn();
        let mut n = 0usize;
        for (id, ts) in nodes {
            n += conn.execute(
                "UPDATE class_nodes SET last_recalled_at = ?2
                 WHERE id = ?1 AND (last_recalled_at IS NULL OR last_recalled_at < ?2)",
                params![id, ts],
            )?;
        }
        for (id, ts) in links {
            n += conn.execute(
                "UPDATE class_links SET last_recalled_at = ?2
                 WHERE id = ?1 AND (last_recalled_at IS NULL OR last_recalled_at < ?2)",
                params![id, ts],
            )?;
        }
        Ok(n)
    }

    /// Per live node: the newest recall of the node itself or any of its live
    /// links (`None` = never served). The warmth rule rolls this up the tree.
    pub fn node_warmth(&self) -> rusqlite::Result<std::collections::HashMap<String, Option<i64>>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT n.id,
                    MAX(COALESCE(n.last_recalled_at, 0),
                        COALESCE((SELECT MAX(l.last_recalled_at) FROM class_links l
                                  WHERE l.node_id = n.id AND l.retired_by_run IS NULL), 0))
             FROM class_nodes n WHERE n.retired_by_run IS NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (id, ts) = row?;
            out.insert(id, (ts > 0).then_some(ts));
        }
        Ok(out)
    }

    /// The live nodes a user note targets — the other half of protection.
    pub fn noted_node_ids(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT un.target_id FROM user_notes un
             JOIN class_nodes n ON n.id = un.target_id AND n.retired_by_run IS NULL
             WHERE un.target_kind = 'class_node' AND un.target_id IS NOT NULL AND un.text <> ''",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// Every live prompt link with the prompt's own `project_path` — the
    /// provenance check's input (`catalog_health.provenance_violations`).
    pub fn prompt_link_provenance(&self) -> rusqlite::Result<Vec<(String, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT l.node_id, p.project_path
             FROM class_links l
             JOIN ledger_events le ON le.seq = CAST(l.target_id AS INTEGER)
             JOIN prompts p ON p.id = le.prompt_id
             WHERE l.target_kind = 'prompt' AND l.retired_by_run IS NULL AND l.target_id GLOB '[0-9]*'",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
        rows.collect()
    }

    /// The `project_path` of a lake item by seq (a prompt's), for the file
    /// op's provenance invariant. `None` when the item carries none.
    pub fn item_project_path(&self, seq: i64) -> rusqlite::Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT p.project_path FROM ledger_events le
                 LEFT JOIN prompts p ON p.id = le.prompt_id WHERE le.seq = ?1",
                params![seq],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// A ledger event's `(kind, ref_kind, ref_id)` — the supersede rule's
    /// subject key. `None` for an unknown seq.
    pub fn ledger_event_ref(&self, seq: i64) -> rusqlite::Result<Option<(String, Option<String>, Option<String>)>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT kind, ref_kind, ref_id FROM ledger_events WHERE seq = ?1",
            params![seq],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
    }

    /// Whether a ledger seq exists.
    pub fn seq_exists(&self, seq: i64) -> rusqlite::Result<bool> {
        let conn = self.conn();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM ledger_events WHERE seq = ?1", params![seq], |r| r.get(0))?;
        Ok(n > 0)
    }
}
