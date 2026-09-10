// SPDX-License-Identifier: Apache-2.0
//! Bounded local diagnostic storage, separate from the semantic corpus.
use crate::PolisStore;
use polis_core::diagnostics::RetrievalTrace;
use rusqlite::{params, Connection};

pub fn ensure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS polis_traces (
        id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, payload TEXT NOT NULL
    ); CREATE INDEX IF NOT EXISTS polis_traces_time ON polis_traces(started_at DESC);",
    )
}

impl PolisStore {
    pub fn save_retrieval_trace(&self, trace: &RetrievalTrace) -> rusqlite::Result<()> {
        let payload = serde_json::to_string(trace)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        if payload.len() > 32_768 {
            return Err(rusqlite::Error::InvalidParameterName(
                "trace exceeds 32 KiB".into(),
            ));
        }
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("INSERT INTO polis_traces(id,started_at,payload) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload", params![trace.id, trace.started_at, payload])?;
        tx.execute("DELETE FROM polis_traces WHERE id NOT IN (SELECT id FROM polis_traces ORDER BY started_at DESC, id DESC LIMIT 1000)", [])?;
        tx.commit()
    }

    pub fn retrieval_traces(
        &self,
        id: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<RetrievalTrace>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT payload FROM polis_traces WHERE (?1 IS NULL OR id=?1) ORDER BY started_at DESC, id DESC LIMIT ?2")?;
        let rows = stmt
            .query_map(params![id, limit.clamp(1, 1000)], |r| {
                let value: String = r.get(0)?;
                serde_json::from_str(&value).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
