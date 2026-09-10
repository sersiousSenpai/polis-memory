// SPDX-License-Identifier: Apache-2.0
//! Scoped session views derive labels and counts from permitted ledger sources.
use crate::{principals::ScopeFilter, PolisStore};
use polis_core::types::{ClassRun, LakeItem};

#[derive(Debug, Clone)]
pub struct ThreadDigest {
    pub kind: String,
    pub id: String,
    pub count: i64,
    pub last_ts: i64,
}

impl PolisStore {
    pub fn thread_items_scoped(
        &self,
        kind: &str,
        id: &str,
        limit: i64,
        scope: &ScopeFilter,
    ) -> rusqlite::Result<Vec<LakeItem>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "p", scope)?.numbered(4);
        let mut stmt = conn.prepare(&format!("SELECT le.seq,le.ts,le.kind,le.ref_kind,le.ref_id,le.session_id,p.surface,p.origin,p.role,p.mission_id,p.project_path,{},p.thread_kind,p.thread_id,p.parent_session_id,p.model FROM ledger_events le JOIN prompts p ON p.id=le.prompt_id WHERE le.kind='prompt' AND ((?1='session' AND (p.session_id=?2 OR p.claude_session_id=?2)) OR (p.thread_kind=?1 AND p.thread_id=?2)){} ORDER BY le.seq DESC LIMIT ?3", crate::PROMPT_TEXT, scoped.sql))?;
        let limit = limit.clamp(1, 200);
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&kind, &id, &limit];
        binds.extend(scoped.binds.iter().map(|v| v.as_ref()));
        let mut rows = stmt
            .query_map(binds.as_slice(), Self::row_to_lake_item)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn thread_digests_scoped(
        &self,
        scope: &ScopeFilter,
    ) -> rusqlite::Result<Vec<ThreadDigest>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "p", scope)?;
        let mut stmt = conn.prepare(&format!("WITH eligible AS (SELECT p.* FROM prompts p WHERE 1=1{}) SELECT kind,id,COUNT(*),MAX(ts) FROM (SELECT 'session' AS kind,COALESCE(session_id,claude_session_id) AS id,ts FROM eligible WHERE COALESCE(session_id,claude_session_id) IS NOT NULL UNION ALL SELECT thread_kind,thread_id,ts FROM eligible WHERE thread_kind IS NOT NULL AND thread_id IS NOT NULL AND NOT(thread_kind='session' AND thread_id=COALESCE(session_id,claude_session_id,''))) GROUP BY kind,id ORDER BY kind,id", scoped.sql))?;
        let binds: Vec<&dyn rusqlite::ToSql> = scoped.binds.iter().map(|v| v.as_ref()).collect();
        let rows = stmt
            .query_map(binds.as_slice(), |r| {
                Ok(ThreadDigest {
                    kind: r.get(0)?,
                    id: r.get(1)?,
                    count: r.get(2)?,
                    last_ts: r.get(3)?,
                })
            })?
            .collect();
        rows
    }

    pub fn list_class_runs_scoped(
        &self,
        limit: i64,
        scope: &ScopeFilter,
    ) -> rusqlite::Result<Vec<ClassRun>> {
        if scope.is_empty() {
            return self.list_class_runs(limit);
        }
        let conn = self.conn();
        let scoped = Self::ledger_scope_clause_locked(&conn, "le", scope)?;
        let mut stmt = conn.prepare(&format!("SELECT {} FROM class_runs cr WHERE EXISTS (SELECT 1 FROM ledger_events le WHERE le.seq>COALESCE(cr.seq_from,0) AND le.seq<=COALESCE(cr.seq_to,9223372036854775807){}) ORDER BY id DESC LIMIT ?", Self::CLASS_RUN_COLS, scoped.sql))?;
        let limit = limit.max(1);
        let mut binds: Vec<&dyn rusqlite::ToSql> =
            scoped.binds.iter().map(|v| v.as_ref()).collect();
        binds.push(&limit);
        let mut rows = stmt
            .query_map(binds.as_slice(), Self::row_to_class_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // A summary/canary can discuss sources outside the window. Scoped
        // diagnostics retain timings and status, without free-form global text.
        for row in &mut rows {
            row.summary =
                Some("Scoped run metadata; full narrative requires store-wide inspection".into());
            row.error = row
                .error
                .as_ref()
                .map(|_| "Run failed; inspect store-wide diagnostics for details".into());
            row.canary_json = None;
            row.claude_session_id = None;
            row.items = None;
            row.ops = None;
        }
        Ok(rows)
    }
}
