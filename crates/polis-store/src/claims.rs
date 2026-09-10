// SPDX-License-Identifier: Apache-2.0
//! Temporal claim projection. The immutable claim record commits the cited
//! assertion to the ledger; rebuilding the projection never rewrites hashes.

use crate::{principals::ScopeFilter, PolisStore};
use polis_core::claims::*;
use rusqlite::{params, Connection, OptionalExtension};

fn invalid(message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::InvalidParameterName(message.into())
}
fn encode<T: serde::Serialize>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| invalid(e.to_string()))
}
fn decode(value: &str) -> rusqlite::Result<ClaimWrite> {
    serde_json::from_str(value).map_err(|e| invalid(format!("invalid claim record: {e}")))
}

impl PolisStore {
    /// Structured projection write, with no model call. Literal supporting
    /// passages and source roles are checked; semantic entailment is not
    /// inferred from citation existence. Gardener assertions therefore stay
    /// labelled as derivations and cannot override a user's structured claim.
    pub fn write_claim(&self, write: &ClaimWrite, recorded_at: i64) -> rusqlite::Result<Claim> {
        if write.id.trim().is_empty()
            || write.id.len() > 200
            || write.subject.trim().is_empty()
            || write.sources.is_empty()
            || write.sources.len() > 32
            || write
                .valid_until
                .is_some_and(|until| until <= write.valid_from)
            || write.value.supporting_text().is_empty()
            || matches!(write.value, ClaimValue::Number(n) if !n.is_finite())
        {
            return Err(invalid(
                "claim requires an ID, subject, finite value, citations and a valid interval",
            ));
        }
        if write.derivation == ClaimDerivation::Gardener
            && (write.organizer_run.is_none()
                || write.model_version.as_deref().is_none_or(str::is_empty))
        {
            return Err(invalid(
                "gardener claims require organizerRun and modelVersion",
            ));
        }
        let json = encode(write)?;
        if json.len() > 64_000 {
            return Err(invalid("claim exceeds 64000 bytes"));
        }
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some((existing, ts, seq)) = tx.query_row(
            "SELECT assertion_json, recorded_at, event_seq FROM claim_records WHERE id = ?1 AND invalidated = 0",
            [&write.id], |r| Ok((r.get::<_, Option<String>>(0)?, r.get(1)?, r.get(2)?)),
        ).optional()? {
            if existing.as_deref() != Some(&json) { return Err(invalid("claim ID already exists with different content")); }
            return Ok(Claim { assertion: write.clone(), recorded_at: ts, event_seq: seq, unresolved_alternatives: vec![] });
        }
        if write.derivation == ClaimDerivation::Gardener {
            let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM class_runs WHERE id = ?1 AND status = 'running' AND COALESCE(outcome, '') NOT IN ('reverted', 'reverted_by_canary'))",
                [write.organizer_run], |r| r.get(0))?;
            if !active {
                return Err(invalid(
                    "gardener claims require the current running organizer run",
                ));
            }
        }
        let f = ScopeFilter::from_scope(&write.scope);
        let clause = Self::scope_clause_locked(&tx, "p", &f)?;
        let mut derived_principal = None;
        let mut derived_project = None;
        for (index, source) in write.sources.iter().enumerate() {
            let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(source.seq)];
            // Scope binds are re-created since trait objects cannot be cloned.
            binds.extend(Self::scope_clause_locked(&tx, "p", &f)?.binds);
            let row = tx.query_row(&format!(
                "SELECT p.body, p.role, p.device_id, p.principal_id, p.project_path FROM prompts p
                 JOIN ledger_events e ON e.prompt_id = p.id AND e.kind = 'prompt'
                 WHERE e.seq = ? AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources) {}", clause.sql),
                rusqlite::params_from_iter(binds.iter().map(|v| v.as_ref())),
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?)),
            ).optional()?.ok_or_else(|| invalid(format!("citation {} is unavailable or outside scope", source.seq)))?;
            let role = match row.1.as_deref().unwrap_or("user") {
                "user" => ClaimRole::User,
                "assistant" => ClaimRole::Assistant,
                "agent" => ClaimRole::Agent,
                "system" => ClaimRole::System,
                "tool" => ClaimRole::Tool,
                _ => return Err(invalid("unknown evidence role")),
            };
            if source.chain_id != row.2.as_deref().unwrap_or("local") || source.role != role {
                return Err(invalid("citation chain or role does not match evidence"));
            }
            let supports = !source.quote.trim().is_empty()
                && match source.start_byte {
                    Some(start) => {
                        row.0.get(start..start.saturating_add(source.quote.len()))
                            == Some(&source.quote)
                    }
                    None => row.0.contains(&source.quote),
                };
            if !supports || !source.quote.contains(&write.value.supporting_text()) {
                return Err(invalid(
                    "claim value requires an exact supporting passage in every cited source",
                ));
            }
            if index == 0 {
                derived_principal = row.3;
                derived_project = row.4;
            } else if derived_principal != row.3 || derived_project != row.4 {
                return Err(invalid("claim sources must share a principal and project"));
            }
        }
        for id in write.supersedes.iter().chain(&write.contradicts) {
            let old_json: String = tx
                .query_row(
                    "SELECT assertion_json FROM claims WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| invalid("claim relationship references an unavailable claim"))?;
            let old = decode(&old_json)?;
            if old.subject != write.subject
                || old.predicate != write.predicate
                || old.scope != write.scope
            {
                return Err(invalid(
                    "related claims must have the same subject, predicate and scope",
                ));
            }
            let (old_principal, old_project): (Option<String>, Option<String>) = tx.query_row(
                "SELECT principal_id, project_path FROM claims WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if old_principal != derived_principal || old_project != derived_project {
                return Err(invalid(
                    "related claims must belong to the same evidence namespace",
                ));
            }
            if write.supersedes.contains(id)
                && old.sources.iter().any(|s| s.role == ClaimRole::User)
                && (write.derivation != ClaimDerivation::Structured
                    || !write.sources.iter().all(|s| s.role == ClaimRole::User))
            {
                return Err(invalid(
                    "a derived or assistant claim cannot supersede explicit user evidence",
                ));
            }
            let old_recorded: i64 =
                tx.query_row("SELECT recorded_at FROM claims WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
            if recorded_at < old_recorded {
                return Err(invalid("supersession cannot precede the recorded claim"));
            }
        }
        let hash = polis_core::ledger::body_hash(&json);
        let event = crate::ledger::append_event(
            &tx,
            &polis_core::ledger::LedgerAppend {
                kind: "claim",
                author: self.author(),
                ts: recorded_at,
                prompt_id: None,
                session_id: None,
                version_number: None,
                ref_kind: Some("claim"),
                ref_id: Some(&write.id),
                payload_hash: &hash,
            },
        )?;
        tx.execute("INSERT INTO claim_records(id, event_seq, recorded_at, assertion_json) VALUES (?1, ?2, ?3, ?4)",
            params![write.id, event.seq, recorded_at, json])?;
        project(
            &tx,
            write,
            &json,
            recorded_at,
            event.seq,
            None,
            derived_principal.as_deref(),
            derived_project.as_deref(),
        )?;
        if write.derivation == ClaimDerivation::Gardener {
            Self::journal_op_locked(
                &tx,
                write.organizer_run.unwrap(),
                &crate::runs::OpRecord::applied(
                    "claim",
                    vec![format!("claim:{}", write.id)],
                    serde_json::json!({"claimId": write.id}),
                )
                .with_ledger_seq(Some(event.seq)),
            )?;
        }
        tx.commit()?;
        Ok(Claim {
            assertion: write.clone(),
            recorded_at,
            event_seq: event.seq,
            unresolved_alternatives: vec![],
        })
    }

    pub fn query_claims(&self, query: &ClaimQuery) -> rusqlite::Result<Vec<Claim>> {
        let now = polis_core::ledger::now_millis();
        let valid = query.valid_at.unwrap_or(now);
        let known = query.known_at.unwrap_or(now);
        let conn = self.conn();
        let scoped = source_eligibility(&conn, query)?;
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(known), Box::new(valid)];
        binds.extend(scoped.binds);
        let mut sql = format!(
            "SELECT c.assertion_json, c.recorded_at, c.event_seq FROM claims c
            WHERE c.recorded_at <= ?1 AND c.valid_from <= ?2
              AND (c.valid_until IS NULL OR c.valid_until > ?2)
              AND (c.retired_at IS NULL OR c.retired_at > ?1) {}",
            scoped.sql
        );
        if let Some(subject) = &query.subject {
            sql.push_str(" AND c.subject = ?");
            binds.push(Box::new(subject.clone()));
        }
        if let Some(predicate) = &query.predicate {
            sql.push_str(" AND c.predicate = ?");
            binds.push(Box::new(encode(predicate)?));
        }
        sql.push_str(" ORDER BY c.recorded_at DESC, c.id");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(binds.iter().map(|v| v.as_ref())),
                |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut claims = rows
            .into_iter()
            .map(|(json, recorded_at, event_seq)| {
                Ok(Claim {
                    assertion: decode(&json)?,
                    recorded_at,
                    event_seq,
                    unresolved_alternatives: vec![],
                })
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Supersession takes effect from its declared valid time, even after
        // the replacement's own interval ends. Never resurrect the old claim.
        let relation_scope = source_eligibility(&conn, query)?;
        let mut relation_binds: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(known), Box::new(valid)];
        relation_binds.extend(relation_scope.binds);
        let mut related = conn.prepare(&format!(
            "SELECT assertion_json FROM claims c WHERE recorded_at <= ?1 AND valid_from <= ?2
            AND (retired_at IS NULL OR retired_at > ?1) {}",
            relation_scope.sql
        ))?;
        let mut superseded = std::collections::HashSet::new();
        for json in related.query_map(
            rusqlite::params_from_iter(relation_binds.iter().map(|v| v.as_ref())),
            |r| r.get::<_, String>(0),
        )? {
            superseded.extend(decode(&json?)?.supersedes);
        }
        claims.retain(|c| !superseded.contains(&c.assertion.id));
        for index in 0..claims.len() {
            claims[index].unresolved_alternatives = claims
                .iter()
                .filter(|other| {
                    other.assertion.id != claims[index].assertion.id
                        && other.assertion.subject == claims[index].assertion.subject
                        && other.assertion.predicate == claims[index].assertion.predicate
                        && other.assertion.scope == claims[index].assertion.scope
                        && other.assertion.value != claims[index].assertion.value
                })
                .map(|c| c.assertion.id.clone())
                .collect();
        }
        if let Some(raw) = query.q.as_deref().filter(|q| !q.trim().is_empty()) {
            let terms = polis_core::query::plan_fts_query(raw)
                .map(|p| p.terms)
                .unwrap_or_else(|| vec![raw.to_lowercase()]);
            let score = |claim: &Claim| {
                let text = format!(
                    "{} {:?} {}",
                    claim.assertion.subject,
                    claim.assertion.predicate,
                    claim.assertion.value.supporting_text()
                )
                .to_lowercase();
                terms
                    .iter()
                    .filter(|term| text.contains(term.as_str()))
                    .count()
            };
            claims.retain(|c| score(c) > 0);
            claims.sort_by_key(|c| std::cmp::Reverse((score(c), c.recorded_at)));
        }
        claims.truncate(query.limit.unwrap_or(50).clamp(1, 200));
        Ok(claims)
    }

    pub fn retire_claim(&self, id: &str, recorded_at: i64) -> rusqlite::Result<bool> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = Self::retire_claim_locked(&tx, id, recorded_at, self.author())?;
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn retire_claim_locked(
        conn: &Connection,
        id: &str,
        recorded_at: i64,
        author: &str,
    ) -> rusqlite::Result<bool> {
        let changed = conn.execute("UPDATE claim_records SET retired_at = ?2 WHERE id = ?1 AND retired_at IS NULL AND recorded_at <= ?2", params![id, recorded_at])?;
        conn.execute("UPDATE claims SET retired_at = ?2 WHERE id = ?1 AND retired_at IS NULL AND recorded_at <= ?2", params![id, recorded_at])?;
        if changed > 0 {
            crate::ledger::append_event(
                conn,
                &polis_core::ledger::LedgerAppend {
                    kind: "claim_retired",
                    author,
                    ts: recorded_at,
                    prompt_id: None,
                    session_id: None,
                    version_number: None,
                    ref_kind: Some("claim"),
                    ref_id: Some(id),
                    payload_hash: &polis_core::ledger::body_hash(id),
                },
            )?;
        }
        Ok(changed > 0)
    }

    /// Forgetting is absolute, including historical queries: remove all
    /// copied assertion text while preserving ledger commitments and IDs.
    pub fn invalidate_prompt_claims_locked(
        conn: &Connection,
        prompt_id: i64,
    ) -> rusqlite::Result<usize> {
        let select = "SELECT claim_id FROM claim_sources WHERE seq IN (SELECT seq FROM ledger_events WHERE prompt_id = ?1 AND kind = 'prompt')";
        let changed = conn.execute(&format!("UPDATE claim_records SET assertion_json = NULL, invalidated = 1 WHERE id IN ({select})"), [prompt_id])?;
        conn.execute(
            &format!("DELETE FROM claims WHERE id IN ({select})"),
            [prompt_id],
        )?;
        Ok(changed)
    }

    pub fn rebuild_claim_projection(&self) -> rusqlite::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let rows = tx.prepare("SELECT assertion_json, recorded_at, event_seq, retired_at FROM claim_records WHERE invalidated = 0 ORDER BY recorded_at, event_seq")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute("DELETE FROM claims", [])?;
        tx.execute("DELETE FROM claim_sources WHERE claim_id IN (SELECT id FROM claim_records WHERE invalidated = 0)", [])?;
        for (json, recorded, seq, retired) in &rows {
            let w = decode(json)?;
            let first = &w.sources[0];
            let (principal, source_project): (Option<String>, Option<String>) = tx.query_row(
                "SELECT p.principal_id, p.project_path FROM prompts p JOIN ledger_events e ON e.prompt_id = p.id WHERE e.seq = ?1", [first.seq], |r| Ok((r.get(0)?, r.get(1)?)))?;
            project(
                &tx,
                &w,
                json,
                *recorded,
                *seq,
                *retired,
                principal.as_deref(),
                source_project.as_deref(),
            )?;
        }
        tx.commit()?;
        Ok(rows.len())
    }
}

