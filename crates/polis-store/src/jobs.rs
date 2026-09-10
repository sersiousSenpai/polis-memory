// SPDX-License-Identifier: Apache-2.0
//! Durable, leased background work. Idempotency keys survive crashes; expired
//! attempts are reclaimed, bounded retries end visibly, and stale workers are
//! fenced by owner plus attempt number on every checkpoint and completion.
use crate::PolisStore;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundJob {
    pub id: i64,
    pub kind: String,
    pub job_key: String,
    pub status: String,
    pub attempts: i64,
    pub max_attempts: i64,
    pub available_at: i64,
    pub lease_owner: Option<String>,
    pub lease_until: Option<i64>,
    pub checkpoint: Option<String>,
    pub error: Option<String>,
}
const COLS: &str = "id, kind, job_key, status, attempts, max_attempts, available_at, lease_owner, lease_until, checkpoint, error";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<BackgroundJob> {
    Ok(BackgroundJob {
        id: r.get(0)?,
        kind: r.get(1)?,
        job_key: r.get(2)?,
        status: r.get(3)?,
        attempts: r.get(4)?,
        max_attempts: r.get(5)?,
        available_at: r.get(6)?,
        lease_owner: r.get(7)?,
        lease_until: r.get(8)?,
        checkpoint: r.get(9)?,
        error: r.get(10)?,
    })
}
impl PolisStore {
    pub fn enqueue_job(
        &self,
        kind: &str,
        key: &str,
        now: i64,
        max_attempts: i64,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute("INSERT OR IGNORE INTO background_jobs(kind, job_key, available_at, max_attempts, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?3, ?3)",
            params![kind, key, now, max_attempts.clamp(1, 20)])?;
        conn.query_row(
            "SELECT id FROM background_jobs WHERE job_key = ?1",
            [key],
            |r| r.get(0),
        )
    }

    pub fn lease_job(
        &self,
        kind: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
    ) -> rusqlite::Result<Option<BackgroundJob>> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("UPDATE background_jobs SET status = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'pending' END,
            lease_owner = NULL, lease_until = NULL, error = 'lease expired', updated_at = ?1
            WHERE status = 'running' AND lease_until <= ?1", [now])?;
        let id: Option<i64> = tx.query_row("SELECT id FROM background_jobs WHERE kind = ?1 AND status = 'pending'
            AND available_at <= ?2 AND attempts < max_attempts
            AND NOT EXISTS (SELECT 1 FROM background_jobs running WHERE running.kind = ?1 AND running.status = 'running')
            ORDER BY available_at, id LIMIT 1", params![kind, now], |r| r.get(0)).optional()?;
        let result = if let Some(id) = id {
            tx.execute("UPDATE background_jobs SET status = 'running', attempts = attempts + 1, lease_owner = ?2,
                lease_until = ?3, updated_at = ?4 WHERE id = ?1", params![id, owner, now.saturating_add(lease_ms.clamp(1, 600_000)), now])?;
            Some(tx.query_row(
                &format!("SELECT {COLS} FROM background_jobs WHERE id = ?1"),
                [id],
                row,
            )?)
        } else {
            None
        };
        tx.commit()?;
        Ok(result)
    }

    pub fn checkpoint_job(
        &self,
        job: &BackgroundJob,
        checkpoint: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("UPDATE background_jobs SET checkpoint = ?4, updated_at = ?5
            WHERE id = ?1 AND lease_owner = ?2 AND attempts = ?3 AND status = 'running' AND lease_until > ?5",
            params![job.id, job.lease_owner, job.attempts, checkpoint, now])? > 0)
    }

    pub fn finish_job(
        &self,
        job: &BackgroundJob,
        error: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let status = if error.is_none() {
            "done"
        } else if job.attempts >= job.max_attempts {
            "failed"
        } else {
            "pending"
        };
        Ok(self.conn().execute("UPDATE background_jobs SET status = ?4, error = ?5, lease_owner = NULL,
            lease_until = NULL, available_at = ?6, updated_at = ?7
            WHERE id = ?1 AND lease_owner = ?2 AND attempts = ?3 AND status = 'running' AND lease_until > ?7",
            params![job.id, job.lease_owner, job.attempts, status, error, now.saturating_add(1000 * job.attempts), now])? > 0)
    }

    /// Explicitly retry a terminal failure; never changes completed work.
    pub fn retry_job(&self, id: i64, now: i64) -> rusqlite::Result<bool> {
        Ok(self.conn().execute(
            "UPDATE background_jobs SET status = 'pending', attempts = 0, error = NULL,
            available_at = ?2, updated_at = ?2 WHERE id = ?1 AND status = 'failed'",
            params![id, now],
        )? > 0)
    }

    pub fn list_jobs(&self, limit: usize) -> rusqlite::Result<Vec<BackgroundJob>> {
        let conn = self.conn();
        conn.execute("UPDATE background_jobs SET status = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'pending' END,
            lease_owner = NULL, lease_until = NULL, error = 'lease expired', updated_at = ?1
            WHERE status = 'running' AND lease_until <= ?1", [polis_core::ledger::now_millis()])?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM background_jobs ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = stmt.query_map([limit.min(1000) as i64], row)?.collect();
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crash_recovery_fences_old_attempt_and_keeps_checkpoint() {
        let store = PolisStore::open_in_memory().unwrap();
        let id = store.enqueue_job("index", "index:0:40", 100, 2).unwrap();
        assert_eq!(
            store.enqueue_job("index", "index:0:40", 100, 2).unwrap(),
            id
        );
        let a = store.lease_job("index", "a", 100, 20).unwrap().unwrap();
        assert!(store.lease_job("index", "b", 101, 20).unwrap().is_none());
        assert!(store.checkpoint_job(&a, "20", 110).unwrap());
        let b = store.lease_job("index", "b", 121, 20).unwrap().unwrap();
        assert_eq!(b.checkpoint.as_deref(), Some("20"));
        assert_eq!(b.attempts, 2);
        assert!(!store.finish_job(&a, None, 122).unwrap());
        assert!(store.finish_job(&b, Some("poisoned item"), 123).unwrap());
        assert_eq!(store.list_jobs(10).unwrap()[0].status, "failed");
        assert!(store.lease_job("index", "c", 9000, 20).unwrap().is_none());
        assert!(store.retry_job(id, 9000).unwrap());
        assert!(store.lease_job("index", "c", 9000, 20).unwrap().is_some());
    }
}
