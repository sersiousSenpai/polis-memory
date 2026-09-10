// SPDX-License-Identifier: Apache-2.0
//! Candidate scoring and hydration. Output limits are applied after eligibility.
use crate::PolisStore;
use polis_core::types::BrowseHit;
use rusqlite::params;
use std::collections::HashMap;

impl PolisStore {
    /// Score every eligible link against full source content without copying
    /// full bodies into a candidate pool. Insertion order never decides relevance.
    pub fn score_link_seqs(
        &self,
        seqs: &[i64],
        terms: &[String],
    ) -> rusqlite::Result<HashMap<i64, i64>> {
        let conn = self.conn();
        let mut scores = HashMap::new();
        for chunk in seqs.chunks(400) {
            let expression = if terms.is_empty() {
                "0".into()
            } else {
                terms
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        format!(
                            "(instr(lower(COALESCE(NULLIF(p.body,''),p.gist,'')),?{})>0)",
                            i + 1
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("+")
            };
            let marks = (0..chunk.len())
                .map(|i| format!("?{}", terms.len() + i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let mut stmt = conn.prepare(&format!("SELECT le.seq, {expression} FROM ledger_events le LEFT JOIN decision_evidence de ON de.seq=le.seq LEFT JOIN ledger_events src ON src.seq=de.source_seq AND src.kind='prompt' LEFT JOIN prompts p ON p.id=COALESCE(le.prompt_id,src.prompt_id) AND (le.kind='prompt' OR (le.payload_hash=p.body_hash AND le.ref_kind='prompt' AND le.ref_id=CAST(p.id AS TEXT))) WHERE le.seq IN ({marks})"))?;
            let mut refs: Vec<&dyn rusqlite::ToSql> =
                terms.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            refs.extend(chunk.iter().map(|n| n as &dyn rusqlite::ToSql));
            scores.extend(
                stmt.query_map(refs.as_slice(), |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(scores)
    }

    pub fn browse_hits_for_ids(
        &self,
        ids: &[i64],
        terms: &[String],
    ) -> rusqlite::Result<Vec<BrowseHit>> {
        let conn = self.conn();
        let mut out = Vec::new();
        let mut stmt = conn.prepare("SELECT be.id,le.seq,be.ts,be.url,be.title,COALESCE(be.text,''),be.shot_key,be.caption FROM browse_events be JOIN ledger_events le ON le.ref_kind='browse_event' AND le.ref_id=CAST(be.id AS TEXT) WHERE be.id=?1")?;
        for id in ids {
            let rows = stmt.query_map(params![id], |r| {
                let text: String = r.get(5)?;
                Ok(BrowseHit {
                    id: r.get(0)?,
                    seq: r.get(1)?,
                    ts: r.get(2)?,
                    url: r.get(3)?,
                    title: r.get(4)?,
                    snippet: polis_core::dedup::excerpt_around(&text, terms, 800),
                    score: 0.0,
                    stage: "semantic".into(),
                    shot_key: r.get(6)?,
                    caption: r.get(7)?,
                })
            })?;
            out.extend(rows.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    pub fn neighboring_prompt_seqs(
        &self,
        anchor: i64,
        session: &str,
        scope: &crate::principals::ScopeFilter,
    ) -> rusqlite::Result<Vec<i64>> {
        let conn = self.conn();
        let scoped = Self::scope_clause_locked(&conn, "p", scope)?.numbered(3);
        let mut out = Vec::new();
        for (op, order) in [("<", "DESC"), (">", "ASC")] {
            let mut stmt=conn.prepare(&format!("SELECT le.seq FROM ledger_events le JOIN prompts p ON p.id=le.prompt_id WHERE le.kind='prompt' AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources) AND le.session_id=?1 AND le.seq {op} ?2{} ORDER BY le.seq {order} LIMIT 1",scoped.sql))?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&session, &anchor];
            binds.extend(scoped.binds.iter().map(|v| v.as_ref()));
            out.extend(
                stmt.query_map(binds.as_slice(), |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        Ok(out)
    }

    /// Scoped live source targets with current-model/source-hash vectors.
    /// Counts only lexical source targets; catalog vectors are auxiliary.
    pub fn scoped_index_readiness(
        &self,
        model: &str,
        scope: &crate::principals::ScopeFilter,
    ) -> rusqlite::Result<(i64, i64)> {
        let conn = self.conn();
        let mut total = 0i64;
        let mut indexed = 0i64;
        for (table, alias, kind, hash, live) in [
            (
                "prompts",
                "p",
                "prompt",
                "body_hash",
                "length(p.fts_text)>0 AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources)",
            ),
            (
                "browse_events",
                "be",
                "browse_event",
                "context_hash",
                "length(be.text)>0",
            ),
        ] {
            let scoped = Self::scope_clause_locked(&conn, alias, scope)?.numbered(2);
            let mut stmt=conn.prepare(&format!("SELECT COUNT(*),COALESCE(SUM(EXISTS(SELECT 1 FROM embeddings e WHERE e.model=?1 AND e.target_kind='{kind}' AND e.target_id={alias}.id AND e.source_hash={alias}.{hash})),0) FROM {table} {alias} WHERE {live}{}",scoped.sql))?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&model];
            binds.extend(scoped.binds.iter().map(|v| v.as_ref()));
            let counts = stmt.query_row(binds.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            total += counts.0;
            indexed += counts.1;
        }
        Ok((total, indexed))
    }

    pub fn snapshot_head(&self) -> rusqlite::Result<(i64, String)> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn()
            .query_row(
                "SELECT seq,entry_hash FROM ledger_events ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or_default())
    }
}
