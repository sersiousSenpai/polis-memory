// SPDX-License-Identifier: Apache-2.0
//! Rebuildable decision-to-source references. Bodies live in the cited prompt,
//! whose hash must match the decision's payload hash; legacy gaps stay explicit.
use crate::{principals::ScopeFilter, PolisStore};
use polis_core::{
    ledger::{now_millis, LedgerAppend},
    query::MatchStage,
    types::LakeItem,
};
use rusqlite::{params, Connection, OptionalExtension};

pub fn ensure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS decision_evidence(seq INTEGER PRIMARY KEY,source_seq INTEGER NOT NULL,recorded_at INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS decision_evidence_source ON decision_evidence(source_seq);
        INSERT OR IGNORE INTO decision_evidence(seq,source_seq,recorded_at)
        SELECT d.seq,MIN(s.seq),d.ts FROM ledger_events d JOIN prompts p ON d.ref_kind='prompt' AND d.ref_id=CAST(p.id AS TEXT) AND d.payload_hash=p.body_hash
        JOIN ledger_events s ON s.kind='prompt' AND s.prompt_id=p.id
        WHERE d.kind IN ('decision','resolution','approval','review_verdict') GROUP BY d.seq;")
}
impl PolisStore {
    /// Record a decision over an existing, accessible source. Hash commitment
    /// and projection are committed in the same SQLite transaction.
    pub fn record_cited_decision(
        &self,
        kind: &str,
        source_seq: i64,
        scope: &ScopeFilter,
    ) -> rusqlite::Result<i64> {
        if !matches!(
            kind,
            "decision" | "resolution" | "approval" | "review_verdict"
        ) {
            return Err(rusqlite::Error::InvalidParameterName(
                "unsupported decision kind".into(),
            ));
        }
        if !self
            .eligible_seqs(&[source_seq], scope)?
            .contains(&source_seq)
        {
            return Err(rusqlite::Error::InvalidParameterName(
                "decision source outside scope".into(),
            ));
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (prompt_id,hash,session,author):(i64,String,Option<String>,String)=tx.query_row("SELECT p.id,p.body_hash,s.session_id,s.author FROM ledger_events s JOIN prompts p ON p.id=s.prompt_id WHERE s.seq=?1 AND s.kind='prompt' AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources)",[source_seq],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        let reference = prompt_id.to_string();
        let existing:Option<i64>=tx.query_row("SELECT seq FROM ledger_events WHERE kind=?1 AND ref_kind='prompt' AND ref_id=?2 AND payload_hash=?3",params![kind,reference,hash],|r|r.get(0)).optional()?;
        if let Some(seq) = existing {
            return Ok(seq);
        }
        let ts = now_millis();
        let row = crate::ledger::append_event(
            &tx,
            &LedgerAppend {
                kind,
                author: &author,
                ts,
                prompt_id: None,
                session_id: session.as_deref(),
                version_number: None,
                ref_kind: Some("prompt"),
                ref_id: Some(&reference),
                payload_hash: &hash,
            },
        )?;
        tx.execute(
            "INSERT INTO decision_evidence(seq,source_seq,recorded_at) VALUES(?1,?2,?3)",
            params![row.seq, source_seq, ts],
        )?;
        tx.commit()?;
        Ok(row.seq)
    }

    pub fn decision_items_scoped(
        &self,
        query: Option<&str>,
        seqs: Option<&[i64]>,
        limit: usize,
        scope: &ScopeFilter,
    ) -> rusqlite::Result<Vec<(LakeItem, MatchStage)>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "p", scope)?;
        let mut sql=format!("SELECT d.seq,d.ts,d.kind,d.ref_kind,d.ref_id,d.session_id,p.surface,p.origin,p.role,p.mission_id,p.project_path,{},p.thread_kind,p.thread_id,p.parent_session_id,p.model FROM decision_evidence de JOIN ledger_events d ON d.seq=de.seq JOIN ledger_events src ON src.seq=de.source_seq AND src.kind='prompt' JOIN prompts p ON p.id=src.prompt_id WHERE d.ref_kind='prompt' AND d.ref_id=CAST(p.id AS TEXT) AND d.payload_hash=p.body_hash AND p.id NOT IN(SELECT prompt_id FROM forgotten_sources){}",crate::PROMPT_TEXT,scoped.sql);
        let mut binds = scoped.binds;
        if let Some(seqs) = seqs {
            if seqs.is_empty() {
                return Ok(Vec::new());
            }
            sql.push_str(&format!(
                " AND d.seq IN ({})",
                vec!["?"; seqs.len()].join(",")
            ));
            binds.extend(
                seqs.iter()
                    .map(|s| Box::new(*s) as Box<dyn rusqlite::ToSql>),
            );
        }
        if let Some(q) = query {
            let Some(plan) = polis_core::query::plan_fts_query(q) else {
                return Ok(Vec::new());
            };
            let mut terms = plan.terms;
            terms.extend(plan.phrases);
            sql.push_str(" AND (");
            sql.push_str(
                &terms
                    .iter()
                    .map(|_| format!("instr(lower({}),?)>0", crate::PROMPT_TEXT))
                    .collect::<Vec<_>>()
                    .join(" OR "),
            );
            sql.push(')');
            binds.extend(
                terms
                    .into_iter()
                    .map(|t| Box::new(t) as Box<dyn rusqlite::ToSql>),
            );
        }
        sql.push_str(" ORDER BY d.seq DESC LIMIT ?");
        binds.push(Box::new(limit.min(200) as i64));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(binds.iter().map(|v| v.as_ref())),
                |r| Ok((Self::row_to_lake_item_full(r)?, MatchStage::Or)),
            )?
            .collect();
        rows
    }
}
