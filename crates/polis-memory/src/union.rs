// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The union arm (Session E3, plan §4.6): hits from imported foreign chains,
//! fused across their lexical and semantic evidence, each labelled by its
//! source. Runs ONLY under `include_shared`, and returns a separate list —
//! a peer's words never sit in `prompt_hits` beside the user's own. Queries
//! stay local: nothing here reaches a peer, so no peer learns what was asked.

use polis_core::pack::{rrf_fuse, Arm, ArmCoverage, ForeignHit};
use polis_core::query::plan_fts_query;
use polis_store::foreign::ForeignPromptRow;

use crate::Polis;

/// The label a consumer shows beside a foreign hit: the peer's display name
/// when the chain card carried one, else `shared:<fingerprint>`.
pub fn source_label(polis: &Polis<'_>, chain_id: &str) -> String {
    match polis.store.get_foreign_chain(chain_id) {
        Ok(Some(c)) => match c.display_name.filter(|n| !n.trim().is_empty()) {
            Some(n) => format!("shared:{n}"),
            None => format!("shared:{}", polis_core::identity::fingerprint(chain_id)),
        },
        _ => format!("shared:{}", polis_core::identity::fingerprint(chain_id)),
    }
}

fn to_hit(polis: &Polis<'_>, row: ForeignPromptRow, arms: Vec<polis_core::pack::ArmHit>, score: f64) -> ForeignHit {
    ForeignHit {
        source: source_label(polis, &row.chain_id),
        chain_id: row.chain_id,
        seq: row.seq,
        role: row.role,
        redaction: row.redaction,
        text: row.text,
        project: row.project,
        arms,
        score,
    }
}

/// Foreign hits for a question: BM25 over the foreign bodies' own index and,
/// when an embedder is configured, cosine over the foreign vectors the index
/// tick re-embedded locally — fused with the same reciprocal-rank rule the
/// local pack uses. Empty when there is no question.
pub fn shared_hits(polis: &Polis<'_>, query: Option<&str>, limit: i64) -> Vec<ForeignHit> {
    let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) else { return Vec::new() };
    let limit = limit.clamp(1, 200);
    let store = polis.store;

    let lexical: Vec<(String, f64)> = crate::latency::timed("pack.shared.lexical", || {
        let Some(plan) = plan_fts_query(q) else { return Vec::new() };
        let lookup = |m: &str| match store.search_foreign_prompts(m, limit) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "foreign lexical arm failed");
                Vec::new()
            }
        };
        let mut rows = lookup(&plan.and_match);
        if rows.is_empty() && plan.or_match != plan.and_match {
            rows = lookup(&plan.or_match);
        }
        rows.into_iter().map(|(r, bm25)| (format!("{}:{}", r.chain_id, r.seq), -bm25)).collect()
    });

    let semantic: Vec<(String, f64)> = crate::latency::timed("pack.shared.semantic", || {
        let Some(embedder) = polis.embedder.as_deref() else { return Vec::new() };
        let Some(hits) = polis_embed::semantic_search(store, embedder, q, (limit as usize) * 4) else { return Vec::new() };
        hits.into_iter()
            .filter(|h| h.target_kind == "foreign_prompt")
            .filter_map(|h| store.foreign_prompt_by_id(h.target_id).ok().flatten().map(|r| (format!("{}:{}", r.chain_id, r.seq), f64::from(h.score))))
            .take(limit as usize)
            .collect()
    });

    if lexical.is_empty() && semantic.is_empty() {
        return Vec::new();
    }
    let fused = rrf_fuse(&[(Arm::Lexical, lexical), (Arm::Semantic, semantic)]);
    let mut out = Vec::new();
    for (key, arms, score) in fused.into_iter().take(limit as usize) {
        let Some((chain, seq)) = key.rsplit_once(':') else { continue };
        let Ok(seq) = seq.parse::<i64>() else { continue };
        if let Ok(Some(row)) = store.foreign_prompt(chain, seq) {
            out.push(to_hit(polis, row, arms, score));
        }
    }
    out
}

/// The coverage row for the union arm — absent, with the reason, unless the
/// scope asked for shared content.
pub fn with_shared_coverage(mut coverage: Vec<ArmCoverage>, include_shared: bool, hits: usize) -> Vec<ArmCoverage> {
    coverage.push(ArmCoverage {
        arm: Arm::Shared,
        ran: include_shared,
        hits,
        absent_because: (!include_shared).then(|| "shared chains are read only under include_shared".to_string()),
    });
    coverage
}
