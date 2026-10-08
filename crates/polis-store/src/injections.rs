// SPDX-License-Identifier: Apache-2.0
//! What the capture hook injected into a harness session, and the lexical
//! statistics its relevance floor reads.
//!
//! `polis_injections` is local bookkeeping, not evidence: it is never
//! hashed, never synced, and keeps a week. It is created on the first
//! injection, not by the migration: a host that never injects (Redline)
//! never grows the table, and the shared schema stays what it attached. It lets a later assistant turn
//! that merely restates injected memory be recognized as an echo rather than
//! recorded as fresh evidence, and lets `polis inspect` show what was pushed.
use std::collections::HashMap;

use rusqlite::{params, Connection};

use crate::PolisStore;

/// The records term statistics count: user and assistant text that is not a
/// user's question.
const ANSWERING: &str = "COALESCE(p.role, 'user') IN ('user', 'assistant')
    AND NOT (COALESCE(p.role, 'user') = 'user' AND rtrim(COALESCE(p.body, '')) LIKE '%?')";

/// How long an injection row is kept.
pub const INJECTION_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'polis_injections')", [], |r| r.get(0))
}

pub fn ensure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS polis_injections (
            id INTEGER PRIMARY KEY,
            session TEXT NOT NULL,
            seq INTEGER NOT NULL,
            at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS polis_injections_session ON polis_injections(session, at);",
    )
}

/// Document frequencies of planned query terms over the records that could
/// answer a prompt, and which of those terms each candidate source contains —
/// through the same FTS5 tokenizer retrieval matched with, so stemming agrees.
///
/// "Could answer" is the candidate set's own rule: user and assistant text,
/// not a user's question. Counting questions would make the topics a user
/// keeps asking about look common, and so never worth injecting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TermStats {
    /// Prompt rows in the corpus.
    pub docs: i64,
    /// One count per input term, in order.
    pub df: Vec<i64>,
    /// Per candidate seq: one flag per input term, in order.
    pub present: HashMap<i64, Vec<bool>>,
}

