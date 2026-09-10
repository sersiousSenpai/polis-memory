// SPDX-License-Identifier: Apache-2.0
//! A bounded invariant sample, frozen alongside reachability probes. These
//! use real scoped retrieval and temporal queries; no model or invented facts.
use super::{Gold, Probe, ProbeResult, Subject};
use crate::Polis;
use polis_core::{
    api::{EvidenceFilter, Scope, SearchRequest},
    claims::{ClaimQuery, ClaimWrite},
};
use polis_store::principals::ScopeFilter;
use std::time::Instant;

pub(super) fn freeze(polis: &Polis<'_>, run: u64, head: i64) -> Vec<Probe> {
    let mut out = Vec::new();
    let sources = (|| -> rusqlite::Result<Vec<(i64, String, String, Scope)>> {
        let conn = polis.store.conn();
        let mut stmt = conn.prepare(
            "SELECT e.seq, substr(p.body, 1, 400), COALESCE(p.role, 'user'),
            p.principal_id, p.project_path, p.agent_id, p.run_id, p.org_id
            FROM ledger_events e JOIN prompts p ON p.id=e.prompt_id
            WHERE e.kind='prompt' AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources)
            ORDER BY e.seq DESC LIMIT 8",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    Scope {
                        principal: r.get(3)?,
                        project: r.get(4)?,
                        agent: r.get(5)?,
                        run: r.get(6)?,
                        org: r.get(7)?,
                        include_shared: false,
                    },
                ))
            })?
            .collect();
        rows
    })()
    .unwrap_or_default();
    for (seq, body, role, scope) in sources {
        let query = super::tokens(&body)
            .into_iter()
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        if query.is_empty() {
            continue;
        }
        out.push(Probe {
            subject: Subject::EvidencePolicy,
            query,
            gold: Gold::EvidencePolicy {
                scope,
                roles: vec![role],
                max_bytes: 4096,
                expected_seq: Some(seq),
                expect_empty: false,
            },
        });
    }
    // Query real text through a namespace verified to contain no sources.
    // This catches scope leakage without treating a random query as an
    // authoritative natural-language no-answer label.
    if let Some(first) = out.first() {
        let project = format!("polis-canary-absent-{run}-{head}");
        let exists: bool = polis.store.conn().query_row("SELECT EXISTS(SELECT 1 FROM prompts WHERE project_path=?1 UNION ALL SELECT 1 FROM browse_events WHERE project_path=?1 UNION ALL SELECT 1 FROM user_notes WHERE project_path=?1 UNION ALL SELECT 1 FROM class_nodes WHERE project_path=?1)", [&project], |r|r.get(0)).unwrap_or(true);
        if !exists {
            out.push(Probe {
                subject: Subject::EvidencePolicy,
                query: first.query.clone(),
                gold: Gold::EvidencePolicy {
                    scope: Scope {
                        project: Some(project),
                        ..Default::default()
                    },
                    roles: vec![],
                    max_bytes: 4096,
                    expected_seq: None,
                    expect_empty: true,
                },
            });
        }
    }
    let claims = (|| -> rusqlite::Result<Vec<(String, i64)>> {
        let conn = polis.store.conn();
        let mut stmt = conn.prepare("SELECT assertion_json, recorded_at FROM claims WHERE retired_at IS NULL ORDER BY recorded_at DESC, id LIMIT 8")?;
        let rows = stmt.query_map([], |r|Ok((r.get(0)?,r.get(1)?)))?.collect();
        rows
    })().unwrap_or_default();
    for (json, recorded_at) in claims {
        let Ok(claim) = serde_json::from_str::<ClaimWrite>(&json) else {
            continue;
        };
        let roles = claim
            .sources
            .iter()
            .filter_map(|s| {
                serde_json::to_value(s.role)
                    .ok()?
                    .as_str()
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        for (valid, known, expected) in [
            (claim.valid_from, recorded_at, true),
            (claim.valid_from.saturating_sub(1), recorded_at, false),
            (claim.valid_from, recorded_at.saturating_sub(1), false),
        ] {
            out.push(Probe {
                subject: Subject::TemporalClaim,
                query: claim.subject.clone(),
                gold: Gold::ClaimAt {
                    id: claim.id.clone(),
                    scope: claim.scope.clone(),
                    roles: roles.clone(),
                    valid_at: valid,
                    known_at: known,
                    expected,
                },
            });
        }
    }
    out
}

pub(super) fn run(polis: &Polis<'_>, probe: &Probe, limit: i64) -> Option<ProbeResult> {
    let start = Instant::now();
    let mut pack_bytes = 0;
    let hit = match &probe.gold {
        Gold::ClaimAt {
            id,
            scope,
            roles,
            valid_at,
            known_at,
            expected,
        } => {
            let result = polis.store.query_claims(&ClaimQuery {
                subject: Some(probe.query.clone()),
                scope: scope.clone(),
                valid_at: Some(*valid_at),
                known_at: Some(*known_at),
                evidence_filter: EvidenceFilter {
                    roles: roles.clone(),
                    ..Default::default()
                },
                limit: Some(200),
                ..Default::default()
            });
            result.is_ok_and(|claims| {
                pack_bytes = serde_json::to_vec(&claims).map_or(0, |v| v.len());
                let found = claims.iter().find(|c| &c.assertion.id == id);
                found.is_some() == *expected && found.is_none_or(|c| supported(polis, &c.assertion))
            })
        }
        Gold::EvidencePolicy {
            scope,
            roles,
            max_bytes,
            expected_seq,
            expect_empty,
        } => {
            let req = SearchRequest {
                q: Some(probe.query.clone()),
                scope: scope.clone(),
                limit: Some(limit),
                max_tokens: Some(*max_bytes),
                filter: EvidenceFilter {
                    roles: roles.clone(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut filter = ScopeFilter::from_scope(scope);
            filter.roles = roles.clone();
            crate::retrieval::search_request(polis, &req, &filter).is_ok_and(|pack| {
                pack_bytes = serde_json::to_vec(&pack).map_or(usize::MAX, |v| v.len());
                let mut refs = Vec::new();
                for hit in &pack.prompt_hits {
                    refs.push(hit.item.seq);
                    refs.extend(&hit.duplicate_of);
                    refs.extend(hit.superseded_by);
                }
                refs.extend(pack.browse_hits.iter().filter_map(|h| h.seq));
                refs.extend(pack.notes.iter().filter_map(|n| n.seq));
                refs.extend(pack.grep_hits.iter().filter_map(|h| h.seq));
                refs.extend(
                    pack.claims
                        .iter()
                        .flat_map(|c| c.assertion.sources.iter().map(|s| s.seq)),
                );
                if let Some(node) = &pack.node {
                    refs.extend(
                        node.links
                            .iter()
                            .filter(|l| {
                                matches!(
                                    l.link.target_kind.as_str(),
                                    "prompt"
                                        | "decision"
                                        | "ledger"
                                        | "resolution"
                                        | "approval"
                                        | "review_verdict"
                                )
                            })
                            .filter_map(|l| l.link.target_id.parse::<i64>().ok()),
                    );
                    refs.extend(node.links.iter().filter_map(|l| l.superseded_by));
                    refs.extend(
                        node.observations
                            .iter()
                            .flat_map(|o| o.cite_seqs.iter().copied()),
                    );
                }
                let eligible = polis
                    .store
                    .eligible_seqs(&refs, &filter)
                    .unwrap_or_default();
                pack_bytes <= *max_bytes
                    && pack.retrieval.errors.is_empty()
                    && refs.iter().all(|s| eligible.contains(s))
                    && expected_seq.is_none_or(|seq| {
                        pack.prompt_hits
                            .iter()
                            .any(|h| h.item.seq == seq || h.duplicate_of.contains(&seq))
                    })
                    && (!expect_empty
                        || (refs.is_empty()
                            && pack.node.is_none()
                            && pack.matched_nodes.is_empty()
                            && pack.shared_hits.is_empty()
                            && pack.notes.is_empty()
                            && pack.grep_hits.is_empty()
                            && pack.claims.is_empty()
                            && pack.browse_hits.is_empty()))
            })
        }
        _ => return None,
    };
    Some(ProbeResult {
        subject: probe.subject,
        hit,
        rank: None,
        ms: start.elapsed().as_millis().min(u32::MAX as u128) as u32,
        pack_bytes,
        arms: vec![],
    })
}

fn supported(polis: &Polis<'_>, claim: &ClaimWrite) -> bool {
    claim.sources.iter().all(|source| {
        polis.store.conn().query_row("SELECT p.body, COALESCE(p.role,'user') FROM prompts p JOIN ledger_events e ON e.prompt_id=p.id WHERE e.seq=?1 AND e.kind='prompt' AND p.id NOT IN (SELECT prompt_id FROM forgotten_sources)", [source.seq], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))
            .is_ok_and(|(body,role)| {
                serde_json::to_value(source.role).ok().and_then(|v|v.as_str().map(str::to_string)).as_deref()==Some(&role)
                    && source.quote.contains(&claim.value.supporting_text())
                    && source.start_byte.map_or_else(||body.contains(&source.quote),|start|body.get(start..start.saturating_add(source.quote.len()))==Some(&source.quote))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use polis_core::{claims::*, host::NoHost};
    use polis_llm::NoopSink;
    use polis_store::PolisStore;

    fn fixture() -> PolisStore {
        let db = PolisStore::open_in_memory().unwrap();
        let body = "The project selected technology is Cedar for durable local storage.";
        let hash = polis_core::ledger::body_hash(body);
        let id = {
            let conn = db.conn();
            conn.execute("INSERT INTO prompts(ts, source, surface, role, body, body_hash, user_text, project_path) VALUES(10,'hook','test','assistant',?1,?2,?1,'/project-a')",rusqlite::params![body,hash]).unwrap();
            conn.last_insert_rowid()
        };
        let seq = db
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
        db.write_claim(
            &ClaimWrite {
                id: "canary-claim".into(),
                subject: "storage".into(),
                predicate: ClaimPredicate::SelectedTechnology,
                value: ClaimValue::Text("Cedar".into()),
                scope: Scope {
                    project: Some("/project-a".into()),
                    ..Default::default()
                },
                sources: vec![ClaimSource {
                    chain_id: "local".into(),
                    seq,
                    role: ClaimRole::Assistant,
                    quote: body.into(),
                    start_byte: Some(0),
                }],
                derivation: ClaimDerivation::Structured,
                valid_from: 20,
                valid_until: None,
                supersedes: vec![],
                contradicts: vec![],
                organizer_run: None,
                model_version: None,
            },
            100,
        )
        .unwrap();
        db
    }

    #[test]
    fn policy_canary_exercises_role_scope_budget_empty_namespace_and_both_time_axes() {
        let db = fixture();
        let polis = Polis::new(&db, None, &NoHost, &NoopSink);
        let probes = freeze(&polis, 1, db.max_ledger_seq().unwrap());
        assert_eq!(
            probes
                .iter()
                .filter(|p| p.subject == Subject::TemporalClaim)
                .count(),
            3
        );
        assert!(probes.iter().any(|p| matches!(
            p.gold,
            Gold::EvidencePolicy {
                expect_empty: true,
                ..
            }
        )));
        for probe in &probes {
            let result = run(&polis, probe, 8).unwrap();
            assert!(result.hit, "policy baseline failed: {:?}", probe.gold);
            if probe.subject == Subject::EvidencePolicy {
                assert!(result.pack_bytes <= 4096);
            }
        }
        // A projection bug exposing a claim before it was known is a
        // regression even though the source remains textually retrievable.
        db.conn()
            .execute(
                "UPDATE claims SET recorded_at=1 WHERE id='canary-claim'",
                [],
            )
            .unwrap();
        assert!(probes
            .iter()
            .filter(|p| p.subject == Subject::TemporalClaim)
            .any(|p| !run(&polis, p, 8).unwrap().hit));
    }

    #[test]
    fn an_authority_change_is_a_zero_tolerance_canary_regression() {
        let db = fixture();
        let polis = Polis::new(&db, None, &NoHost, &NoopSink);
        let cfg = super::super::CanaryConfig::default();
        let set = super::super::freeze(&polis, 2, &cfg);
        let (before, _) = super::super::evaluate(&polis, &set, &cfg);
        db.conn()
            .execute("UPDATE prompts SET role='user'", [])
            .unwrap();
        let (after, _) = super::super::evaluate(&polis, &set, &cfg);
        let reason = super::super::regression(&before, &after, &cfg).unwrap();
        assert!(reason.contains("zero tolerance"), "{reason}");
    }
}