#[allow(clippy::too_many_arguments)]
fn project(
    conn: &Connection,
    write: &ClaimWrite,
    json: &str,
    recorded_at: i64,
    event_seq: i64,
    retired_at: Option<i64>,
    principal: Option<&str>,
    source_project: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO claims(id, subject, predicate, assertion_json, recorded_at, event_seq,
        valid_from, valid_until, retired_at, principal_id, agent_id, run_id, org_id, project_path)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            write.id,
            write.subject,
            encode(&write.predicate)?,
            json,
            recorded_at,
            event_seq,
            write.valid_from,
            write.valid_until,
            retired_at,
            principal,
            write.scope.agent,
            write.scope.run,
            write.scope.org,
            source_project
        ],
    )?;
    for source in &write.sources {
        conn.execute(
            "INSERT OR IGNORE INTO claim_sources(claim_id, chain_id, seq) VALUES (?1, ?2, ?3)",
            params![write.id, source.chain_id, source.seq],
        )?;
    }
    Ok(())
}

fn source_eligibility(
    conn: &Connection,
    query: &ClaimQuery,
) -> rusqlite::Result<crate::principals::ScopeSql> {
    let mut filter = ScopeFilter::from_scope(&query.scope);
    filter.roles = if query.evidence_filter.roles.is_empty() {
        vec!["user".into(), "assistant".into()]
    } else {
        query
            .evidence_filter
            .roles
            .iter()
            .map(|role| {
                polis_core::ledger::CorpusRole::parse(role)
                    .map(|role| role.as_str().to_string())
                    .ok_or_else(|| invalid(format!("unsupported evidence role: {role}")))
            })
            .collect::<rusqlite::Result<_>>()?
    };
    filter.after = query.evidence_filter.after;
    filter.before = query.evidence_filter.before;
    let scoped = PolisStore::scope_clause_locked(conn, "p", &filter)?;
    // Check every supporting source against current authoritative scope and
    // redaction state, before claim ranking or limits. This also survives
    // identity adoption after a claim was first projected.
    Ok(crate::principals::ScopeSql {
        sql: format!(
            " AND EXISTS (SELECT 1 FROM claim_sources cs WHERE cs.claim_id = c.id)
        AND NOT EXISTS (SELECT 1 FROM claim_sources cs WHERE cs.claim_id = c.id AND NOT EXISTS (
            SELECT 1 FROM prompts p JOIN ledger_events e ON e.prompt_id = p.id AND e.kind = 'prompt'
            WHERE e.seq = cs.seq AND (cs.chain_id = 'local' OR p.device_id = cs.chain_id)
            AND NOT EXISTS (SELECT 1 FROM forgotten_sources f WHERE f.prompt_id = p.id) {}))",
            scoped.sql
        ),
        binds: scoped.binds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn evidence(store: &PolisStore, text: &str, role: &str) -> ClaimSource {
        let hash = polis_core::ledger::body_hash(text);
        let id = {
            let conn = store.conn();
            conn.execute("INSERT INTO prompts(ts, source, surface, role, body, body_hash, user_text) VALUES(1, 'hook', 'test', ?1, ?2, ?3, ?2)", params![role, text, hash]).unwrap();
            conn.last_insert_rowid()
        };
        let e = store
            .append_event(&polis_core::ledger::LedgerAppend {
                kind: "prompt",
                author: "local",
                ts: 1,
                prompt_id: Some(id),
                session_id: None,
                version_number: None,
                ref_kind: None,
                ref_id: None,
                payload_hash: &hash,
            })
            .unwrap();
        ClaimSource {
            chain_id: "local".into(),
            seq: e.seq,
            role: if role == "user" {
                ClaimRole::User
            } else {
                ClaimRole::Assistant
            },
            quote: text.into(),
            start_byte: Some(0),
        }
    }
    fn claim(store: &PolisStore, id: &str, value: &str, start: i64, role: &str) -> ClaimWrite {
        ClaimWrite {
            id: id.into(),
            subject: "editor".into(),
            predicate: ClaimPredicate::Preference,
            value: ClaimValue::Text(value.into()),
            scope: Default::default(),
            sources: vec![evidence(
                store,
                &format!("My editor preference is {value}"),
                role,
            )],
            derivation: ClaimDerivation::Structured,
            valid_from: start,
            valid_until: None,
            supersedes: vec![],
            contradicts: vec![],
            organizer_run: None,
            model_version: None,
        }
    }
    fn query(store: &PolisStore, valid: i64, known: i64) -> Vec<String> {
        store
            .query_claims(&ClaimQuery {
                valid_at: Some(valid),
                known_at: Some(known),
                ..Default::default()
            })
            .unwrap()
            .into_iter()
            .map(|c| c.assertion.id)
            .collect()
    }
    #[test]
    fn valid_and_known_time_are_independent_and_projection_rebuilds() {
        let store = PolisStore::open_in_memory().unwrap();
        let a = claim(&store, "old", "Vim", 10, "user");
        store.write_claim(&a, 20).unwrap();
        let mut b = claim(&store, "correction", "Emacs", 30, "user");
        b.supersedes.push(a.id.clone());
        store.write_claim(&b, 50).unwrap();
        assert_eq!(
            query(&store, 40, 40),
            ["old"],
            "the retroactive correction was not yet known"
        );
        assert_eq!(query(&store, 40, 60), ["correction"]);
        assert_eq!(
            query(&store, 20, 60),
            ["old"],
            "valid history survives new knowledge"
        );
        assert!(query(&store, 40, 19).is_empty());
        let before = store.chain_head().unwrap();
        assert_eq!(store.rebuild_claim_projection().unwrap(), 2);
        assert_eq!(store.chain_head().unwrap(), before);
        assert_eq!(query(&store, 40, 60), ["correction"]);
        assert!(store.verify_ledger_chain().unwrap().ok);
    }
    #[test]
    fn assistant_cannot_override_user_and_alternatives_remain_visible() {
        let store = PolisStore::open_in_memory().unwrap();
        let a = claim(&store, "user", "Vim", 1, "user");
        store.write_claim(&a, 10).unwrap();
        let mut b = claim(&store, "assistant", "Emacs", 1, "assistant");
        b.supersedes.push("user".into());
        assert!(store
            .write_claim(&b, 20)
            .unwrap_err()
            .to_string()
            .contains("cannot supersede"));
        b.supersedes.clear();
        b.contradicts.push("user".into());
        store.write_claim(&b, 20).unwrap();
        let found = store
            .query_claims(&ClaimQuery {
                valid_at: Some(30),
                known_at: Some(30),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(found.len(), 2);
        assert!(found.iter().all(|c| c.unresolved_alternatives.len() == 1));
    }
    #[test]
    fn citation_existence_does_not_validate_an_unsupported_value_or_role() {
        let store = PolisStore::open_in_memory().unwrap();
        let mut c = claim(&store, "a", "Vim", 1, "user");
        c.value = ClaimValue::Text("Emacs".into());
        assert!(store.write_claim(&c, 10).is_err());
        c.value = ClaimValue::Text("Vim".into());
        c.sources[0].role = ClaimRole::Assistant;
        assert!(store.write_claim(&c, 10).is_err());
        c.sources[0].role = ClaimRole::User;
        c.sources[0].chain_id = "another-device".into();
        assert!(store.write_claim(&c, 10).is_err());
    }
    #[test]
    fn source_roles_and_time_are_filtered_before_the_claim_limit() {
        let store = PolisStore::open_in_memory().unwrap();
        let user = claim(&store, "user", "Vim", 1, "user");
        store.write_claim(&user, 10).unwrap();
        let assistant = claim(&store, "assistant", "Emacs", 1, "assistant");
        store.write_claim(&assistant, 20).unwrap();
        let mut q = ClaimQuery {
            q: Some("editor".into()),
            valid_at: Some(30),
            known_at: Some(30),
            limit: Some(1),
            evidence_filter: polis_core::api::EvidenceFilter {
                roles: vec!["user".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let result = store.query_claims(&q).unwrap();
        assert_eq!(result[0].assertion.id, "user");
        assert!(result[0].unresolved_alternatives.is_empty());
        q.evidence_filter.after = Some(2);
        assert!(store.query_claims(&q).unwrap().is_empty());
        q.evidence_filter.after = None;
        q.q = Some("unrelated".into());
        assert!(store.query_claims(&q).unwrap().is_empty());
    }
    #[test]
    fn compact_then_forget_removes_archive_user_text_and_all_claim_history() {
        let store = PolisStore::open_in_memory().unwrap();
        let c = claim(&store, "private", "Vim", 1, "user");
        store.write_claim(&c, 10).unwrap();
        let pid: i64 = store
            .conn()
            .query_row(
                "SELECT prompt_id FROM ledger_events WHERE seq = ?1",
                [c.sources[0].seq],
                |r| r.get(0),
            )
            .unwrap();
        assert!(store
            .compact_prompt_body(
                pid,
                "editor preference Vim",
                "cold",
                "deterministic",
                "test"
            )
            .unwrap()
            .is_some());
        assert_eq!(store.archive_stats().unwrap().0, 1);
        assert!(store
            .compact_prompt_body(pid, "[forgotten]", "forget", "deterministic", "test")
            .unwrap()
            .is_some());
        assert_eq!(store.archive_stats().unwrap().0, 0);
        assert!(!store.restore_prompt_body(pid).unwrap());
        assert!(query(&store, 20, 20).is_empty());
        assert_eq!(store.rebuild_claim_projection().unwrap(), 0);
        assert!(query(&store, 20, 20).is_empty());
        let conn = store.conn();
        let (body, user_text, gist): (String, Option<String>, String) = conn
            .query_row(
                "SELECT body,user_text,gist FROM prompts WHERE id = ?1",
                [pid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (body, user_text, gist),
            (String::new(), None, "[forgotten]".into())
        );
        drop(conn);
        assert!(store.verify_ledger_chain().unwrap().ok);
    }
}
