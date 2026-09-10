// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Cold-body compaction and its reversible archive: gist in, deflated original beside it, hash-verified on restore.
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

/// The codec `prompt_archive.blob` is written with. Stored per row rather than
/// assumed, so a future codec is a new value here and not a migration.
pub const ARCHIVE_ALGO: &str = "deflate";

/// Deflate a released prompt body for the compaction archive.
pub fn deflate_body(body: &str) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(body.as_bytes())?;
    enc.finish()
}

/// Inflate an archived body. Callers must re-verify the result against the
/// row's `body_hash` before trusting it — see `restore_prompt_body`.
pub fn inflate_body(blob: &[u8]) -> std::io::Result<String> {
    use std::io::Read;
    let mut out = String::new();
    flate2::read::DeflateDecoder::new(blob).read_to_string(&mut out)?;
    Ok(out)
}

impl PolisStore {
    /// Compact a cold prompt: swap its stored body for `gist` and emit a
    /// `compaction` ledger event proving the swap. The original `body_hash`
    /// stays untouched (the tamper-evident fact + dedup key), so
    /// `verify_ledger_chain` and `verify_bundle` remain green — they are
    /// body-blind. Idempotent via the `gist IS NULL` guard: a re-compaction of
    /// an already-compacted row is a no-op returning `Ok(None)`. On success
    /// returns the new ledger seq. `reason` distinguishes an automatic gist
    /// (`"cold"`) from an explicit forget (`"forget"`). `actor` is who released
    /// the words — the keeper's seat name for an automatic gist, the local
    /// human for an explicit forget. `gist_source` records which tier wrote the
    /// gist — `"agent"` (the keeper's summarizer) or `"deterministic"` (the
    /// fallback window) — so a summarizer that silently stopped running is
    /// visible on Health instead of hiding behind the reclaim number.
    pub fn compact_prompt_body(
        &self,
        prompt_id: i64,
        gist: &str,
        reason: &str,
        gist_source: &str,
        actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        let conn = self.conn();
        Self::compact_prompt_body_locked(&conn, prompt_id, gist, reason, gist_source, actor)
    }

    /// `compact_prompt_body` journaled under a gardener run (B2): the
    /// `compact` op's inverse is `restore_prompt_body`, hash-verified against
    /// the archive, so a `cold` compaction is revertible for as long as its
    /// archive row lives. Journal and swap happen under one lock.
    pub fn compact_prompt_body_in_run(
        &self,
        run_id: i64,
        prompt_id: i64,
        gist: &str,
        reason: &str,
        gist_source: &str,
        actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        let conn = self.conn();
        let seq = Self::compact_prompt_body_locked(&conn, prompt_id, gist, reason, gist_source, actor)?;
        if let Some(seq) = seq {
            Self::journal_op_locked(
                &conn,
                run_id,
                &crate::runs::OpRecord::applied(
                    "compact",
                    vec![crate::runs::subject::prompt(prompt_id)],
                    crate::runs::image::compact(prompt_id, seq),
                )
                .with_ledger_seq(Some(seq)),
            )?;
        }
        Ok(seq)
    }

    /// The core, under an already-held lock.
    pub fn compact_prompt_body_locked(
        conn: &rusqlite::Connection,
        prompt_id: i64,
        gist: &str,
        reason: &str,
        gist_source: &str,
        actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        if !conn.is_autocommit() {
            return Self::compact_prompt_body_in_transaction(conn, prompt_id, gist, reason, gist_source, actor);
        }
        conn.execute_batch("BEGIN IMMEDIATE")?;
        match Self::compact_prompt_body_in_transaction(conn, prompt_id, gist, reason, gist_source, actor) {
            Ok(result) => { conn.execute_batch("COMMIT")?; Ok(result) }
            Err(error) => { let _ = conn.execute_batch("ROLLBACK"); Err(error) }
        }
    }

