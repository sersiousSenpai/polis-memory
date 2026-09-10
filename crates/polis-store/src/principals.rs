// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Identity in the store (Session E2, plan §4.5): the `principals` and
//! `principal_aliases` tables, the resolution of an author string to the
//! ids a row is scoped by, the stamping of the non-hashed scope columns, and
//! the scope clauses the read paths bind.
//!
//! The pure derivations (ids from a key, the alias rule) are
//! `polis_core::identity`; the key itself and the bind event's signature are
//! `polis_memory::identity`. This module never sees a private key.

use polis_core::identity::{Principal, PrincipalKind};
use rusqlite::{params, Connection, OptionalExtension};

use crate::PolisStore;

/// The ids a row is scoped by, resolved from its author.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeIds {
    /// The human.
    pub principal_id: Option<String>,
    pub device_id: Option<String>,
    pub agent_id: Option<String>,
}

/// What a backfill / stamp sweep touched, per table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StampReport {
    pub prompts: usize,
    pub browse_events: usize,
    pub user_notes: usize,
    pub class_nodes: usize,
    pub class_observations: usize,
}

impl StampReport {
    pub fn total(&self) -> usize {
        self.prompts + self.browse_events + self.user_notes + self.class_nodes + self.class_observations
    }
}

/// The identity-scope filter a read path binds. Every field is an id or a
/// name resolved to an id through the alias table before binding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ScopeFilter {
    pub principal: Option<String>,
    pub project: Option<String>,
    pub roles: Vec<String>,
    pub after: Option<i64>,
    pub before: Option<i64>,
    pub agent: Option<String>,
    pub run: Option<String>,
    pub org: Option<String>,
    /// E3: widen a read to the imported foreign chains. Never the default.
    pub include_shared: bool,
}

impl ScopeFilter {
    /// The identity half of a [`polis_core::api::Scope`].
    pub fn from_scope(scope: &polis_core::api::Scope) -> Self {
        ScopeFilter { principal: scope.principal.clone(), agent: scope.agent.clone(), run: scope.run.clone(), org: scope.org.clone(), include_shared: scope.include_shared, project: scope.project.clone(), ..Default::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.project.is_none() && self.roles.is_empty() && self.after.is_none() && self.before.is_none() && self.principal.is_none() && self.agent.is_none() && self.run.is_none() && self.org.is_none()
    }
}

/// A WHERE fragment plus its bound values — appended to a query whose own
/// placeholders come first (bare `?` binds after the highest numbered one).
pub struct ScopeSql {
    pub sql: String,
    pub binds: Vec<Box<dyn rusqlite::ToSql>>,
}

impl ScopeSql {
    /// Number placeholders explicitly when a caller's later LIMIT reserves slots.
    pub fn numbered(mut self, first: usize) -> Self {
        let mut next = first;
        self.sql = self.sql.chars().fold(String::new(), |mut out, c| {
            if c == '?' { out.push_str(&format!("?{next}")); next += 1; } else { out.push(c); }
            out
        });
        self
    }
}

impl PolisStore {
    /// Monotonic identity for process-local caches; never reused after a store is dropped.
    pub fn cache_identity(&self) -> usize { self.cache_id }