fn phrase(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

impl PolisStore {
    /// Remember that these seqs were injected into `session` at `at`, and
    /// drop rows past retention.
    pub fn record_injection(&self, session: &str, seqs: &[i64], at: i64) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        ensure(&conn)?;
        let tx = conn.transaction()?;
        for seq in seqs {
            tx.execute("INSERT INTO polis_injections(session, seq, at) VALUES (?1, ?2, ?3)", params![session, seq, at])?;
        }
        tx.execute("DELETE FROM polis_injections WHERE at < ?1", params![at - INJECTION_RETENTION_MS])?;
        tx.commit()
    }

    /// The bodies injected into `session` (newest first, at most `limit`),
    /// as `(seq, body)`. A source forgotten since has no body and is skipped.
    pub fn injected_bodies(&self, session: &str, limit: usize) -> rusqlite::Result<Vec<(i64, String)>> {
        let conn = self.conn();
        if !exists(&conn)? {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT i.seq, p.body FROM polis_injections i
             JOIN ledger_events le ON le.seq = i.seq
             JOIN prompts p ON p.id = le.prompt_id
             WHERE i.session = ?1 AND p.body IS NOT NULL AND p.body != ''
             GROUP BY i.seq ORDER BY MAX(i.at) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![session, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Injection rows, newest first: `(session, seq, at)`.
    pub fn recent_injections(&self, limit: usize) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let conn = self.conn();
        if !exists(&conn)? {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare("SELECT session, seq, at FROM polis_injections ORDER BY at DESC, id DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// The injection floor's candidates: user and assistant prompt events
    /// matching ANY planned term (`or_match`, the planner's OR stage),
    /// recorded strictly before `before`, under `scope`, best BM25 first.
    ///
    /// Not the answer pack's cascade, on purpose: the cascade stops at its
    /// AND stage whenever anything contains every term, and an earlier
    /// asking of the same question always does — so the record that answers
    /// it, sharing only some of the words, would never be considered.
    pub fn injection_candidates(
        &self,
        or_match: &str,
        before: Option<i64>,
        limit: i64,
        scope: &crate::principals::ScopeFilter,
    ) -> rusqlite::Result<Vec<polis_core::types::LakeItem>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "p", scope)?.numbered(4);
        let mut stmt = conn.prepare(&format!(
            "SELECT le.seq, le.ts, le.kind, le.ref_kind, le.ref_id, le.session_id,
                    p.surface, p.origin, p.role, p.mission_id, p.project_path,
                    COALESCE(NULLIF(p.body, ''), p.gist),
                    p.thread_kind, p.thread_id, p.parent_session_id, p.model
             FROM prompts_fts
             JOIN prompts p ON p.id = prompts_fts.rowid
             JOIN ledger_events le ON le.prompt_id = p.id
             WHERE prompts_fts MATCH ?1 AND le.kind = 'prompt'
               AND COALESCE(p.role, 'user') IN ('user', 'assistant')
               AND (?3 IS NULL OR le.ts < ?3){}
             ORDER BY bm25(prompts_fts, 3.0, 1.0) LIMIT ?2",
            scoped.sql
        ))?;
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&or_match, &limit, &before];
        for b in &scoped.binds {
            binds.push(b.as_ref());
        }
        let rows = stmt.query_map(binds.as_slice(), Self::row_to_lake_item_full)?;
        rows.collect()
    }

    /// See [`TermStats`]. Candidates that are not prompt events report no
    /// terms present.
    pub fn term_statistics(&self, terms: &[String], seqs: &[i64]) -> rusqlite::Result<TermStats> {
        let conn = self.conn();
        let docs: i64 = conn.query_row(&format!("SELECT count(*) FROM prompts p WHERE {ANSWERING}"), [], |r| r.get(0))?;
        let mut df = Vec::with_capacity(terms.len());
        {
            let mut stmt = conn.prepare(&format!(
                "SELECT count(*) FROM prompts_fts JOIN prompts p ON p.id = prompts_fts.rowid WHERE prompts_fts MATCH ?1 AND {ANSWERING}"
            ))?;
            for term in terms {
                df.push(stmt.query_row([phrase(term)], |r| r.get(0)).unwrap_or(0));
            }
        }
        let mut present = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT count(*) FROM prompts_fts f JOIN ledger_events le ON le.prompt_id = f.rowid
             WHERE le.seq = ?1 AND prompts_fts MATCH ?2",
        )?;
        for &seq in seqs {
            let flags = terms
                .iter()
                .map(|t| stmt.query_row(params![seq, phrase(t)], |r| r.get::<_, i64>(0)).map(|n| n > 0).unwrap_or(false))
                .collect();
            present.insert(seq, flags);
        }
        Ok(TermStats { docs, df, present })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{record_prompt, PromptInput};
    use polis_core::ledger::{CorpusRole, Origin, PromptSource};

    fn prompt(store: &PolisStore, body: &str) -> i64 {
        record_prompt(store, PromptInput {
            source: PromptSource::Api,
            origin: Origin::External,
            surface: "test".into(),
            role: CorpusRole::User,
            user_text: None,
            session_id: Some("s".into()),
            claude_session_id: Some("s".into()),
            mission_id: None,
            project_path: None,
            body: body.into(),
            thread: None,
            author: None,
            model: None,
            model_source: None,
        })
        .unwrap()
        .unwrap()
    }

    #[test]
    fn statistics_follow_the_index_tokenizer() {
        let store = PolisStore::open_in_memory().unwrap();
        let a = prompt(&store, "run the tests before the release");
        let b = prompt(&store, "the release uses cargo build --frozen");
        let terms = vec!["test".to_string(), "release".to_string(), "--frozen".to_string()];
        prompt(&store, "do the release tests run before --frozen builds?");
        let stats = store.term_statistics(&terms, &[a, b]).unwrap();
        assert_eq!(stats.docs, 2, "a user's question is not counted");
        assert_eq!(stats.df, vec![1, 2, 1], "porter stemming: `test` counts `tests`");
        assert_eq!(stats.present[&a], vec![true, true, false]);
        assert_eq!(stats.present[&b], vec![false, true, true]);
    }

    #[test]
    fn injections_resolve_to_bodies_and_expire() {
        let store = PolisStore::open_in_memory().unwrap();
        let schema = store.schema_sql().unwrap();
        assert!(!schema.contains("polis_injections"), "the migration never creates it");
        assert!(store.injected_bodies("sess", 10).unwrap().is_empty() && store.recent_injections(10).unwrap().is_empty());
        assert!(!store.schema_sql().unwrap().contains("polis_injections"), "reads never create it");
        let a = prompt(&store, "the API port is 9090");
        let b = prompt(&store, "Bluebird runs in eu-west-2");
        store.record_injection("sess", &[a], 1_000).unwrap();
        store.record_injection("sess", &[b, a], 2_000).unwrap();
        store.record_injection("other", &[b], 2_000).unwrap();
        let bodies = store.injected_bodies("sess", 10).unwrap();
        assert_eq!(bodies.iter().map(|(s, _)| *s).collect::<Vec<_>>().len(), 2);
        assert!(bodies.iter().any(|(s, body)| *s == a && body == "the API port is 9090"));
        assert!(store.injected_bodies("nobody", 10).unwrap().is_empty());
        store.record_injection("late", &[a], 2_000 + INJECTION_RETENTION_MS + 1).unwrap();
        assert!(store.injected_bodies("sess", 10).unwrap().is_empty(), "past retention");
        assert_eq!(store.recent_injections(10).unwrap(), vec![("late".to_string(), a, 2_000 + INJECTION_RETENTION_MS + 1)]);
    }
}