    fn compact_prompt_body_in_transaction(
        conn: &rusqlite::Connection, prompt_id: i64, gist: &str, reason: &str,
        gist_source: &str, actor: &str,
    ) -> rusqlite::Result<Option<i64>> {
        // Read the original body + hash under the same lock, then swap — all
        // atomic with the ledger append below so the chain can't race.
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT body, body_hash FROM prompts WHERE id = ?1 AND (gist IS NULL OR ?2 = 'forget')
                 AND id NOT IN (SELECT prompt_id FROM forgotten_sources)",
                params![prompt_id, reason],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((body, body_hash)) = row else {
            return Ok(None); // no such warm prompt (already compacted, or gone)
        };
        let original_bytes = body.len() as i64;
        let ts = polis_core::ledger::now_millis();
        // Archive BEFORE the words are released, and only for a `cold`
        // compaction: cold means "we think you're done with this", which is a
        // guess and must therefore be reversible. `forget` means the user said
        // so — it archives nothing and *deletes* any archive an earlier cold
        // pass left behind, because a forget that leaves a recoverable copy on
        // disk is not a forget. The two branches are the whole contract.
        if reason == "cold" {
            match deflate_body(&body) {
                Ok(blob) => {
                    if let Err(e) = conn.execute(
                        "INSERT INTO prompt_archive
                            (prompt_id, body_hash, algo, original_bytes, blob, archived_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT(prompt_id) DO UPDATE SET
                            body_hash = excluded.body_hash, algo = excluded.algo,
                            original_bytes = excluded.original_bytes,
                            blob = excluded.blob, archived_at = excluded.archived_at",
                        params![prompt_id, body_hash, ARCHIVE_ALGO, original_bytes, blob, ts],
                    ) {
                        // Best-effort: an archive failure must not block the
                        // reclaim, but it must not be silent either — an
                        // un-archived compaction is exactly today's behaviour.
                        tracing::warn!(error = %e, prompt = prompt_id, "prompt archive write failed");
                    }
                }
                Err(e) => tracing::warn!(error = %e, prompt = prompt_id, "prompt archive deflate failed"),
            }
        } else {
            conn.execute("DELETE FROM prompt_archive WHERE prompt_id = ?1", params![prompt_id])?;
        }
        if reason == "forget" {
            // A managed restore can replace this SQLite file. Keep the hash
            // tombstone beside it as well, fsync'ed BEFORE releasing words.
            Self::persist_forget_marker_locked(conn, &body_hash)?;
            conn.execute("INSERT INTO forgotten_sources(prompt_id, body_hash, forgotten_at) VALUES (?1, ?2, ?3)", params![prompt_id, body_hash, ts])?;
            conn.execute("INSERT OR IGNORE INTO redaction_outbox(prompt_id, created_at) VALUES (?1, ?2)", params![prompt_id, ts])?;
            conn.execute("DELETE FROM embeddings WHERE target_kind = 'prompt' AND target_id = ?1", [prompt_id])?;
            let seqs = conn.prepare("SELECT e.seq FROM ledger_events e LEFT JOIN decision_evidence de ON de.seq=e.seq
                LEFT JOIN ledger_events src ON src.seq=de.source_seq WHERE COALESCE(e.prompt_id,src.prompt_id)=?1")?
                .query_map([prompt_id],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Self::forget_derived_copies_locked(conn, "prompt", prompt_id, &seqs, ts)?;
            // A copied observation/gist must not keep a forgotten quotation.
            conn.execute("UPDATE class_observations SET summary = '[forgotten source]', dismissed = 1,
                retired_at = ?2, retired_reason = 'forgotten source'
                WHERE EXISTS (SELECT 1 FROM json_each(cite_seqs) cite JOIN ledger_events e ON e.seq = cite.value
                    LEFT JOIN decision_evidence de ON de.seq = e.seq LEFT JOIN ledger_events src ON src.seq = de.source_seq
                    WHERE COALESCE(e.prompt_id, src.prompt_id) = ?1)", params![prompt_id, ts])?;
            conn.execute("UPDATE class_links SET note = NULL WHERE target_kind IN ('prompt','decision','ledger','resolution','approval','review_verdict')
                AND target_id IN (SELECT CAST(e.seq AS TEXT) FROM ledger_events e
                    LEFT JOIN decision_evidence de ON de.seq = e.seq LEFT JOIN ledger_events src ON src.seq = de.source_seq
                    WHERE COALESCE(e.prompt_id, src.prompt_id) = ?1)", [prompt_id])?;
            Self::invalidate_prompt_claims_locked(conn, prompt_id)?;
        }
        conn.execute(
            "UPDATE prompts
                SET gist = ?2, body = '', compacted_at = ?3, original_bytes = COALESCE(original_bytes, ?4),
                    gist_source = ?5, user_text = CASE WHEN ?6 = 'forget' THEN NULL ELSE user_text END
             WHERE id = ?1 AND (gist IS NULL OR ?6 = 'forget')",
            params![prompt_id, gist, ts, original_bytes, gist_source, reason],
        )?;
        // The compaction event references the prompt and pins the ORIGINAL body
        // hash + the gist hash + the reason, so "what was forgotten" is provable
        // even if the prompt row is later purged.
        let gist_hash = polis_core::ledger::body_hash(gist);
        let ph = polis_core::ledger::decision_payload_hash(&[
            ("original_body_hash", &body_hash),
            ("gist_hash", &gist_hash),
            ("reason", reason),
        ]);
        let author = actor.to_string();
        let pid_str = prompt_id.to_string();
        let ev = Self::append_ledger_event_locked(
            conn,
            &polis_core::ledger::LedgerAppend {
                kind: polis_core::ledger::EventKind::Compaction.as_str(),
                author: &author,
                ts,
                prompt_id: Some(prompt_id),
                session_id: None,
                version_number: None,
                ref_kind: Some("prompt"),
                ref_id: Some(pid_str.as_str()),
                payload_hash: &ph,
            },
        )?;
        Ok(Some(ev.seq))
    }

    /// Restore a cold-compacted prompt's original body from the archive.
    ///
    /// Returns `Ok(false)` when there is nothing archived (never compacted, or
    /// compacted before archiving existed, or forgotten on purpose). The
    /// inflated bytes are re-hashed and checked against the archive row's
    /// `body_hash` before they go anywhere near the prompt row: the archive is
    /// derived data that lives OUTSIDE the hash chain, so it gets no trust it
    /// hasn't just earned. A mismatch is an error, never a silent overwrite —
    /// the whole point of the chain is that corrupt bytes can't quietly become
    /// the record.
    pub fn restore_prompt_body(&self, prompt_id: i64) -> rusqlite::Result<bool> {
        let conn = self.conn();
        Self::restore_prompt_body_locked(&conn, prompt_id)
    }

    /// The core, under an already-held lock (a revert runs it inside its
    /// one transaction).
    pub fn restore_prompt_body_locked(conn: &rusqlite::Connection, prompt_id: i64) -> rusqlite::Result<bool> {
        let forgotten: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM forgotten_sources WHERE prompt_id = ?1)", [prompt_id], |r| r.get(0))?;
        if forgotten { return Ok(false); }
        let row: Option<(String, String, Vec<u8>)> = conn
            .query_row(
                "SELECT body_hash, algo, blob FROM prompt_archive WHERE prompt_id = ?1",
                params![prompt_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((archived_hash, algo, blob)) = row else {
            return Ok(false);
        };
        if algo != ARCHIVE_ALGO {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "prompt {prompt_id}: unknown archive codec `{algo}`"
            )));
        }
        let body = inflate_body(&blob).map_err(|e| {
            rusqlite::Error::InvalidParameterName(format!("prompt {prompt_id}: inflate failed: {e}"))
        })?;
        if polis_core::ledger::body_hash(&body) != archived_hash {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "prompt {prompt_id}: archived body does not match its recorded hash"
            )));
        }
        // The prompt's own `body_hash` is the chain's commitment and is never
        // rewritten by compaction, so restoring is a pure re-inflation: put the
        // words back, clear the compaction marks (all four — a restored row is
        // warm again, and `gist_source` is a compaction fact), drop the
        // archive row.
        let changed = conn.execute(
            "UPDATE prompts
                SET body = ?2, gist = NULL, compacted_at = NULL, original_bytes = NULL,
                    gist_source = NULL
             WHERE id = ?1 AND body_hash = ?3",
            params![prompt_id, body, archived_hash],
        )?;
        if changed == 0 {
            return Ok(false);
        }
        conn.execute("DELETE FROM prompt_archive WHERE prompt_id = ?1", params![prompt_id])?;
        Ok(true)
    }