    /// Eligible ledger evidence, resolved from its authoritative source row.
    /// Unmapped bookkeeping never acquires another source's scope by proximity.
    pub fn ledger_scope_clause_locked(conn: &Connection, alias: &str, f: &ScopeFilter) -> rusqlite::Result<ScopeSql> {
        if f.is_empty() { return Ok(ScopeSql { sql: String::new(), binds: Vec::new() }); }
        let p = Self::scope_clause_locked(conn, "p", f)?;
        let be = Self::scope_clause_locked(conn, "be", f)?;
        let un = Self::scope_clause_locked(conn, "un", f)?;
        let decision = Self::scope_clause_locked(conn, "p", f)?;
        let sql = format!(" AND (EXISTS (SELECT 1 FROM prompts p WHERE p.id = {alias}.prompt_id{})
            OR EXISTS (SELECT 1 FROM browse_events be WHERE {alias}.ref_kind = 'browse_event' AND CAST(be.id AS TEXT) = {alias}.ref_id{})
            OR EXISTS (SELECT 1 FROM user_notes un WHERE (un.seq = {alias}.seq OR un.id IN
                (SELECT ne.note_id FROM note_events ne WHERE ne.seq={alias}.seq AND ne.payload_hash={alias}.payload_hash
                    AND ((un.target_kind='none' AND {alias}.ref_kind='none' AND {alias}.ref_id=CAST(un.id AS TEXT))
                        OR (un.target_kind<>'none' AND {alias}.ref_kind=un.target_kind AND {alias}.ref_id=un.target_id)
                        OR ({alias}.ref_kind='user_note' AND {alias}.ref_id=CAST(un.id AS TEXT))))) {})
            OR EXISTS (SELECT 1 FROM decision_evidence d JOIN ledger_events src ON src.seq = d.source_seq JOIN prompts p ON p.id = src.prompt_id WHERE d.seq = {alias}.seq{}))", p.sql, be.sql, un.sql, decision.sql);
        let mut binds = p.binds; binds.extend(be.binds); binds.extend(un.binds); binds.extend(decision.binds);
        Ok(ScopeSql { sql, binds })
    }

    pub fn eligible_seqs(&self, seqs: &[i64], scope: &ScopeFilter) -> rusqlite::Result<std::collections::HashSet<i64>> {
        if seqs.is_empty() { return Ok(Default::default()); }
        let conn = self.conn();
        let scoped = Self::ledger_scope_clause_locked(&conn, "le", scope)?;
        let mut out = std::collections::HashSet::new();
        for chunk in seqs.chunks(400) {
            let marks = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!("SELECT le.seq FROM ledger_events le WHERE le.seq IN ({marks}){}", scoped.sql))?;
            let mut refs: Vec<&dyn rusqlite::ToSql> = chunk.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
            refs.extend(scoped.binds.iter().map(|v| v.as_ref()));
            out.extend(stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    /// The model index's eligible source targets, before cosine top-k.
    pub fn eligible_embedding_targets(&self, scope: &ScopeFilter) -> rusqlite::Result<std::collections::HashSet<(String, i64)>> {
        let conn = self.conn();
        let mut out = std::collections::HashSet::new();
        for (table, alias, kind, id) in [("prompts", "p", "prompt", "id"), ("browse_events", "be", "browse_event", "id"), ("class_nodes", "n", "class_node", "rowid")] {
            let scoped = Self::scope_clause_locked(&conn, alias, scope)?;
            let live = if table == "class_nodes" { format!(" AND {alias}.retired_by_run IS NULL") } else { String::new() };
            let mut stmt = conn.prepare(&format!("SELECT {alias}.{id} FROM {table} {alias} WHERE 1=1{live}{}", scoped.sql))?;
            let refs: Vec<&dyn rusqlite::ToSql> = scoped.binds.iter().map(|v| v.as_ref()).collect();
            for id in stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0))? { out.insert((kind.to_string(), id?)); }
        }
        if scope.include_shared {
            let scoped = Self::foreign_scope_clause_locked(&conn, scope)?;
            let mut stmt = conn.prepare(&format!("SELECT p.id FROM foreign_prompts p WHERE p.tombstoned = 0{}", scoped.sql))?;
            let refs: Vec<&dyn rusqlite::ToSql> = scoped.binds.iter().map(|v| v.as_ref()).collect();
            for id in stmt.query_map(refs.as_slice(), |r| r.get::<_, i64>(0))? { out.insert(("foreign_prompt".to_string(), id?)); }
        }
        Ok(out)
    }

