// SPDX-License-Identifier: Apache-2.0
//! The classifier may propose a few literal, cited claims beside proposals.
//! Runtime provenance is never accepted from its JSON.

use crate::fence::FencedItem;
use polis_core::{api::Scope, claims::*};
use polis_store::{runs::OpRecord, PolisStore};
use rusqlite::OptionalExtension;
use serde::Deserialize;

pub(super) const MAX_CLAIMS: usize = 8;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Draft {
    subject: String,
    predicate: ClaimPredicate,
    value: ClaimValue,
    source_seq: i64,
    quote: String,
}

pub(super) fn apply_reply(
    db: &PolisStore,
    run_id: i64,
    model: Option<&str>,
    text: &str,
    shown: &[FencedItem],
) -> Result<(usize, usize), String> {
    let refuse = |reason: String| -> Result<(), String> {
        db.journal_op(run_id, &OpRecord::refused("claim", vec![], reason))
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    if text.len() > 64_000 {
        refuse("claim output exceeds 64000 bytes".into())?;
        return Ok((0, 1));
    }
    let Some(object) = polis_core::json::extract_object_with_key(text, "claims") else {
        return Ok((0, 0));
    };
    let Some(drafts) = object["claims"]
        .as_array()
        .filter(|v| v.len() <= MAX_CLAIMS)
    else {
        refuse(format!(
            "claims must be an array of at most {MAX_CLAIMS} entries"
        ))?;
        return Ok((0, 1));
    };
    let mut admitted = 0;
    let mut refused = 0;
    for value in drafts {
        let result = (|| -> Result<(), String> {
            let model = model
                .filter(|m| !m.trim().is_empty() && m.len() <= 200)
                .ok_or("backend did not report a model version")?;
            let draft: Draft =
                serde_json::from_value(value.clone()).map_err(|e| format!("invalid claim: {e}"))?;
            if draft.subject.len() > 200
                || draft.quote.is_empty()
                || draft.quote.len() > 4096
                || draft.value.supporting_text().len() > 1024
            {
                return Err("claim subject, passage or value exceeds its bound".into());
            }
            let item = shown
                .iter()
                .find(|item| item.label == "seq" && item.id == draft.source_seq.to_string())
                .ok_or("claim cites a source absent from the classifier's bounded prompt")?;
            if !matches!(item.role.as_str(), "user" | "assistant")
                || !item.text.contains(&draft.quote)
            {
                return Err(
                    "claim needs an exact passage shown with user or assistant source role".into(),
                );
            }
            // Read authoritative source metadata, not the model's assertions
            // about a source. write_claim repeats eligibility/span checks
            // inside its atomic write transaction and stamps the run journal.
            let source = db.conn().query_row(
                "SELECT p.body, COALESCE(p.role, 'user'), COALESCE(p.device_id, 'local'),
                        p.principal_id, p.project_path, p.agent_id, p.run_id, p.org_id, e.ts
                 FROM ledger_events e JOIN prompts p ON p.id = e.prompt_id
                 WHERE e.seq = ?1 AND e.kind = 'prompt' AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources)",
                [draft.source_seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                    Scope {principal:r.get(3)?, project:r.get(4)?, agent:r.get(5)?, run:r.get(6)?, org:r.get(7)?, include_shared:false},
                    r.get::<_, i64>(8)?)),
            ).optional().map_err(|e| e.to_string())?.ok_or("claim source is unavailable")?;
            if source.1 != item.role {
                return Err("claim source role changed after the prompt was built".into());
            }
            let start = source
                .0
                .find(&draft.quote)
                .ok_or("claim passage is not an exact source span")?;
            let role = if source.1 == "user" {
                ClaimRole::User
            } else {
                ClaimRole::Assistant
            };
            let digest = polis_core::ledger::body_hash(&value.to_string());
            let claim = ClaimWrite {
                id: format!("gardener-{run_id}-{}", &digest[..24]),
                subject: draft.subject,
                predicate: draft.predicate,
                value: draft.value,
                scope: source.3,
                sources: vec![ClaimSource {
                    chain_id: source.2,
                    seq: draft.source_seq,
                    role,
                    quote: draft.quote,
                    start_byte: Some(start),
                }],
                derivation: ClaimDerivation::Gardener,
                valid_from: source.4,
                valid_until: None,
                supersedes: vec![],
                contradicts: vec![],
                organizer_run: Some(run_id),
                model_version: Some(model.into()),
            };
            db.write_claim(&claim, polis_core::ledger::now_millis())
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        match result {
            Ok(()) => admitted += 1,
            Err(reason) => {
                refuse(reason)?;
                refused += 1;
            }
        }
    }
    Ok((admitted, refused))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fence::Fence;

    fn fixture(role: &str) -> (PolisStore, i64, i64, Vec<FencedItem>, String) {
        let db = PolisStore::open_in_memory().unwrap();
        let body = "For this project I suggest Cedar as the selected technology.";
        let hash = polis_core::ledger::body_hash(body);
        let id = {
            let conn = db.conn();
            conn.execute("INSERT INTO prompts(ts, source, surface, role, body, body_hash, user_text, project_path, run_id) VALUES(10,'hook','test',?1,?2,?3,?2,'/project-a','capture-run')", rusqlite::params![role,body,hash]).unwrap();
            conn.last_insert_rowid()
        };
        let source = db
            .append_event(&polis_core::ledger::LedgerAppend {
                kind: "prompt",
                author: "local",
                ts: 10,
                prompt_id: Some(id),
                session_id: None,
                version_number: None,
                ref_kind: None,
                ref_id: None,
                payload_hash: &hash,
            })
            .unwrap()
            .seq;
        let run = db.insert_class_run(source, source).unwrap();
        let fence = Fence::with_nonce("claim-test");
        let shown = fence.split(&fence.wrap("seq", &source.to_string(), role, None, body));
        let reply = serde_json::json!({"proposals":[],"claims":[{"subject":"project stack","predicate":"selected_technology","value":{"type":"text","value":"Cedar"},"sourceSeq":source,"quote":body}]}).to_string();
        (db, source, run, shown, reply)
    }

    #[test]
    fn gardener_claims_stamp_source_role_scope_model_and_atomic_revert_journal() {
        let (db, source, run, shown, reply) = fixture("assistant");
        assert_eq!(
            apply_reply(&db, run, Some("fixture-model-v3"), &reply, &shown).unwrap(),
            (1, 0)
        );
        let claims = db.query_claims(&ClaimQuery::default()).unwrap();
        assert_eq!(claims.len(), 1);
        let claim = &claims[0].assertion;
        assert_eq!(claim.derivation, ClaimDerivation::Gardener);
        assert_eq!(claim.organizer_run, Some(run));
        assert_eq!(claim.model_version.as_deref(), Some("fixture-model-v3"));
        assert_eq!(claim.scope.project.as_deref(), Some("/project-a"));
        assert_eq!(claim.scope.run.as_deref(), Some("capture-run"));
        assert_eq!(claim.sources[0].seq, source);
        assert_eq!(claim.sources[0].role, ClaimRole::Assistant);
        assert_eq!(claim.sources[0].start_byte, Some(0));
        assert_eq!(
            db.list_run_ops(run)
                .unwrap()
                .iter()
                .filter(|o| o.op == "claim" && o.outcome == "applied")
                .count(),
            1
        );
        assert!(matches!(
            db.revert_run(run, "test").unwrap(),
            polis_store::runs::RevertOutcome::Reverted(_)
        ));
        assert!(db.query_claims(&ClaimQuery::default()).unwrap().is_empty());
        db.rebuild_claim_projection().unwrap();
        assert!(db.query_claims(&ClaimQuery::default()).unwrap().is_empty());
        assert!(db.verify_ledger_chain().unwrap().ok);
    }

    #[test]
    fn gardener_claims_reject_unshown_text_forged_metadata_missing_model_and_finished_runs() {
        let (db, _, run, shown, reply) = fixture("user");
        assert_eq!(apply_reply(&db, run, None, &reply, &shown).unwrap(), (0, 1));
        assert_eq!(
            apply_reply(&db, run, Some("m1"), &reply, &[]).unwrap(),
            (0, 1)
        );
        let mut clipped = shown.clone();
        clipped[0].text = "For this project".into();
        assert_eq!(
            apply_reply(&db, run, Some("m1"), &reply, &clipped).unwrap(),
            (0, 1)
        );
        let mut forged: serde_json::Value = serde_json::from_str(&reply).unwrap();
        forged["claims"][0]["organizerRun"] = 999.into();
        assert_eq!(
            apply_reply(&db, run, Some("m1"), &forged.to_string(), &shown).unwrap(),
            (0, 1)
        );
        let mut unsupported: serde_json::Value = serde_json::from_str(&reply).unwrap();
        unsupported["claims"][0]["value"]["value"] = "Juniper".into();
        assert_eq!(
            apply_reply(&db, run, Some("m1"), &unsupported.to_string(), &shown).unwrap(),
            (0, 1)
        );
        db.conn()
            .execute("UPDATE class_runs SET status='done' WHERE id=?1", [run])
            .unwrap();
        assert_eq!(
            apply_reply(&db, run, Some("m1"), &reply, &shown).unwrap(),
            (0, 1)
        );
        assert!(db.query_claims(&ClaimQuery::default()).unwrap().is_empty());
        assert!(db
            .list_run_ops(run)
            .unwrap()
            .iter()
            .all(|o| o.outcome == "refused"));
    }

    #[test]
    fn gardener_claim_output_count_is_bounded() {
        let (db, _, run, shown, reply) = fixture("user");
        let parsed: serde_json::Value = serde_json::from_str(&reply).unwrap();
        let oversized =
            serde_json::json!({"claims":vec![parsed["claims"][0].clone();MAX_CLAIMS+1]})
                .to_string();
        assert_eq!(
            apply_reply(&db, run, Some("m1"), &oversized, &shown).unwrap(),
            (0, 1)
        );
        assert!(db.query_claims(&ClaimQuery::default()).unwrap().is_empty());
    }

    struct ModelVersionFixture {
        reply: String,
    }
    #[polis_llm::async_trait]
    impl polis_llm::Agent for ModelVersionFixture {
        fn name(&self) -> &'static str {
            "fixture-backend"
        }
        async fn run(
            &self,
            req: polis_llm::AgentRequest,
        ) -> Result<polis_llm::AgentReply, polis_llm::AgentError> {
            let text = if req.prompt.contains("optional top-level `claims`") {
                self.reply.clone()
            } else {
                crate::scripted::filing_reply(&req.seat, &req.prompt)
            };
            Ok(polis_llm::AgentReply {
                text,
                json: None,
                session_id: None,
                usage: polis_llm::Usage {
                    model: Some("fixture-exact-model-v4".into()),
                    ..Default::default()
                },
                clipped: false,
            })
        }
    }

    #[tokio::test]
    async fn real_organizer_consumes_optional_claims_with_the_current_run_and_reported_version() {
        let (db, _, earlier_run, _, reply) = fixture("assistant");
        // The fixture's starter row is not the organizer's runtime row.
        db.conn()
            .execute(
                "UPDATE class_runs SET status='error', outcome='interrupted' WHERE id=?1",
                [earlier_run],
            )
            .unwrap();
        db.set_meta(crate::filing::HEALTH_PRESSURE_KEY, "1")
            .unwrap();
        db.set_meta(
            crate::filing::ORGANIZE_COUNT_KEY,
            &(crate::filing::CONSOLIDATE_EVERY - 1).to_string(),
        )
        .unwrap();
        let agent = std::sync::Arc::new(ModelVersionFixture { reply });
        let polis = crate::Polis::new(
            &db,
            Some(agent),
            &polis_core::host::NoHost,
            &polis_llm::NoopSink,
        );
        let outcome = super::super::organize_once(&polis).await.unwrap();
        assert!(outcome.ran);
        let claims = db.query_claims(&ClaimQuery::default()).unwrap();
        assert_eq!(claims.len(), 1, "{}", outcome.summary);
        assert_eq!(claims[0].assertion.organizer_run, outcome.run_id);
        assert_ne!(outcome.run_id, Some(earlier_run));
        assert_eq!(
            claims[0].assertion.model_version.as_deref(),
            Some("fixture-exact-model-v4")
        );
        assert_eq!(
            db.get_class_run(outcome.run_id.unwrap())
                .unwrap()
                .unwrap()
                .model
                .as_deref(),
            Some("fixture-exact-model-v4")
        );
    }
}