    /// How many compacted bodies are still recoverable, and the deflated bytes
    /// they occupy — the honest counterpart to `compaction_stats`' reclaim
    /// number, which reads as pure profit until you can see the cost.
    pub fn archive_stats(&self) -> rusqlite::Result<(i64, i64)> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(blob)), 0) FROM prompt_archive",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    /// Release copied organizer text and disable deferred work that could
    /// reintroduce it. Run windows and explicit subject/citation references
    /// provide the dependency closure; we drop that run's reversible images
    /// rather than allowing an inverse to restore a forgotten quotation.
    fn forget_derived_copies_locked(conn: &Connection, kind: &str, id: i64, seqs: &[i64], ts: i64) -> rusqlite::Result<()> {
        let seq_json = serde_json::to_string(seqs).unwrap();
        let links = conn.prepare("SELECT id,node_id FROM class_links WHERE
            (target_kind IN ('prompt','decision','ledger','resolution','approval','review_verdict','note','user_note') AND target_id IN (SELECT CAST(value AS TEXT) FROM json_each(?1)))
            OR (?2='browse_event' AND target_kind=?2 AND target_id=?3)")?.query_map(params![seq_json,kind,id.to_string()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let obs = conn.prepare("SELECT id,node_id FROM class_observations WHERE EXISTS(SELECT 1 FROM json_each(cite_seqs) WHERE value IN (SELECT value FROM json_each(?1)))")?
            .query_map([&seq_json],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let nodes: HashSet<String> = links.iter().chain(obs.iter()).map(|(_,node)|node.clone()).collect();
        let node_json = serde_json::to_string(&nodes).unwrap();
        let mut subjects: HashSet<String> = seqs.iter().map(|seq|format!("seq:{seq}")).collect();
        subjects.insert(format!("{kind}:{id}"));
        if kind == "user_note" { subjects.insert(format!("note:{id}")); }
        subjects.extend(nodes.iter().map(|node|format!("node:{node}")));
        subjects.extend(links.iter().map(|(link,_)|format!("link:{link}")));
        subjects.extend(obs.iter().map(|(observation,_)|format!("obs:{observation}")));
        let subject_json = serde_json::to_string(&subjects).unwrap();
        let mut runs: HashSet<i64> = conn.prepare("SELECT id FROM class_runs WHERE EXISTS(SELECT 1 FROM json_each(?1) WHERE value >= seq_from AND value <= seq_to)
            UNION SELECT run_id FROM class_run_ops WHERE EXISTS(SELECT 1 FROM json_each(subject_ids) WHERE value IN (SELECT value FROM json_each(?2)))")?
            .query_map(params![seq_json,subject_json],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let run_json = serde_json::to_string(&runs).unwrap();
        let proposals = conn.prepare("SELECT id,run_id FROM class_proposals WHERE run_id IN (SELECT value FROM json_each(?1))
            OR node_id IN (SELECT value FROM json_each(?2)) OR parent_id IN (SELECT value FROM json_each(?2))
            OR EXISTS(SELECT 1 FROM json_tree(CASE WHEN json_valid(extra_json) THEN extra_json ELSE 'null' END)
                WHERE (type='integer' AND value IN (SELECT value FROM json_each(?3))) OR (type='text' AND value IN (SELECT value FROM json_each(?2))))")?
            .query_map(params![run_json,node_json,seq_json],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Option<i64>>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (proposal,run) in proposals {
            if let Some(run)=run {runs.insert(run);}
            conn.execute("UPDATE class_proposals SET title=NULL, summary=NULL, extra_json=NULL, rationale=NULL,
                last_reason='forgotten source', status='expired', next_after_run=NULL WHERE id=?1",[proposal])?;
        }
        let run_json=serde_json::to_string(&runs).unwrap();
        conn.execute("UPDATE class_run_ops SET reason=NULL, pre_image=NULL, post_image=NULL WHERE run_id IN (SELECT value FROM json_each(?1))",[&run_json])?;
        conn.execute("UPDATE class_runs SET summary='[forgotten source]', error=NULL,source_forgotten_at=?2 WHERE id IN (SELECT value FROM json_each(?1))",params![run_json,ts])?;
        conn.execute("UPDATE class_links SET note=NULL WHERE id IN (SELECT value FROM json_each(?1))",[serde_json::to_string(&links.iter().map(|(id,_)|id).collect::<Vec<_>>()).unwrap()])?;
        conn.execute("UPDATE class_observations SET summary='[forgotten source]',dismissed=1,retired_at=?2,retired_reason='forgotten source' WHERE id IN (SELECT value FROM json_each(?1))",
            params![serde_json::to_string(&obs.iter().map(|(id,_)|id).collect::<Vec<_>>()).unwrap(),ts])?;
        conn.execute("UPDATE class_nodes SET summary=NULL WHERE kind='digest' AND id IN (SELECT value FROM json_each(?1))",[&node_json])?;
        conn.execute("DELETE FROM class_centroids WHERE node_id IN (SELECT value FROM json_each(?1))",[node_json])?;
        Ok(())
    }

    /// Admission guard for delayed organizer output. The check runs under the
    /// same writer transaction as the derived write, so a completed forget
    /// cannot race with a stale model response that copied the source text.
    pub(crate) fn source_forgotten_locked(conn: &Connection, seq: i64) -> rusqlite::Result<bool> {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM ledger_events e
            LEFT JOIN decision_evidence de ON de.seq=e.seq LEFT JOIN ledger_events src ON src.seq=de.source_seq
            WHERE e.seq=?1 AND (
                EXISTS(SELECT 1 FROM forgotten_sources f WHERE f.prompt_id=COALESCE(e.prompt_id,src.prompt_id)) OR
                EXISTS(SELECT 1 FROM forgotten_captures f WHERE f.target_kind=e.ref_kind AND CAST(f.target_id AS TEXT)=e.ref_id) OR
                EXISTS(SELECT 1 FROM note_events ne JOIN forgotten_captures f ON f.target_kind='user_note' AND f.target_id=ne.note_id WHERE ne.seq=e.seq)))",
            [seq], |r|r.get(0))
    }

    pub(crate) fn run_source_forgotten_locked(conn: &Connection, run_id: i64) -> rusqlite::Result<bool> {
        conn.query_row("SELECT EXISTS(SELECT 1 FROM class_runs WHERE id=?1 AND source_forgotten_at IS NOT NULL)", [run_id], |r|r.get(0))
    }

    /// Forget the locally captured page text. Current sharing formats carry
    /// browse ledger commitments, never browse bodies; notes use a different
    /// export lifecycle and are deliberately not handled here.
    pub fn forget_browse_event(&self, id: i64, actor: &str) -> rusqlite::Result<Option<i64>> {
        let mut conn=self.conn();
        let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let hash:Option<String>=tx.query_row("SELECT context_hash FROM browse_events WHERE id=?1 AND NOT EXISTS(SELECT 1 FROM forgotten_captures f WHERE f.target_kind='browse_event' AND f.target_id=browse_events.id)",[id],|r|r.get(0)).optional()?;
        let Some(hash)=hash else {return Ok(None);};
        let ts=polis_core::ledger::now_millis();
        Self::persist_forget_marker_locked(&tx,&format!("browse_event:{hash}"))?;
        tx.execute("INSERT INTO forgotten_captures(target_kind,target_id,source_hash,forgotten_at) VALUES('browse_event',?1,?2,?3)",params![id,hash,ts])?;
        let seqs=tx.prepare("SELECT seq FROM ledger_events WHERE ref_kind='browse_event' AND ref_id=?1")?.query_map([id.to_string()],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Self::forget_derived_copies_locked(&tx,"browse_event",id,&seqs,ts)?;
        tx.execute("DELETE FROM embeddings WHERE target_kind='browse_event' AND target_id=?1",[id])?;
        tx.execute("UPDATE browse_events SET url='',title=NULL,text='',caption=NULL,shot_key=NULL WHERE id=?1",[id])?;
        let ref_id=id.to_string();
        let payload=polis_core::ledger::decision_payload_hash(&[("target_kind","browse_event"),("source_hash",&hash)]);
        let event=Self::append_ledger_event_locked(&tx,&polis_core::ledger::LedgerAppend {kind:polis_core::ledger::EventKind::CaptureForgotten.as_str(),author:actor,ts,prompt_id:None,session_id:None,
            version_number:None,ref_kind:Some("browse_event"),ref_id:Some(&ref_id),payload_hash:&payload})?;
        tx.commit()?;
        Ok(Some(event.seq))
    }

    /// Forget a note's complete history. Keep its identity and commitments so
    /// every historical citation can report redaction and peers can retire the
    /// version they received, even when it predates the current text.
    pub fn forget_user_note(&self, id: i64, actor: &str) -> rusqlite::Result<Option<i64>> {
        let mut connection = self.conn();
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let corrupt: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM note_events ne JOIN user_notes n ON n.id=ne.note_id
            LEFT JOIN ledger_events e ON e.seq=ne.seq WHERE n.id=?1 AND (e.seq IS NULL OR e.payload_hash<>ne.payload_hash OR NOT (
                (n.target_kind='none' AND e.ref_kind='none' AND e.ref_id=CAST(n.id AS TEXT)) OR
                (n.target_kind<>'none' AND e.ref_kind=n.target_kind AND e.ref_id=n.target_id) OR
                (e.ref_kind='user_note' AND e.ref_id=CAST(n.id AS TEXT)))))", [id], |r|r.get(0))?;
        if corrupt { return Err(rusqlite::Error::InvalidParameterName("note history does not match its ledger references".into())); }
        let hash: Option<String> = tx.query_row(
            "SELECT e.entry_hash FROM note_events ne JOIN ledger_events e ON e.seq=ne.seq AND e.payload_hash=ne.payload_hash
             JOIN user_notes n ON n.id=ne.note_id WHERE ne.note_id=?1
             AND NOT EXISTS(SELECT 1 FROM forgotten_captures f WHERE f.target_kind='user_note' AND f.target_id=?1)
             ORDER BY ne.seq LIMIT 1", [id], |r| r.get(0),
        ).optional()?;
        let Some(hash) = hash else { return Ok(None); };
        let ts = polis_core::ledger::now_millis();
        Self::persist_forget_marker_locked(&tx, &format!("user_note:{hash}"))?;
        tx.execute("INSERT INTO forgotten_captures(target_kind,target_id,source_hash,forgotten_at) VALUES('user_note',?1,?2,?3)", params![id,hash,ts])?;
        let seqs = tx.prepare("SELECT seq FROM note_events WHERE note_id=?1 ORDER BY seq")?
            .query_map([id], |r| r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for seq in &seqs {
            tx.execute("INSERT OR IGNORE INTO capture_redaction_outbox(target_seq,created_at) VALUES(?1,?2)", params![seq,ts])?;
        }
        Self::forget_derived_copies_locked(&tx, "user_note", id, &seqs, ts)?;
        tx.execute("DELETE FROM embeddings WHERE target_kind IN ('note','user_note') AND target_id=?1", [id])?;
        tx.execute("UPDATE user_notes SET text='',starred=0,updated_at=?2 WHERE id=?1", params![id,ts])?;
        let ref_id = id.to_string();
        let payload = polis_core::ledger::decision_payload_hash(&[("target_kind","user_note"),("source_hash",&hash)]);
        let event = Self::append_ledger_event_locked(&tx, &polis_core::ledger::LedgerAppend {
            kind: polis_core::ledger::EventKind::CaptureForgotten.as_str(), author: actor, ts,
            prompt_id: None, session_id: None, version_number: None,
            ref_kind: Some("user_note"), ref_id: Some(&ref_id), payload_hash: &payload,
        })?;
        tx.commit()?;
        Ok(Some(event.seq))
    }

    pub(crate) fn persist_forget_marker_locked(conn: &Connection, body_hash: &str) -> rusqlite::Result<()> {
        use std::io::Write;
        let db: String = conn.query_row("SELECT file FROM pragma_database_list WHERE name = 'main'", [], |r| r.get(0))?;
        if db.is_empty() { return Ok(()); }
        let path = format!("{db}.forgotten");
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
        let mut file = options.open(&path).map_err(|e| rusqlite::Error::InvalidParameterName(format!("durable forget marker {path}: {e}")))?;
        file.write_all(format!("{body_hash}\n").as_bytes()).and_then(|()| file.sync_all()).map_err(|e| rusqlite::Error::InvalidParameterName(format!("sync forget marker: {e}")))?;
        #[cfg(unix)]
        if let Some(parent) = std::path::Path::new(&path).parent() {
            std::fs::File::open(parent).and_then(|directory| directory.sync_all())
                .map_err(|e| rusqlite::Error::InvalidParameterName(format!("sync forget registry directory: {e}")))?;
        }
        Ok(())
    }

    /// Export migrated legacy tombstones to the independent restore registry.
    pub fn sync_forget_registry(&self) -> rusqlite::Result<()> {
        let conn = self.conn();
        let db: String = conn.query_row("SELECT file FROM pragma_database_list WHERE name = 'main'", [], |r| r.get(0))?;
        if db.is_empty() { return Ok(()); }
        let path = format!("{db}.forgotten");
        let previous = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(rusqlite::Error::InvalidParameterName(format!("read forget registry: {error}"))),
        };
        let existing: HashSet<&str> = previous.lines().collect();
        let mut stmt = conn.prepare("SELECT DISTINCT body_hash FROM forgotten_sources")?;
        for hash in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let hash = hash?;
            if !existing.contains(hash.as_str()) { Self::persist_forget_marker_locked(&conn, &hash)?; }
        }
        let mut stmt=conn.prepare("SELECT target_kind || ':' || source_hash FROM forgotten_captures")?;
        for marker in stmt.query_map([],|r|r.get::<_,String>(0))? {
            let marker=marker?;
            if !existing.contains(marker.as_str()) {Self::persist_forget_marker_locked(&conn,&marker)?;}
        }
        let mut stmt = conn.prepare("SELECT DISTINCT 'foreign_capture:' || target_chain || ':' || target_seq
            FROM foreign_redactions WHERE chain_id=target_chain
            UNION SELECT 'foreign_capture:' || replace(substr(key,length('polis.foreignForgotten.')+1),'.',':') FROM polis_meta WHERE key LIKE 'polis.foreignForgotten.%'")?;
        for marker in stmt.query_map([], |r| r.get::<_,String>(0))? {
            let marker = marker?;
            if !existing.contains(marker.as_str()) { Self::persist_forget_marker_locked(&conn, &marker)?; }
        }
        Ok(())
    }

    /// Compaction candidates: warm prompts (`gist IS NULL`) at least `size_floor`
    /// bytes. A prompt links into a node by ledger seq
    /// (`class_links.target_id` = the prompt event's seq), so we resolve
    /// seq → `prompt_id` here; the keeper groups the rows by prompt and applies
    /// the role/cold/pinned/size interlocks in pure code.
    ///
    /// Warm prompts worth considering for compaction, as flat
    /// `(id, bytes, role, node_id)` rows — one per accepted class link, or a
    /// single row with `node_id = NULL` for machine text.
    ///
    /// The class-link join is now a LEFT join, because machine text
    /// (`agent`/`system`) is deliberately kept out of the classifier and so can
    /// never acquire a link to go cold *through*. Without this it would be
    /// immortal: excluded from classification by Phase 1.5, and therefore never
    /// selectable — the 6.9 MB would sit on disk forever while the only rows
    /// still reachable by the blade were the user's own. Machine rows qualify on
    /// lake-relative age instead (`machine_cold_before_ts`).
    pub fn list_compaction_candidates(
        &self,
        size_floor: i64,
        machine_cold_before_ts: i64,
    ) -> rusqlite::Result<Vec<(i64, i64, String, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT p.id, LENGTH(CAST(p.body AS BLOB)) AS bytes,
                    COALESCE(p.role, 'user') AS role, l.node_id
             FROM prompts p
             JOIN ledger_events le ON le.prompt_id = p.id AND le.kind = 'prompt'
             LEFT JOIN class_links l
               ON l.target_kind = 'prompt'
              AND CAST(l.target_id AS INTEGER) = le.seq
              AND l.status = 'accepted' AND l.retired_by_run IS NULL
             WHERE p.gist IS NULL
               AND LENGTH(CAST(p.body AS BLOB)) >= ?1
               AND (l.node_id IS NOT NULL
                    OR (COALESCE(p.role, 'user') <> 'user' AND p.ts <= ?2))",
        )?;
        let rows = stmt.query_map(params![size_floor, machine_cold_before_ts], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect()
    }

    /// Aggregate compaction stats for the memory pill/inspector:
    /// `(compacted_count, reclaimed_bytes, newest_compacted_at?)`.
    pub fn compaction_stats(&self) -> rusqlite::Result<(i64, i64, Option<i64>)> {
        let conn = self.conn();
        conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(original_bytes), 0),
                    MAX(compacted_at)
             FROM prompts WHERE gist IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    }
}