    pub fn context_stats_scoped(&self, scope: &ScopeFilter) -> rusqlite::Result<polis_core::types::ContextStats> {
        let conn = self.conn();
        let scoped = Self::ledger_scope_clause_locked(&conn, "le", scope)?;
        let from = format!("FROM ledger_events le LEFT JOIN prompts p ON p.id = le.prompt_id WHERE 1=1{}", scoped.sql);
        let refs: Vec<&dyn rusqlite::ToSql> = scoped.binds.iter().map(|v| v.as_ref()).collect();
        let count = |extra: &str| conn.query_row(&format!("SELECT COUNT(*) {from} {extra}"), refs.as_slice(), |r| r.get::<_, i64>(0));
        let group = |expr: &str, extra: &str| -> rusqlite::Result<Vec<(String, i64)>> {
            let mut stmt = conn.prepare(&format!("SELECT COALESCE({expr}, 'unknown'), COUNT(*) {from} {extra} GROUP BY 1 ORDER BY 1"))?;
            let rows = stmt.query_map(refs.as_slice(), |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
            rows
        };
        Ok(polis_core::types::ContextStats {
            generated_ts: polis_core::ledger::now_millis(), total_prompts: count("AND le.kind = 'prompt'")?, total_events: count("")?,
            by_day: group("strftime('%Y-%m-%d', le.ts / 1000, 'unixepoch')", "AND le.kind = 'prompt'")?,
            by_surface: group("p.surface", "AND le.kind = 'prompt'")?, by_kind: group("le.kind", "")?, by_author: group("le.author", "")?,
            by_class: Vec::new(), latency: Vec::new(),
        })
    }

    pub fn upsert_principal(&self, p: &Principal) -> rusqlite::Result<()> {
        Self::upsert_principal_locked(&self.conn(), p)
    }

    pub fn upsert_principal_locked(conn: &Connection, p: &Principal) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT INTO principals (principal_id, kind, pubkey, parent_id, display_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(principal_id) DO UPDATE SET
                kind = excluded.kind,
                pubkey = COALESCE(excluded.pubkey, principals.pubkey),
                parent_id = COALESCE(excluded.parent_id, principals.parent_id),
                display_name = COALESCE(excluded.display_name, principals.display_name)",
            params![p.principal_id, p.kind.as_str(), p.pubkey, p.parent_id, p.display_name, p.created_at],
        )?;
        Ok(())
    }

    pub fn get_principal(&self, id: &str) -> rusqlite::Result<Option<Principal>> {
        Self::get_principal_locked(&self.conn(), id)
    }

    pub fn get_principal_locked(conn: &Connection, id: &str) -> rusqlite::Result<Option<Principal>> {
        conn.query_row(
            "SELECT principal_id, kind, pubkey, parent_id, display_name, created_at
             FROM principals WHERE principal_id = ?1",
            params![id],
            Self::row_to_principal,
        )
        .optional()
    }

