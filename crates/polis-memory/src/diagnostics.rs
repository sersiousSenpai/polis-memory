// SPDX-License-Identifier: Apache-2.0
//! Exact evidence resolution and body-free diagnostic inspection.
use crate::Polis;
use polis_core::{
    diagnostics::{EvidenceRecord, EvidenceRequest, RetrievalTrace, TraceRequest},
    types::LakeItem,
    MemoryError,
};
use polis_store::{principals::ScopeFilter, PolisStore};
use rusqlite::OptionalExtension;
fn err(e: impl std::fmt::Display) -> MemoryError {
    MemoryError::Store(e.to_string())
}

pub fn evidence(
    polis: &Polis<'_>,
    req: &EvidenceRequest,
    scope: &ScopeFilter,
) -> Result<EvidenceRecord, MemoryError> {
    let db = polis.store;
    let mut out = EvidenceRecord {
        seq: req.seq,
        chain_id: req.chain_id.clone().unwrap_or_else(|| "local".into()),
        status: "unavailable".into(),
        item: None,
    };
    if let Some(chain) = req.chain_id.as_deref().filter(|c| *c != "local") {
        let local:Option<String>=db.conn().query_row("SELECT p.device_id FROM ledger_events le LEFT JOIN decision_evidence de ON de.seq=le.seq LEFT JOIN ledger_events src ON src.seq=de.source_seq JOIN prompts p ON p.id=COALESCE(le.prompt_id,src.prompt_id) WHERE le.seq=?1 AND (de.seq IS NULL OR (src.kind='prompt' AND le.ref_kind='prompt' AND le.ref_id=CAST(p.id AS TEXT) AND le.payload_hash=p.body_hash))",[req.seq],|r|r.get(0)).optional().map_err(err)?.flatten();
        let local = if local.is_some() {
            local
        } else {
            db.conn().query_row("SELECT device_id FROM user_notes WHERE id=(SELECT ne.note_id FROM note_events ne JOIN ledger_events e ON e.seq=ne.seq AND e.payload_hash=ne.payload_hash WHERE ne.seq=?1)
                UNION ALL SELECT be.device_id FROM browse_events be JOIN ledger_events e ON e.ref_kind='browse_event' AND e.ref_id=CAST(be.id AS TEXT) WHERE e.seq=?1 LIMIT 1",
                [req.seq], |r|r.get::<_,Option<String>>(0)).optional().map_err(err)?.flatten()
        };
        if local.as_deref() != Some(chain) {
            if !scope.include_shared {
                return Ok(out);
            }
            let conn = db.conn();
            let filter = PolisStore::foreign_scope_clause_locked(&conn, scope)
                .map_err(err)?
                .numbered(3);
            let mut stmt=conn.prepare(&format!("SELECT p.role,p.text,p.project,p.tombstoned,fe.ts,fe.session_id FROM foreign_prompts p JOIN foreign_events fe ON fe.chain_id=p.chain_id AND fe.seq=p.seq WHERE p.chain_id=?1 AND p.seq=?2{}",filter.sql)).map_err(err)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = vec![&chain, &req.seq];
            binds.extend(filter.binds.iter().map(|v| v.as_ref()));
            let row = stmt
                .query_row(binds.as_slice(), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, bool>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                })
                .optional()
                .map_err(err)?;
            if let Some((role, body, project, forgotten, ts, session)) = row {
                if forgotten {
                    out.status = "redacted".into();
                    return Ok(out);
                }
                let mut item = blank_item(req.seq, ts, "prompt");
                item.body = body;
                item.role = Some(role);
                item.project_path = project;
                item.session_id = session;
                out.status = if item.body.is_some() {
                    "available"
                } else {
                    "unavailable"
                }
                .into();
                out.item = Some(item);
            }
            return Ok(out);
        }
    }
    if !db
        .eligible_seqs(&[req.seq], scope)
        .map_err(err)?
        .contains(&req.seq)
    {
        return Ok(out);
    }
    let page_forgotten:bool=db.conn().query_row("SELECT EXISTS(SELECT 1 FROM ledger_events le JOIN forgotten_captures f ON f.target_kind=le.ref_kind AND CAST(f.target_id AS TEXT)=le.ref_id WHERE le.seq=?1)",[req.seq],|r|r.get(0)).map_err(err)?;
    if page_forgotten {
        out.status = "redacted".into();
        return Ok(out);
    }
    if let Some(note) = db.note_for_seq(req.seq).map_err(err)? {
        let conn = db.conn();
        let forgotten: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM forgotten_captures WHERE target_kind='user_note' AND target_id=?1)", [note.id], |r| r.get(0)).map_err(err)?;
        if forgotten {
            out.status = "redacted".into();
            return Ok(out);
        }
        let (ts, hash): (i64, String) = conn
            .query_row(
                "SELECT ts,payload_hash FROM ledger_events WHERE seq=?1",
                [req.seq],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(err)?;
        // A current row is not evidence for the previous version's words.
        // Only return it if its text still satisfies that event's commitment.
        let supported = ["note", "star", "unstar"].iter().any(|action| {
            polis_core::ledger::decision_payload_hash(&[("action", action), ("text", &note.text)])
                == hash
        });
        if supported {
            let mut item = blank_item(req.seq, ts, "note");
            item.body = Some(note.text);
            item.role = Some("annotation".into());
            item.ref_kind = Some("user_note".into());
            item.ref_id = Some(note.id.to_string());
            out.status = "available".into();
            out.item = Some(item);
        }
        return Ok(out);
    }
    let row=db.conn().query_row("SELECT le.ts,le.kind,le.ref_kind,le.ref_id,le.session_id,p.device_id,EXISTS(SELECT 1 FROM forgotten_sources fs WHERE fs.prompt_id=COALESCE(le.prompt_id,src.prompt_id)) FROM ledger_events le LEFT JOIN decision_evidence de ON de.seq=le.seq LEFT JOIN ledger_events src ON src.seq=de.source_seq LEFT JOIN prompts p ON p.id=COALESCE(le.prompt_id,src.prompt_id) WHERE le.seq=?1 AND (de.seq IS NULL OR (src.kind='prompt' AND le.ref_kind='prompt' AND le.ref_id=CAST(p.id AS TEXT) AND le.payload_hash=p.body_hash))",[req.seq],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,bool>(6)?))).optional().map_err(err)?;
    let Some((ts, kind, ref_kind, ref_id, session, chain, forgotten)) = row else {
        return Ok(out);
    };
    out.chain_id = chain.unwrap_or_else(|| "local".into());
    if forgotten {
        out.status = "redacted".into();
        return Ok(out);
    }
    if let Some(item) = db
        .lake_items_for_seqs_scoped(&[req.seq], scope)
        .map_err(err)?
        .into_iter()
        .next()
    {
        out.status = "available".into();
        out.item = Some(item);
        return Ok(out);
    }
    if let Some((item, _)) = db
        .decision_items_scoped(None, Some(&[req.seq]), 1, scope)
        .map_err(err)?
        .into_iter()
        .next()
    {
        out.status = "available".into();
        out.item = Some(item);
        return Ok(out);
    }
    let mut item = blank_item(req.seq, ts, &kind);
    item.session_id = session;
    item.ref_kind = ref_kind.clone();
    item.ref_id = ref_id.clone();
    if ref_kind.as_deref() == Some("browse_event") {
        item.body = db
            .conn()
            .query_row(
                "SELECT text FROM browse_events WHERE id=?1",
                [ref_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        item.role = Some("page".into());
    } else if kind == "user_note" || ref_kind.as_deref() == Some("user_note") {
        item.body = db
            .conn()
            .query_row("SELECT text FROM user_notes WHERE seq=?1", [req.seq], |r| {
                r.get(0)
            })
            .optional()
            .map_err(err)?;
        item.role = Some("annotation".into());
    }
    out.status = if item.body.is_some() {
        "available"
    } else {
        "legacy_gap"
    }
    .into();
    out.item = Some(item);
    Ok(out)
}
fn blank_item(seq: i64, ts: i64, kind: &str) -> LakeItem {
    LakeItem {
        seq,
        ts,
        kind: kind.into(),
        surface: None,
        origin: None,
        role: None,
        session_id: None,
        mission_id: None,
        project_path: None,
        ref_kind: None,
        ref_id: None,
        body: None,
        thread_kind: None,
        thread_id: None,
        parent_session_id: None,
        model: None,
    }
}

pub fn traces(
    polis: &Polis<'_>,
    req: &TraceRequest,
    scope: &ScopeFilter,
) -> Result<Vec<RetrievalTrace>, MemoryError> {
    let principal_ids = scope
        .principal
        .as_ref()
        .map(|p| polis.store.principal_id_set(p))
        .transpose()
        .map_err(err)?;
    let mut rows = polis
        .store
        .retrieval_traces(req.id.as_deref(), 1000)
        .map_err(err)?;
    rows.retain(|t| {
        principal_ids
            .as_ref()
            .is_none_or(|ids| t.scope.principal.as_ref().is_some_and(|p| ids.contains(p)))
            && scope
                .project
                .as_ref()
                .is_none_or(|p| t.scope.project.as_ref() == Some(p))
            && scope
                .agent
                .as_ref()
                .is_none_or(|p| t.scope.agent.as_ref() == Some(p))
            && scope
                .run
                .as_ref()
                .is_none_or(|p| t.scope.run.as_ref() == Some(p))
            && scope
                .org
                .as_ref()
                .is_none_or(|p| t.scope.org.as_ref() == Some(p))
    });
    rows.truncate(req.limit.unwrap_or(50).clamp(1, 200));
    // No old text is embedded in traces. Selected seqs remain citations and
    // resolve as redacted/unavailable after forgetting or visibility changes.
    Ok(rows)
}
