// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Foreign chains (Session E3, plan §4.6): what a peer's signed segments
//! leave behind once they verify. Every row here is keyed by
//! `(chain_id, seq)` and is NEVER re-chained — the peer's `entry_hash` is the
//! identity, our chain never references it, and a re-import of the same
//! segment is a no-op by primary key. Trust, subscriptions and redaction
//! acknowledgements live beside the rows they guard, so a backup carries
//! them and `doctor` reads them with one query.

use polis_core::identity::{PrincipalCard, PrincipalKind};
use polis_core::ledger::LedgerEventRow;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::PolisStore;

/// The tombstone a redaction leaves in a foreign body.
pub const TOMBSTONE: &str = "[forgotten]";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignChain {
    pub chain_id: String,
    pub human_id: String,
    pub device_name: Option<String>,
    pub display_name: Option<String>,
    pub head_seq: i64,
    pub head_hash: String,
    pub forked: bool,
    pub fork_detail: Option<String>,
    pub first_import_at: i64,
    pub last_import_at: i64,
}

/// One foreign prompt as the FTS arm or the semantic arm returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignPromptRow {
    /// The local rowid — what an embedding row targets (`foreign_prompt`).
    pub id: i64,
    pub chain_id: String,
    pub seq: i64,
    pub prompt_id: i64,
    pub role: String,
    pub body_hash: String,
    pub redaction: String,
    pub text: Option<String>,
    pub project: Option<String>,
    pub tombstoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustEntry {
    pub principal_id: String,
    pub pubkey: String,
    pub fingerprint: String,
    /// `tofu` (first use, fingerprint printed) or `admin` (a distributed key).
    pub source: String,
    pub display_name: Option<String>,
    pub added_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionRow {
    pub id: i64,
    pub principal: Option<String>,
    pub project: Option<String>,
    pub class: Option<String>,
    pub created_at: i64,
}

/// What one segment leaves behind — the store's half of an import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedRows {
    pub events: usize,
    pub prompts: usize,
    pub notes: usize,
    pub principals: usize,
    pub tombstoned: usize,
    pub redactions: usize,
}

/// A prompt row to write, already at its redaction.
pub struct ForeignPromptInput<'a> {
    pub seq: i64,
    pub prompt_id: i64,
    pub role: &'a str,
    pub body_hash: &'a str,
    pub redaction: &'a str,
    pub text: Option<&'a str>,
    pub project: Option<&'a str>,
}

pub struct ForeignNoteInput<'a> {
    pub note_id: i64,
    pub seq: Option<i64>,
    pub target_kind: &'a str,
    pub target_id: Option<&'a str>,
    pub text: &'a str,
    pub created_at: i64,
}

/// A redaction the segment carries: the peer forgot `(target_chain,
/// target_seq)` at its own `event_seq`.
pub struct ForeignRedactionInput<'a> {
    pub event_seq: i64,
    pub target_chain: &'a str,
    pub target_seq: i64,
}

/// E4: a class node a peer published (its catalog rides its segments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignClassNode {
    pub chain_id: String,
    pub node_id: String,
    pub parent_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub summary: Option<String>,
}

/// E4: a pointer from a peer's class into the lake — a seq on its own chain,
/// or `chain:seq` on another's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignClassLink {
    pub chain_id: String,
    pub node_id: String,
    pub target_kind: String,
    pub target_id: String,
}

/// E4: a redaction some chain emitted, as the relay holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignRedactionRow {
    pub chain_id: String,
    pub event_seq: i64,
    pub target_chain: String,
    pub target_seq: i64,
}

fn row_to_chain(r: &rusqlite::Row) -> rusqlite::Result<ForeignChain> {
    Ok(ForeignChain {
        chain_id: r.get(0)?,
        human_id: r.get(1)?,
        device_name: r.get(2)?,
        display_name: r.get(3)?,
        head_seq: r.get(4)?,
        head_hash: r.get(5)?,
        forked: r.get::<_, i64>(6)? != 0,
        fork_detail: r.get(7)?,
        first_import_at: r.get(8)?,
        last_import_at: r.get(9)?,
    })
}

const CHAIN_COLS: &str = "chain_id, human_id, device_name, display_name, head_seq, head_hash, forked, fork_detail, first_import_at, last_import_at";