    pub fn list_principals(&self) -> rusqlite::Result<Vec<Principal>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT principal_id, kind, pubkey, parent_id, display_name, created_at
             FROM principals ORDER BY created_at ASC, principal_id ASC",
        )?;
        let rows = stmt.query_map([], Self::row_to_principal)?;
        rows.collect()
    }

    pub fn row_to_principal(r: &rusqlite::Row) -> rusqlite::Result<Principal> {
        let kind: String = r.get(1)?;
        Ok(Principal {
            principal_id: r.get(0)?,
            kind: PrincipalKind::parse(&kind).unwrap_or(PrincipalKind::Agent),
            pubkey: r.get(2)?,
            parent_id: r.get(3)?,
            display_name: r.get(4)?,
            created_at: r.get(5)?,
        })
    }

    /// The device (or agent) ids under a human, one level down each.
    pub fn list_children(&self, parent_id: &str) -> rusqlite::Result<Vec<Principal>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT principal_id, kind, pubkey, parent_id, display_name, created_at
             FROM principals WHERE parent_id = ?1 ORDER BY principal_id",
        )?;
        let rows = stmt.query_map(params![parent_id], Self::row_to_principal)?;
        rows.collect()
    }

    /// An alias row: a legacy author string → a principal. Insert-only on
    /// purpose: an alias, once resolved, is part of how history reads.
    pub fn set_alias(&self, alias: &str, principal_id: &str) -> rusqlite::Result<bool> {
        Self::set_alias_locked(&self.conn(), alias, principal_id)
    }

    pub fn set_alias_locked(conn: &Connection, alias: &str, principal_id: &str) -> rusqlite::Result<bool> {
        let n = conn.execute(
            "INSERT INTO principal_aliases (alias, principal_id) VALUES (?1, ?2)
             ON CONFLICT(alias) DO NOTHING",
            params![alias, principal_id],
        )?;
        Ok(n > 0)
    }

    pub fn list_aliases(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT alias, principal_id FROM principal_aliases ORDER BY alias")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// `COALESCE(alias.principal_id, author)` — with the second arm accepted
    /// only when it IS a principal id. `None` for a legacy string nobody has
    /// aliased yet.
    pub fn resolve_author(&self, author: &str) -> rusqlite::Result<Option<String>> {
        Self::resolve_author_locked(&self.conn(), author)
    }

    pub fn resolve_author_locked(conn: &Connection, author: &str) -> rusqlite::Result<Option<String>> {
        conn.query_row(
            "SELECT COALESCE(
                (SELECT principal_id FROM principal_aliases WHERE alias = ?1),
                (SELECT principal_id FROM principals WHERE principal_id = ?1))",
            params![author],
            |r| r.get::<_, Option<String>>(0),
        )
    }

    /// The (human, device, agent) triple a row written by `author` carries.
    /// Walks the parent links: an agent sits under a device under a human; a
    /// device under a human; a human is its own scope.
    pub fn scope_ids_for_author(&self, author: &str) -> rusqlite::Result<ScopeIds> {
        Self::scope_ids_for_author_locked(&self.conn(), author)
    }

    pub fn scope_ids_for_author_locked(conn: &Connection, author: &str) -> rusqlite::Result<ScopeIds> {
        let Some(id) = Self::resolve_author_locked(conn, author)? else {
            return Ok(ScopeIds::default());
        };
        let Some(p) = Self::get_principal_locked(conn, &id)? else {
            return Ok(ScopeIds::default());
        };
        Ok(match p.kind {
            PrincipalKind::Human | PrincipalKind::Org => ScopeIds { principal_id: Some(p.principal_id), device_id: None, agent_id: None },
            PrincipalKind::Device => ScopeIds { principal_id: p.parent_id.clone(), device_id: Some(p.principal_id), agent_id: None },
            PrincipalKind::Agent => {
                let device = p.parent_id.clone();
                let human = match device.as_deref() {
                    Some(d) => Self::get_principal_locked(conn, d)?.and_then(|dp| dp.parent_id),
                    None => None,
                };
                ScopeIds { principal_id: human, device_id: device, agent_id: Some(p.principal_id) }
            }
        })
    }

    /// Every author string the lake has ever seen — ledger authors plus the
    /// catalog's `curated_by` — the set the alias table must cover.
    pub fn distinct_authors(&self) -> rusqlite::Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT author FROM ledger_events
             UNION SELECT DISTINCT curated_by FROM class_nodes WHERE curated_by IS NOT NULL AND curated_by <> ''
             ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// The chain head as `(seq, entry_hash)`; `(0, GENESIS_PREV)` when empty.
    pub fn chain_head(&self) -> rusqlite::Result<(i64, String)> {
        let conn = self.conn();
        let head: Option<(i64, String)> = conn
            .query_row(
                "SELECT seq, entry_hash FROM ledger_events ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(head.unwrap_or((0, polis_core::ledger::GENESIS_PREV.to_string())))
    }

    /// The seq of the `principal_bind` event for a device, if one was ever
    /// appended — the idempotency key of adoption.
    pub fn bind_seq_for(&self, device_id: &str) -> rusqlite::Result<Option<i64>> {
        self.conn()
            .query_row(
                "SELECT seq FROM ledger_events
                 WHERE kind = 'principal_bind' AND ref_kind = 'principal' AND ref_id = ?1
                 ORDER BY seq DESC LIMIT 1",
                params![device_id],
                |r| r.get(0),
            )
            .optional()
    }

    /// Stamp every row whose scope is still NULL from its author, resolved
    /// through the alias table: prompts / browse events / notes /
    /// observations by their ledger event's author, class nodes by
    /// `curated_by`. Idempotent and incremental (the partial `_unscoped`
    /// indexes make the sweep O(unstamped rows)); an author nobody has
    /// aliased leaves its rows NULL for a later sweep. This is both the
    /// one-time backfill after adoption and the post-write stamp.
    pub fn stamp_unscoped(&self) -> rusqlite::Result<StampReport> {
        let conn = self.conn();
        let mut report = StampReport::default();
        let authors: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT le.author FROM ledger_events le
                 UNION SELECT DISTINCT curated_by FROM class_nodes WHERE principal_id IS NULL AND curated_by IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for author in authors {
            let ids = Self::scope_ids_for_author_locked(&conn, &author)?;
            if ids.principal_id.is_none() && ids.device_id.is_none() && ids.agent_id.is_none() {
                continue;
            }
            let (h, d, a) = (&ids.principal_id, &ids.device_id, &ids.agent_id);
            report.prompts += conn.execute(
                "UPDATE prompts SET principal_id = ?1, device_id = ?2, agent_id = ?3,
                        run_id = COALESCE(run_id, claude_session_id)
                 WHERE principal_id IS NULL
                   AND id IN (SELECT prompt_id FROM ledger_events WHERE kind = 'prompt' AND author = ?4)",
                params![h, d, a, author],
            )?;
            report.browse_events += conn.execute(
                "UPDATE browse_events SET principal_id = ?1, device_id = ?2, agent_id = ?3
                 WHERE principal_id IS NULL
                   AND id IN (SELECT CAST(ref_id AS INTEGER) FROM ledger_events
                              WHERE ref_kind = 'browse_event' AND author = ?4)",
                params![h, d, a, author],
            )?;
            report.user_notes += conn.execute(
                "UPDATE user_notes SET principal_id = ?1, device_id = ?2, agent_id = ?3
                 WHERE principal_id IS NULL
                   AND seq IN (SELECT seq FROM ledger_events WHERE kind = 'note' AND author = ?4)",
                params![h, d, a, author],
            )?;
            report.class_observations += conn.execute(
                "UPDATE class_observations SET principal_id = ?1, device_id = ?2, agent_id = ?3
                 WHERE principal_id IS NULL
                   AND created_seq IN (SELECT seq FROM ledger_events WHERE kind = 'observation' AND author = ?4)",
                params![h, d, a, author],
            )?;
            report.class_nodes += conn.execute(
                "UPDATE class_nodes SET principal_id = ?1, device_id = ?2, agent_id = ?3
                 WHERE principal_id IS NULL AND curated_by = ?4",
                params![h, d, a, author],
            )?;
        }
        Ok(report)
    }

    /// How many rows per table still carry no scope — what a health report
    /// shows after adoption (zero once every author string is aliased).
    pub fn unscoped_counts(&self) -> rusqlite::Result<StampReport> {
        let conn = self.conn();
        let count = |t: &str| -> rusqlite::Result<usize> {
            conn.query_row(&format!("SELECT COUNT(*) FROM {t} WHERE principal_id IS NULL"), [], |r| r.get::<_, i64>(0))
                .map(|n| n as usize)
        };
        Ok(StampReport {
            prompts: count("prompts")?,
            browse_events: count("browse_events")?,
            user_notes: count("user_notes")?,
            class_nodes: count("class_nodes")?,
            class_observations: count("class_observations")?,
        })
    }

    /// The id set a `principal` scope value stands for: the id itself, its
    /// children and grandchildren (a human → their devices → their agents),
    /// so `scope.principal = <human>` reads everything of theirs.
    pub fn principal_id_set(&self, id_or_alias: &str) -> rusqlite::Result<Vec<String>> {
        Self::principal_id_set_locked(&self.conn(), id_or_alias)
    }

    pub fn principal_id_set_locked(conn: &Connection, id_or_alias: &str) -> rusqlite::Result<Vec<String>> {
        let root = Self::resolve_author_locked(conn, id_or_alias)?.unwrap_or_else(|| id_or_alias.to_string());
        let mut out = vec![root.clone()];
        let mut stmt = conn.prepare("SELECT principal_id FROM principals WHERE parent_id = ?1")?;
        let children: Vec<String> = stmt.query_map(params![root], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for c in &children {
            let grand: Vec<String> = stmt.query_map(params![c], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            out.extend(grand);
        }
        out.extend(children);
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The scope clause for a table alias whose rows carry the scope columns
    /// (`p` for prompts, `be` for browse events). Every value is bound.
    pub fn scope_clause(&self, table_alias: &str, f: &ScopeFilter) -> rusqlite::Result<ScopeSql> {
        Self::scope_clause_locked(&self.conn(), table_alias, f)
    }

    /// The same clause for a caller that already holds the connection (the
    /// read paths lock once; the mutex is not reentrant).
    pub fn scope_clause_locked(conn: &Connection, table_alias: &str, f: &ScopeFilter) -> rusqlite::Result<ScopeSql> {
        let mut sql = String::new();
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(p) = f.principal.as_deref().filter(|s| !s.trim().is_empty()) {
            let ids = Self::principal_id_set_locked(conn, p.trim())?;
            let marks = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(", ");
            sql.push_str(&format!(
                " AND ({t}.principal_id IN ({marks}) OR {t}.device_id IN ({marks}) OR {t}.agent_id IN ({marks}))",
                t = table_alias
            ));
            for _ in 0..3 {
                for id in &ids {
                    binds.push(Box::new(id.clone()));
                }
            }
        }
        if let Some(a) = f.agent.as_deref().filter(|s| !s.trim().is_empty()) {
            let id = Self::resolve_author_locked(conn, a.trim())?.unwrap_or_else(|| a.trim().to_string());
            sql.push_str(&format!(" AND {table_alias}.agent_id = ?"));
            binds.push(Box::new(id));
        }
        if let Some(r) = f.run.as_deref().filter(|s| !s.trim().is_empty()) {
            sql.push_str(&format!(" AND {table_alias}.run_id = ?"));
            binds.push(Box::new(r.trim().to_string()));
        }
        if let Some(o) = f.org.as_deref().filter(|s| !s.trim().is_empty()) {
            sql.push_str(&format!(" AND {table_alias}.org_id = ?"));
            binds.push(Box::new(o.trim().to_string()));
        }
        if let Some(project) = f.project.as_deref().filter(|s| !s.trim().is_empty()) {
            if table_alias == "n" {
                sql.push_str(" AND EXISTS (WITH RECURSIVE scope_ancestors(id,parent_id,project_path) AS (SELECT id,parent_id,project_path FROM class_nodes WHERE id = n.id UNION SELECT cn.id,cn.parent_id,cn.project_path FROM class_nodes cn JOIN scope_ancestors a ON cn.id=a.parent_id) SELECT 1 FROM scope_ancestors WHERE project_path = ?)");
            } else {
                sql.push_str(&format!(" AND {table_alias}.project_path = ?"));
            }
            binds.push(Box::new(project.trim().to_string()));
        }
        if table_alias == "p" {
            if !f.roles.is_empty() {
                let marks = vec!["?"; f.roles.len()].join(",");
                sql.push_str(&format!(" AND COALESCE(p.role, 'user') IN ({marks})"));
                for role in &f.roles { binds.push(Box::new(role.clone())); }
            }
            if let Some(ts) = f.after { sql.push_str(" AND p.ts >= ?"); binds.push(Box::new(ts)); }
            if let Some(ts) = f.before { sql.push_str(" AND p.ts < ?"); binds.push(Box::new(ts)); }
        } else {
            let timestamp = match table_alias { "be" => Some("be.ts"), "un" => Some("un.updated_at"), "o" => Some("o.created_at"), _ => None };
            if let Some(column) = timestamp {
                if let Some(ts) = f.after { sql.push_str(&format!(" AND {column} >= ?")); binds.push(Box::new(ts)); }
                if let Some(ts) = f.before { sql.push_str(&format!(" AND {column} < ?")); binds.push(Box::new(ts)); }
            }
        }
        Ok(ScopeSql { sql, binds })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_core::identity::{agent_id, device_id, principal_id};

    fn principal(id: &str, kind: PrincipalKind, parent: Option<&str>) -> Principal {
        Principal { principal_id: id.into(), kind, pubkey: None, parent_id: parent.map(String::from), display_name: None, created_at: 1 }
    }

    #[test]
    fn the_tables_exist_and_the_scope_columns_are_on_all_five() {
        let store = PolisStore::open_in_memory().unwrap();
        assert_eq!(store.meta(crate::meta::SCHEMA_VERSION_KEY).unwrap().as_deref(), Some(crate::meta::STORE_SCHEMA_VERSION));
        let conn = store.conn();
        for table in ["prompts", "browse_events", "user_notes", "class_nodes", "class_observations"] {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
            let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap().collect::<Result<_, _>>().unwrap();
            for c in ["principal_id", "device_id", "agent_id", "run_id", "org_id", "visibility"] {
                assert!(cols.contains(&c.to_string()), "{table} lacks {c}");
            }
        }
        for t in ["principals", "principal_aliases"] {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = ?1", params![t], |r| r.get(0)).unwrap();
            assert_eq!(n, 1, "{t} missing");
        }
    }

    #[test]
    fn resolution_walks_agent_to_device_to_human_and_the_scope_set_reads_downward() {
        let store = PolisStore::open_in_memory().unwrap();
        let pk = [3u8; 32];
        let (h, d, a) = (principal_id(&pk), device_id(&pk, "box"), agent_id(&pk, "keeper"));
        store.upsert_principal(&principal(&h, PrincipalKind::Human, None)).unwrap();
        store.upsert_principal(&principal(&d, PrincipalKind::Device, Some(&h))).unwrap();
        store.upsert_principal(&principal(&a, PrincipalKind::Agent, Some(&d))).unwrap();
        assert!(store.set_alias("keeper", &a).unwrap());
        assert!(!store.set_alias("keeper", &d).unwrap(), "an alias is insert-only");
        store.set_alias("yusuf", &d).unwrap();
        assert_eq!(store.scope_ids_for_author("keeper").unwrap(), ScopeIds { principal_id: Some(h.clone()), device_id: Some(d.clone()), agent_id: Some(a.clone()) });
        assert_eq!(store.scope_ids_for_author("yusuf").unwrap(), ScopeIds { principal_id: Some(h.clone()), device_id: Some(d.clone()), agent_id: None });
        assert_eq!(store.scope_ids_for_author(&h).unwrap(), ScopeIds { principal_id: Some(h.clone()), device_id: None, agent_id: None });
        assert_eq!(store.scope_ids_for_author("nobody").unwrap(), ScopeIds::default());
        let mut set = store.principal_id_set(&h).unwrap();
        set.sort();
        let mut want = vec![h.clone(), d.clone(), a.clone()];
        want.sort();
        assert_eq!(set, want);
        let clause = store.scope_clause("p", &ScopeFilter { principal: Some("yusuf".into()), ..Default::default() }).unwrap();
        assert!(clause.sql.contains("p.principal_id IN (?, ?, ?)") || clause.sql.contains("p.principal_id IN (?"));
        assert_eq!(clause.binds.len() % 3, 0);
    }

    #[test]
    fn stamping_fills_only_null_rows_from_the_ledger_author_and_is_idempotent() {
        let store = PolisStore::open_in_memory().unwrap();
        let pk = [9u8; 32];
        let (h, d) = (principal_id(&pk), device_id(&pk, "box"));
        store.upsert_principal(&principal(&h, PrincipalKind::Human, None)).unwrap();
        store.upsert_principal(&principal(&d, PrincipalKind::Device, Some(&h))).unwrap();
        // two prompts by the store's default author ("local" via the alias)
        for body in ["one", "two"] {
            crate::record::record_prompt(
                &store,
                crate::record::PromptInput {
                    source: polis_core::ledger::PromptSource::Api,
                    origin: polis_core::ledger::Origin::External,
                    surface: "api".into(),
                    role: polis_core::ledger::CorpusRole::User,
                    session_id: None,
                    claude_session_id: Some("run-1".into()),
                    mission_id: None,
                    project_path: None,
                    body: body.into(),
                    thread: None,
                    author: Some("local".into()),
                    model: None,
                    model_source: None,
                    user_text: None,
                },
            )
            .unwrap();
        }
        // Legacy evidence captured before an alias existed is backfilled.
        store.set_alias("local", &d).unwrap();
        let r = store.stamp_unscoped().unwrap();
        assert_eq!(r.prompts, 2);
        let again = store.stamp_unscoped().unwrap();
        assert_eq!(again.total(), 0, "a second sweep finds nothing");
        let conn = store.conn();
        let (p, dv, run): (String, String, String) = conn
            .query_row("SELECT principal_id, device_id, run_id FROM prompts WHERE body = 'one'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!((p, dv, run), (h, d, "run-1".to_string()));
    }
}