fn row_to_prompt(r: &rusqlite::Row) -> rusqlite::Result<ForeignPromptRow> {
    Ok(ForeignPromptRow {
        id: r.get(0)?,
        chain_id: r.get(1)?,
        seq: r.get(2)?,
        prompt_id: r.get(3)?,
        role: r.get(4)?,
        body_hash: r.get(5)?,
        redaction: r.get(6)?,
        text: r.get(7)?,
        project: r.get(8)?,
        tombstoned: r.get::<_, i64>(9)? != 0,
    })
}

const PROMPT_COLS: &str = "id, chain_id, seq, prompt_id, role, body_hash, redaction, text, project, tombstoned";

impl PolisStore {
    // ---- chains ----------------------------------------------------------

    pub fn get_foreign_chain(&self, chain_id: &str) -> rusqlite::Result<Option<ForeignChain>> {
        self.conn()
            .query_row(&format!("SELECT {CHAIN_COLS} FROM foreign_chains WHERE chain_id = ?1"), params![chain_id], row_to_chain)
            .optional()
    }

    pub fn list_foreign_chains(&self) -> rusqlite::Result<Vec<ForeignChain>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {CHAIN_COLS} FROM foreign_chains ORDER BY last_import_at DESC, chain_id"))?;
        let rows = stmt.query_map([], row_to_chain)?;
        rows.collect()
    }

    /// Record a fork: the peer rewrote history under a head we already hold.
    /// The rows we imported stay (they verified when they arrived); nothing
    /// newer from that chain lands until an operator clears the mark.
    pub fn mark_foreign_forked(&self, chain_id: &str, detail: &str) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "UPDATE foreign_chains SET forked = 1, fork_detail = ?2 WHERE chain_id = ?1",
            params![chain_id, detail],
        )?;
        Ok(n > 0)
    }

    pub fn clear_foreign_fork(&self, chain_id: &str) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "UPDATE foreign_chains SET forked = 0, fork_detail = NULL WHERE chain_id = ?1",
            params![chain_id],
        )?;
        Ok(n > 0)
    }

    /// Delete everything imported from one chain (an operator's reset after
    /// a fork, or `polis subscribe rm --purge`).
    pub fn forget_foreign_chain(&self, chain_id: &str) -> rusqlite::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM embeddings WHERE target_kind = 'foreign_prompt'
               AND target_id IN (SELECT id FROM foreign_prompts WHERE chain_id = ?1)",
            params![chain_id],
        )?;
        tx.execute("DELETE FROM embeddings WHERE target_kind = 'foreign_note' AND target_id IN (SELECT rowid FROM foreign_notes WHERE chain_id = ?1)", [chain_id])?;
        let mut n = 0;
        for table in ["foreign_prompts", "foreign_notes", "foreign_events", "foreign_principals", "foreign_redactions", "foreign_acks"] {
            n += tx.execute(&format!("DELETE FROM {table} WHERE chain_id = ?1"), params![chain_id])?;
        }
        n += tx.execute("DELETE FROM foreign_chains WHERE chain_id = ?1", params![chain_id])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn foreign_event_hash(&self, chain_id: &str, seq: i64) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row("SELECT entry_hash FROM foreign_events WHERE chain_id = ?1 AND seq = ?2", params![chain_id, seq], |r| r.get(0))
            .optional()
    }

    pub fn list_foreign_events(&self, chain_id: &str, from_seq: i64, limit: i64) -> rusqlite::Result<Vec<LedgerEventRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, author, prompt_id, session_id, version_number, ref_kind, ref_id, payload_hash, prev_hash, entry_hash
             FROM foreign_events WHERE chain_id = ?1 AND seq >= ?2 ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![chain_id, from_seq, limit], |r| {
            Ok(LedgerEventRow {
                seq: r.get(0)?,
                ts: r.get(1)?,
                kind: r.get(2)?,
                author: r.get(3)?,
                prompt_id: r.get(4)?,
                session_id: r.get(5)?,
                version_number: r.get(6)?,
                ref_kind: r.get(7)?,
                ref_id: r.get(8)?,
                payload_hash: r.get(9)?,
                prev_hash: r.get(10)?,
                entry_hash: r.get(11)?,
            })
        })?;
        rows.collect()
    }

    // ---- the import, atomically ---------------------------------------------

    /// Write one verified segment: events, prompts (at their redaction),
    /// notes, principal cards, the redactions it carries, then the chain's
    /// new head. All-or-nothing. A row that already exists (a re-import, an
    /// overlap the caller already hash-checked) is left alone; a prompt that
    /// was tombstoned here stays tombstoned even if the peer re-sends its
    /// body. Redactions targeting a chain we hold tombstone that body now.
    #[allow(clippy::too_many_arguments)]
    pub fn import_foreign_segment(
        &self,
        chain: &ForeignChain,
        events: &[LedgerEventRow],
        prompts: &[ForeignPromptInput<'_>],
        notes: &[ForeignNoteInput<'_>],
        principals: &[PrincipalCard],
        redactions: &[ForeignRedactionInput<'_>],
    ) -> rusqlite::Result<ImportedRows> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut out = ImportedRows::default();
        let now = polis_core::ledger::now_millis();
        for e in events {
            let n = tx.execute(
                "INSERT OR IGNORE INTO foreign_events
                    (chain_id, seq, ts, kind, author, prompt_id, session_id, version_number, ref_kind, ref_id, payload_hash, prev_hash, entry_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    chain.chain_id, e.seq, e.ts, e.kind, e.author, e.prompt_id, e.session_id, e.version_number,
                    e.ref_kind, e.ref_id, e.payload_hash, e.prev_hash, e.entry_hash
                ],
            )?;
            out.events += n;
        }
        for p in prompts {
            let n = tx.execute(
                "INSERT OR IGNORE INTO foreign_prompts
                    (chain_id, seq, prompt_id, role, body_hash, redaction, text, project, tombstoned, imported_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9)",
                params![chain.chain_id, p.seq, p.prompt_id, p.role, p.body_hash, p.redaction, p.text, p.project, now],
            )?;
            out.prompts += n;
            if Self::foreign_capture_tombstoned_locked(&tx, chain.chain_id.as_str(), p.seq)? {
                out.tombstoned += Self::tombstone_locked(&tx, &chain.chain_id, p.seq)?;
            }
        }
        for n in notes {
            let seq = n.seq.ok_or_else(|| rusqlite::Error::InvalidParameterName("foreign notes require a source ledger sequence".into()))?;
            let old_seq: Option<i64> = tx.query_row("SELECT seq FROM foreign_notes WHERE chain_id=?1 AND note_id=?2",params![chain.chain_id,n.note_id],|r|r.get(0)).optional()?.flatten();
            let blocked = Self::foreign_note_tombstoned_locked(&tx,&chain.chain_id,n.note_id,seq)?
                || old_seq.map(|old|Self::foreign_capture_tombstoned_locked(&tx,&chain.chain_id,old)).transpose()?.unwrap_or(false);
            let k = tx.execute(
                "INSERT INTO foreign_notes (chain_id, note_id, seq, target_kind, target_id, text, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(chain_id,note_id) DO UPDATE SET seq=excluded.seq,target_kind=excluded.target_kind,target_id=excluded.target_id,text=excluded.text,created_at=excluded.created_at
                 WHERE excluded.seq>COALESCE(foreign_notes.seq,0)",
                params![chain.chain_id, n.note_id, seq, n.target_kind, n.target_id, if blocked {""}else{n.text}, n.created_at],
            )?;
            out.notes += k;
            if blocked {out.tombstoned += Self::tombstone_locked(&tx,&chain.chain_id,seq)?;}
        }
        for p in principals {
            let k = tx.execute(
                "INSERT OR IGNORE INTO foreign_principals (principal_id, chain_id, kind, pubkey, parent_id, display_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![p.principal_id, chain.chain_id, p.kind.as_str(), p.pubkey, p.parent_id, p.display_name],
            )?;
            out.principals += k;
        }
        for r in redactions {
            if r.target_chain != chain.chain_id {
                return Err(rusqlite::Error::InvalidParameterName("a foreign chain may redact only its own captures".into()));
            }
            let k = tx.execute(
                "INSERT OR IGNORE INTO foreign_redactions (chain_id, event_seq, target_chain, target_seq, imported_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![chain.chain_id, r.event_seq, r.target_chain, r.target_seq, now],
            )?;
            out.redactions += k;
            out.tombstoned += Self::tombstone_locked(&tx, r.target_chain, r.target_seq)?;
        }
        tx.execute(
            &format!("INSERT INTO foreign_chains ({CHAIN_COLS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(chain_id) DO UPDATE SET
                human_id = excluded.human_id,
                device_name = COALESCE(excluded.device_name, foreign_chains.device_name),
                display_name = COALESCE(excluded.display_name, foreign_chains.display_name),
                head_seq = MAX(foreign_chains.head_seq, excluded.head_seq),
                head_hash = CASE WHEN excluded.head_seq >= foreign_chains.head_seq THEN excluded.head_hash ELSE foreign_chains.head_hash END,
                last_import_at = excluded.last_import_at"),
            params![
                chain.chain_id, chain.human_id, chain.device_name, chain.display_name, chain.head_seq, chain.head_hash,
                i64::from(chain.forked), chain.fork_detail, chain.first_import_at, chain.last_import_at
            ],
        )?;
        tx.commit()?;
        Ok(out)
    }

    fn tombstone_locked(conn: &Connection, chain_id: &str, seq: i64) -> rusqlite::Result<usize> {
        if chain_id.len()!=64 || !chain_id.bytes().all(|b|b.is_ascii_hexdigit()) || seq<=0 {
            return Err(rusqlite::Error::InvalidParameterName("invalid foreign capture identity".into()));
        }
        let marker_key=format!("polis.foreignForgotten.{chain_id}.{seq}");
        if crate::meta::get(conn,&marker_key)?.is_none() {
            Self::persist_forget_marker_locked(conn,&format!("foreign_capture:{chain_id}:{seq}"))?;
        }
        // This is local suppression state, never a fabricated signed event.
        crate::meta::set(conn,&marker_key,"1")?;
        let n = conn.execute(
            "UPDATE foreign_prompts SET text = ?3, redaction = 'stub', tombstoned = 1
             WHERE chain_id = ?1 AND seq = ?2 AND tombstoned = 0",
            params![chain_id, seq, TOMBSTONE],
        )?;
        conn.execute(
                "DELETE FROM embeddings WHERE target_kind = 'foreign_prompt'
                   AND target_id IN (SELECT id FROM foreign_prompts WHERE chain_id = ?1 AND seq = ?2)",
                params![chain_id, seq],
            )?;
        // A standalone note keeps its id through text edits. The immutable
        // source event can identify that row even if this peer holds a newer
        // note version than the redaction target.
        let selector="chain_id=?1 AND (seq=?2 OR note_id IN (SELECT CAST(ref_id AS INTEGER) FROM foreign_events WHERE chain_id=?1 AND seq=?2 AND kind='note' AND ref_kind='none'))";
        conn.execute(&format!("DELETE FROM embeddings WHERE target_kind='foreign_note' AND target_id IN (SELECT rowid FROM foreign_notes WHERE {selector})"),params![chain_id,seq])?;
        let notes=conn.execute(&format!("UPDATE foreign_notes SET text='' WHERE {selector} AND text<>''"),params![chain_id,seq])?;
        Ok(n+notes)
    }

    fn foreign_capture_tombstoned_locked(conn:&Connection,chain_id:&str,seq:i64)->rusqlite::Result<bool> {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM foreign_redactions WHERE chain_id=target_chain AND target_chain=?1 AND target_seq=?2) OR EXISTS(SELECT 1 FROM polis_meta WHERE key=?3)",params![chain_id,seq,format!("polis.foreignForgotten.{chain_id}.{seq}")],|r|r.get(0))
    }

    fn foreign_note_tombstoned_locked(conn:&Connection,chain_id:&str,note_id:i64,seq:i64)->rusqlite::Result<bool> {
        if Self::foreign_capture_tombstoned_locked(conn,chain_id,seq)? {return Ok(true);}
        conn.query_row("SELECT EXISTS(SELECT 1 FROM foreign_events e WHERE e.chain_id=?1 AND e.kind='note' AND e.ref_kind='none' AND e.ref_id=?2 AND (
            EXISTS(SELECT 1 FROM foreign_redactions r WHERE r.chain_id=r.target_chain AND r.target_chain=e.chain_id AND r.target_seq=e.seq)
            OR EXISTS(SELECT 1 FROM polis_meta m WHERE m.key='polis.foreignForgotten.' || e.chain_id || '.' || e.seq)))",params![chain_id,note_id.to_string()],|r|r.get(0))
    }

    /// Local suppression replayed during managed restore. This never invents
    /// a peer-signed redaction event or changes the foreign chain's head.
    pub fn tombstone_foreign_capture(&self, chain_id:&str, seq:i64)->rusqlite::Result<bool> {
        let mut conn=self.conn();let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let count=Self::tombstone_locked(&tx,chain_id,seq)?;tx.commit()?;Ok(count>0)
    }

    /// Tombstone one foreign body now (a redaction that arrived on its own).
    pub fn tombstone_foreign_prompt(&self, chain_id: &str, seq: i64) -> rusqlite::Result<bool> {
        self.tombstone_foreign_capture(chain_id,seq)
    }

    // ---- reads -------------------------------------------------------------

    pub fn foreign_prompt(&self, chain_id: &str, seq: i64) -> rusqlite::Result<Option<ForeignPromptRow>> {
        self.conn()
            .query_row(&format!("SELECT {PROMPT_COLS} FROM foreign_prompts WHERE chain_id = ?1 AND seq = ?2"), params![chain_id, seq], row_to_prompt)
            .optional()
    }

    pub fn foreign_prompt_by_id(&self, id: i64) -> rusqlite::Result<Option<ForeignPromptRow>> {
        self.conn()
            .query_row(&format!("SELECT {PROMPT_COLS} FROM foreign_prompts WHERE id = ?1"), params![id], row_to_prompt)
            .optional()
    }

    pub fn list_foreign_prompts(&self, chain_id: &str, limit: i64) -> rusqlite::Result<Vec<ForeignPromptRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {PROMPT_COLS} FROM foreign_prompts WHERE chain_id = ?1 ORDER BY seq ASC LIMIT ?2"))?;
        let rows = stmt.query_map(params![chain_id, limit], row_to_prompt)?;
        rows.collect()
    }

    /// BM25 over the foreign bodies we hold (tombstones excluded — a
    /// tombstone has no text to match). `fts_match` is an FTS5 MATCH
    /// expression the query planner built; the same tokenizer as the local
    /// lake, so the same plan works. Chains marked forked still answer — the
    /// rows verified when they arrived; the fork is a fact about what came
    /// AFTER them.
    pub fn foreign_scope_clause_locked(conn: &Connection, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<crate::principals::ScopeSql> {
        let mut sql = String::new();
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if !scope.include_shared || scope.org.is_some() { sql.push_str(" AND 0"); }
        if let Some(principal) = &scope.principal {
            let id = Self::resolve_author_locked(conn, principal)?.unwrap_or_else(|| principal.clone());
            sql.push_str(" AND EXISTS (SELECT 1 FROM foreign_chains fc WHERE fc.chain_id = p.chain_id AND (fc.human_id = ? OR fc.chain_id = ?))");
            binds.push(Box::new(id.clone())); binds.push(Box::new(id));
        }
        if let Some(project) = &scope.project { sql.push_str(" AND p.project = ?"); binds.push(Box::new(project.clone())); }
        if !scope.roles.is_empty() {
            sql.push_str(&format!(" AND p.role IN ({})", vec!["?"; scope.roles.len()].join(",")));
            binds.extend(scope.roles.iter().map(|r| Box::new(r.clone()) as Box<dyn rusqlite::ToSql>));
        }
        for (column, value) in [("author", &scope.agent), ("session_id", &scope.run)] {
            if let Some(value) = value { sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM foreign_events fe WHERE fe.chain_id = p.chain_id AND fe.seq = p.seq AND fe.{column} = ?)")); binds.push(Box::new(value.clone())); }
        }
        for (op, value) in [(">=", scope.after), ("<", scope.before)] {
            if let Some(value) = value { sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM foreign_events fe WHERE fe.chain_id = p.chain_id AND fe.seq = p.seq AND fe.ts {op} ?)")); binds.push(Box::new(value)); }
        }
        Ok(crate::principals::ScopeSql { sql, binds })
    }

    pub fn search_foreign_prompts(&self, fts_match: &str, limit: i64) -> rusqlite::Result<Vec<(ForeignPromptRow, f64)>> {
        self.search_foreign_prompts_scoped(fts_match, limit, &crate::principals::ScopeFilter { include_shared: true, ..Default::default() })
    }

    pub fn search_foreign_prompts_scoped(&self, fts_match: &str, limit: i64, scope: &crate::principals::ScopeFilter) -> rusqlite::Result<Vec<(ForeignPromptRow, f64)>> {
        let conn = self.conn();
        let scoped = Self::foreign_scope_clause_locked(&conn, scope)?.numbered(3);
        // Every column qualified: the FTS table exposes `text` too, and an
        // ambiguous name is an error the arm would otherwise swallow as
        // "no hits".
        let cols = PROMPT_COLS.split(", ").map(|c| format!("p.{c}")).collect::<Vec<_>>().join(", ");
        let mut stmt = conn.prepare(&format!(
            "SELECT {cols}, bm25(foreign_prompts_fts)
             FROM foreign_prompts_fts
             JOIN foreign_prompts p ON p.id = foreign_prompts_fts.rowid
             WHERE foreign_prompts_fts MATCH ?1 AND p.tombstoned = 0{}
             ORDER BY bm25(foreign_prompts_fts) LIMIT ?2", scoped.sql
        ))?;
        let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&fts_match, &limit];
        binds.extend(scoped.binds.iter().map(|v| v.as_ref()));
        let rows = stmt.query_map(binds.as_slice(), |r| Ok((row_to_prompt(r)?, r.get::<_, f64>(10)?)))?;
        rows.collect()
    }

    /// Foreign bodies the semantic index has not embedded under `model`
    /// (mirrors `embedding_backlog`, for the `foreign_prompt` target kind).
    pub fn foreign_embedding_backlog(&self, model: &str, limit: i64) -> rusqlite::Result<Vec<(String, i64, String, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT p.id, p.text, p.body_hash
             FROM foreign_prompts p
             WHERE p.tombstoned = 0 AND p.text IS NOT NULL AND LENGTH(p.text) > 0
               AND NOT EXISTS (
                   SELECT 1 FROM embeddings e
                   WHERE e.target_kind = 'foreign_prompt' AND e.target_id = p.id
                     AND e.model = ?1 AND e.source_hash = p.body_hash)
             ORDER BY p.id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![model, limit], |r| {
            Ok(("foreign_prompt".to_string(), r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        rows.collect()
    }

    /// The vectors we hold for one local prompt, for an export that ships
    /// them: `(model, dim, chunk_ix, char_start, char_len, scale, vec)`.
    pub fn embeddings_for_prompt(&self, prompt_id: i64) -> rusqlite::Result<Vec<(String, i64, i64, i64, i64, f64, Vec<u8>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT model, dim, chunk_ix, char_start, char_len, scale, vec FROM embeddings
             WHERE target_kind = 'prompt' AND target_id = ?1 ORDER BY model, chunk_ix",
        )?;
        let rows = stmt.query_map(params![prompt_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?;
        rows.collect()
    }

    /// Store a foreign vector that matched the local model (reuse; the plan's
    /// "reused only when its `model` id matches"). Raw int8 rows, no
    /// re-quantization.
    #[allow(clippy::too_many_arguments)]
    pub fn store_foreign_embedding_raw(
        &self,
        foreign_prompt_id: i64,
        model: &str,
        source_hash: &str,
        dim: i64,
        chunk_ix: i64,
        char_start: i64,
        char_len: i64,
        scale: f64,
        vec: &[u8],
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO embeddings
                (target_kind, target_id, chunk_ix, char_start, char_len, dim, scale, vec, model, source_hash, created_at)
             VALUES ('foreign_prompt', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![foreign_prompt_id, chunk_ix, char_start, char_len, dim, scale, vec, model, source_hash, polis_core::ledger::now_millis()],
        )?;
        Ok(())
    }

    pub fn foreign_counts(&self) -> rusqlite::Result<(i64, i64, i64, i64, i64)> {
        let conn = self.conn();
        let chains: i64 = conn.query_row("SELECT COUNT(*) FROM foreign_chains", [], |r| r.get(0))?;
        let forked: i64 = conn.query_row("SELECT COUNT(*) FROM foreign_chains WHERE forked = 1", [], |r| r.get(0))?;
        let events: i64 = conn.query_row("SELECT COUNT(*) FROM foreign_events", [], |r| r.get(0))?;
        let prompts: i64 = conn.query_row("SELECT COUNT(*) FROM foreign_prompts", [], |r| r.get(0))?;
        let tombstoned: i64 = conn.query_row("SELECT COUNT(*) FROM foreign_prompts WHERE tombstoned = 1", [], |r| r.get(0))?;
        Ok((chains, forked, events, prompts, tombstoned))
    }

    // ---- trust ---------------------------------------------------------------

    pub fn trust_get(&self, principal_id: &str) -> rusqlite::Result<Option<TrustEntry>> {
        self.conn()
            .query_row(
                "SELECT principal_id, pubkey, fingerprint, source, display_name, added_at FROM foreign_trust WHERE principal_id = ?1",
                params![principal_id],
                |r| Ok(TrustEntry { principal_id: r.get(0)?, pubkey: r.get(1)?, fingerprint: r.get(2)?, source: r.get(3)?, display_name: r.get(4)?, added_at: r.get(5)? }),
            )
            .optional()
    }

    pub fn trust_list(&self) -> rusqlite::Result<Vec<TrustEntry>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT principal_id, pubkey, fingerprint, source, display_name, added_at FROM foreign_trust ORDER BY added_at, principal_id")?;
        let rows = stmt.query_map([], |r| Ok(TrustEntry { principal_id: r.get(0)?, pubkey: r.get(1)?, fingerprint: r.get(2)?, source: r.get(3)?, display_name: r.get(4)?, added_at: r.get(5)? }))?;
        rows.collect()
    }

    /// Insert only: a trusted key is never overwritten by an import (a key
    /// change is a rejection, not an update). `polis trust rm` first.
    pub fn trust_set(&self, e: &TrustEntry) -> rusqlite::Result<bool> {
        let n = self.conn().execute(
            "INSERT OR IGNORE INTO foreign_trust (principal_id, pubkey, fingerprint, source, display_name, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![e.principal_id, e.pubkey, e.fingerprint, e.source, e.display_name, e.added_at],
        )?;
        Ok(n > 0)
    }

    pub fn trust_remove(&self, principal_id: &str) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("DELETE FROM foreign_trust WHERE principal_id = ?1", params![principal_id])? > 0)
    }

    // ---- subscriptions --------------------------------------------------------

    pub fn subscriptions(&self) -> rusqlite::Result<Vec<SubscriptionRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, principal, project, class, created_at FROM foreign_subscriptions ORDER BY id")?;
        let rows = stmt.query_map([], |r| Ok(SubscriptionRow { id: r.get(0)?, principal: r.get(1)?, project: r.get(2)?, class: r.get(3)?, created_at: r.get(4)? }))?;
        rows.collect()
    }

    pub fn subscribe(&self, principal: Option<&str>, project: Option<&str>, class: Option<&str>) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO foreign_subscriptions (principal, project, class, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![principal, project, class, polis_core::ledger::now_millis()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn unsubscribe(&self, id: i64) -> rusqlite::Result<bool> {
        Ok(self.conn().execute("DELETE FROM foreign_subscriptions WHERE id = ?1", params![id])? > 0)
    }

    // ---- acks ------------------------------------------------------------------

    /// A peer told us (in its segment) the newest seq of OUR chain it has
    /// imported — that is how a redaction we emitted counts as acknowledged.
    pub fn set_foreign_ack(&self, chain_id: &str, acked_seq: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO foreign_acks (chain_id, acked_seq, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(chain_id) DO UPDATE SET acked_seq = MAX(foreign_acks.acked_seq, excluded.acked_seq), updated_at = excluded.updated_at",
            params![chain_id, acked_seq, polis_core::ledger::now_millis()],
        )?;
        Ok(())
    }

    pub fn foreign_acks(&self) -> rusqlite::Result<Vec<(String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT chain_id, acked_seq FROM foreign_acks ORDER BY chain_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Our own redaction events: `(seq, target_chain, target_seq)` — read
    /// from the payloads `forget` keeps beside the chain (`polis_meta`
    /// `polis.redaction.<seq>`), because only a hash of them is on the chain.
    pub fn own_redactions(&self) -> rusqlite::Result<Vec<(i64, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT key, value FROM polis_meta WHERE key LIKE 'polis.redaction.%' ORDER BY key")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value) = row?;
            let Some(seq) = key.strip_prefix("polis.redaction.").and_then(|s| s.parse::<i64>().ok()) else { continue };
            let v: serde_json::Value = serde_json::from_str(&value).unwrap_or_default();
            let chain = v.get("chainId").and_then(|c| c.as_str()).unwrap_or_default().to_string();
            let tseq = v.get("seq").and_then(|s| s.as_i64()).unwrap_or(0);
            out.push((seq, chain, tseq));
        }
        Ok(out)
    }

    pub fn known_principal_kind(&self, id: &str) -> rusqlite::Result<Option<PrincipalKind>> {
        Ok(self.get_principal(id)?.map(|p| p.kind))
    }
}

impl PolisStore {
    /// The seq of a prompt's own `prompt` event — what a redaction names.
    pub fn prompt_event_seq(&self, prompt_id: i64) -> rusqlite::Result<Option<i64>> {
        self.conn()
            .query_row("SELECT seq FROM ledger_events WHERE kind = 'prompt' AND prompt_id = ?1 ORDER BY seq LIMIT 1", params![prompt_id], |r| r.get(0))
            .optional()
    }
}

// ---------------------------------------------------------------------------
// E4: a peer's published catalog and the relayed acks
// ---------------------------------------------------------------------------
impl PolisStore {
    /// Replace a peer's published catalog with the one its newest segment
    /// carries (a catalog is a snapshot, not a log: the latest wins).
    pub fn import_foreign_tree(&self, chain_id: &str, nodes: &[ForeignClassNode], links: &[ForeignClassLink]) -> rusqlite::Result<(usize, usize)> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM foreign_class_nodes WHERE chain_id = ?1", params![chain_id])?;
        tx.execute("DELETE FROM foreign_class_links WHERE chain_id = ?1", params![chain_id])?;
        let now = polis_core::ledger::now_millis();
        for n in nodes {
            tx.execute(
                "INSERT OR REPLACE INTO foreign_class_nodes (chain_id, node_id, parent_id, kind, title, summary, imported_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![chain_id, n.node_id, n.parent_id, n.kind, n.title, n.summary, now],
            )?;
        }
        for l in links {
            tx.execute(
                "INSERT OR IGNORE INTO foreign_class_links (chain_id, node_id, target_kind, target_id) VALUES (?1, ?2, ?3, ?4)",
                params![chain_id, l.node_id, l.target_kind, l.target_id],
            )?;
        }
        tx.commit()?;
        Ok((nodes.len(), links.len()))
    }

    pub fn list_foreign_class_nodes(&self, chain_id: &str) -> rusqlite::Result<Vec<ForeignClassNode>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT chain_id, node_id, parent_id, kind, title, summary FROM foreign_class_nodes WHERE chain_id = ?1 ORDER BY title")?;
        let rows = stmt.query_map(params![chain_id], |r| {
            Ok(ForeignClassNode { chain_id: r.get(0)?, node_id: r.get(1)?, parent_id: r.get(2)?, kind: r.get(3)?, title: r.get(4)?, summary: r.get(5)? })
        })?;
        rows.collect()
    }

    pub fn list_foreign_class_links(&self, chain_id: &str, node_id: &str) -> rusqlite::Result<Vec<ForeignClassLink>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT chain_id, node_id, target_kind, target_id FROM foreign_class_links WHERE chain_id = ?1 AND node_id = ?2 ORDER BY target_id")?;
        let rows = stmt.query_map(params![chain_id, node_id], |r| Ok(ForeignClassLink { chain_id: r.get(0)?, node_id: r.get(1)?, target_kind: r.get(2)?, target_id: r.get(3)? }))?;
        rows.collect()
    }

    /// The titles of a peer's classes that point at `(chain, seq)` — the
    /// label a shared hit carries ("the firm files this under …").
    pub fn foreign_class_titles_for(&self, chain_id: &str, seq: i64) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let target = format!("{chain_id}:{seq}");
        let mut stmt = conn.prepare(
            "SELECT l.chain_id, n.title FROM foreign_class_links l
             JOIN foreign_class_nodes n ON n.chain_id = l.chain_id AND n.node_id = l.node_id
             WHERE (l.chain_id = ?1 AND l.target_id = ?2) OR l.target_id = ?3
             ORDER BY n.title",
        )?;
        let rows = stmt.query_map(params![chain_id, seq.to_string(), target], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// "`acker` has imported `chain` up to `seq`" — monotone.
    pub fn record_org_ack(&self, acker_chain: &str, chain_id: &str, acked_seq: i64) -> rusqlite::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO org_acks (acker_chain, chain_id, acked_seq, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(acker_chain, chain_id) DO UPDATE SET acked_seq = MAX(acked_seq, excluded.acked_seq), updated_at = excluded.updated_at",
            params![acker_chain, chain_id, acked_seq, polis_core::ledger::now_millis()],
        )?;
        Ok(())
    }

    /// `(acker, chain, acked_seq)`, optionally narrowed to acks OF one chain.
    pub fn org_acks(&self, chain_id: Option<&str>) -> rusqlite::Result<Vec<(String, String, i64)>> {
        let conn = self.conn();
        let mut out = Vec::new();
        match chain_id {
            Some(c) => {
                let mut stmt = conn.prepare("SELECT acker_chain, chain_id, acked_seq FROM org_acks WHERE chain_id = ?1 ORDER BY acker_chain")?;
                for r in stmt.query_map(params![c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = conn.prepare("SELECT acker_chain, chain_id, acked_seq FROM org_acks ORDER BY chain_id, acker_chain")?;
                for r in stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    /// Every redaction held from any peer chain, newest first.
    pub fn list_foreign_redactions(&self) -> rusqlite::Result<Vec<ForeignRedactionRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT chain_id, event_seq, target_chain, target_seq FROM foreign_redactions ORDER BY event_seq DESC")?;
        let rows = stmt.query_map([], |r| Ok(ForeignRedactionRow { chain_id: r.get(0)?, event_seq: r.get(1)?, target_chain: r.get(2)?, target_seq: r.get(3)? }))?;
        rows.collect()
    }
}
