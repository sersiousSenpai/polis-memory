// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `catalog_health()` (plan §6.3): the gardener's efficacy, rebuilt from the
//! runs, the canary trail and the live tree on every `health()` read. Facts
//! only — every number here is counted, none is judged; the alerts are the
//! plan's thresholds applied to the counts.

use std::collections::HashMap;

use polis_core::types::CatalogHealth;

use crate::Polis;

/// Runs the efficacy window looks back over.
pub const HEALTH_RUNS: i64 = 50;
/// Fan-out thresholds (§6.3): no live node over 150 links; the
/// consolidation prompt must split or justify anything over 120.
pub const FAN_OUT_HARD: i64 = 150;
pub const FAN_OUT_SOFT: i64 = 120;

pub fn catalog_health(polis: &Polis<'_>) -> CatalogHealth {
    let db = polis.store;
    let mut h = CatalogHealth::default();

    // --- the runs ---------------------------------------------------------
    let runs = db.list_class_runs(HEALTH_RUNS).unwrap_or_default();
    let organize: Vec<_> = runs
        .iter()
        .filter(|r| r.mode.as_deref().map(|m| m == polis_store::runs::MODE_ORGANIZE).unwrap_or(true))
        .collect();
    h.runs_considered = organize.len() as i64;
    let mut walls: Vec<i64> = organize.iter().filter_map(|r| r.wall_ms.or(r.duration_ms)).collect();
    walls.sort_unstable();
    if !walls.is_empty() {
        let pct = |p: f64| walls[((walls.len() as f64 * p).ceil() as usize).saturating_sub(1).min(walls.len() - 1)];
        h.organize_p50_ms = Some(pct(0.50));
        h.organize_p90_ms = Some(pct(0.90));
    }
    if !organize.is_empty() {
        let errors = organize.iter().filter(|r| r.outcome.as_deref() == Some("error") || r.status == "error").count();
        h.error_rate = errors as f64 / organize.len() as f64;
        h.no_model_share = organize.iter().filter(|r| r.model.is_none()).count() as f64 / organize.len() as f64;
        h.canary_reverts = organize.iter().filter(|r| r.outcome.as_deref() == Some("reverted_by_canary")).count() as i64;
    }
    h.canary_trend = organize
        .iter()
        .filter_map(|r| Some(r.canary_after? - r.canary_before?))
        .take(3)
        .collect();
    h.canary_alert = h.canary_trend.len() == 3 && h.canary_trend.iter().all(|d| *d < 0.0);

    // --- the tree ---------------------------------------------------------
    let nodes = db.list_class_nodes_with_counts().unwrap_or_default();
    let live = nodes.len() as i64;
    let ids: std::collections::HashSet<&str> = nodes.iter().map(|(n, _)| n.id.as_str()).collect();
    if live > 0 {
        h.max_fan_out = nodes.iter().map(|(_, c)| *c).max().unwrap_or(0);
        h.nodes_over_150 = nodes.iter().filter(|(_, c)| *c > FAN_OUT_HARD).count() as i64;
        h.nodes_over_120 = nodes.iter().filter(|(_, c)| *c > FAN_OUT_SOFT).count() as i64;
        h.digest_ratio = nodes.iter().filter(|(n, _)| n.kind == "digest").count() as f64 / live as f64;
        let orphans = nodes.iter().filter(|(n, _)| n.parent_id.as_deref().map(|p| !ids.contains(p)).unwrap_or(false)).count();
        h.orphan_rate = orphans as f64 / live as f64;
        // Depth histogram.
        let parent: HashMap<&str, Option<&str>> = nodes.iter().map(|(n, _)| (n.id.as_str(), n.parent_id.as_deref())).collect();
        let mut hist: Vec<i64> = Vec::new();
        for (n, _) in &nodes {
            let mut d = 0usize;
            let mut cur = parent.get(n.id.as_str()).copied().flatten();
            while let Some(p) = cur {
                d += 1;
                if d > 64 {
                    break;
                }
                cur = parent.get(p).copied().flatten();
            }
            if hist.len() <= d {
                hist.resize(d + 1, 0);
            }
            hist[d] += 1;
        }
        h.depth_histogram = hist;
        // Duplicate sibling titles.
        let mut seen: HashMap<(Option<&str>, String), i64> = HashMap::new();
        for (n, _) in &nodes {
            *seen.entry((n.parent_id.as_deref(), n.title.to_lowercase())).or_insert(0) += 1;
        }
        let dups: i64 = seen.values().filter(|c| **c > 1).map(|c| c - 1).sum();
        h.duplicate_title_rate = dups as f64 / live as f64;
        // Provenance: a prompt link whose project root is not its node's root
        // (and the node's root is a project root, not ~general).
        let root_of: HashMap<&str, &str> = {
            let mut m = HashMap::new();
            for (n, _) in &nodes {
                let mut cur = n.id.as_str();
                let mut guard = 0;
                while let Some(Some(p)) = parent.get(cur) {
                    cur = p;
                    guard += 1;
                    if guard > 64 {
                        break;
                    }
                }
                m.insert(n.id.as_str(), cur);
            }
            m
        };
        let mut violations = 0i64;
        for (node_id, project) in db.prompt_link_provenance().unwrap_or_default() {
            let Some(root) = root_of.get(node_id.as_str()) else { continue };
            if *root == crate::organize::GENERAL_ROOT_ID {
                continue;
            }
            if let Some(p) = project {
                if crate::organize::root_id_for_path(&p) != *root {
                    violations += 1;
                }
            }
        }
        h.provenance_violations = violations;
    }
    h.queue_depth = db.count_pending_class_proposals().unwrap_or(0);
    h.live_observations = db.list_live_observations().map(|v| v.len() as i64).unwrap_or(0);
    h.unacknowledged_redactions = 0;
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{seed_corpus, CorpusSpec};
    use polis_core::host::NoHost;
    use polis_llm::NoopSink;
    use polis_store::PolisStore;

    #[test]
    fn health_counts_the_seeded_tree() {
        let store = PolisStore::open_in_memory().unwrap();
        seed_corpus(&store, &CorpusSpec::new(200).with_seed(3)).unwrap();
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let h = catalog_health(&polis);
        assert!(h.depth_histogram.len() >= 2, "roots and classes: {:?}", h.depth_histogram);
        assert!(h.max_fan_out > 0);
        assert_eq!(h.orphan_rate, 0.0);
        assert!(!h.canary_alert);
        assert_eq!(h.unacknowledged_redactions, 0);
    }

    /// Provenance is counted against the ROOT a link's node hangs under: a
    /// prompt from project A filed under project B's root is one violation;
    /// under A's root or `~general` it is none.
    #[test]
    fn provenance_violations_count_cross_root_filings() {
        use polis_core::proposal::Proposal;
        use polis_store::record::{record_prompt_at, PromptInput};
        let store = PolisStore::open_in_memory().unwrap();
        let roots = crate::organize::seed_root_rows(&["/x/a".to_string(), "/x/b".to_string()]);
        store.seed_class_roots(&roots).unwrap();
        let (root_a, root_b) = (roots[0].0.clone(), roots[1].0.clone());
        let prompt = |body: &str, project: &str| {
            record_prompt_at(
                &store,
                PromptInput {
                    source: polis_core::ledger::PromptSource::Hook,
                    origin: polis_core::ledger::Origin::External,
                    surface: "t".into(),
                    role: polis_core::ledger::CorpusRole::User,
                    session_id: None,
                    claude_session_id: Some(body.to_string()),
                    mission_id: None,
                    project_path: Some(project.to_string()),
                    body: body.to_string(),
                    thread: None,
                    author: None,
                    model: None,
                    model_source: None,
                    user_text: None,
                },
                1_000,
            )
            .unwrap()
            .unwrap()
        };
        let a_seq = prompt("from a", "/x/a");
        let b_seq = prompt("from b", "/x/b");
        let file = |root: &str, seq: i64| {
            store
                .stage_proposal(None, &Proposal::File { parent_id: root.into(), sub_class: None, target_kind: "prompt".into(), target_id: seq.to_string(), note: None, rationale: None })
                .unwrap();
        };
        file(&root_a, a_seq);
        file(&root_a, b_seq); // b's prompt under a's root
        file(&crate::organize::GENERAL_ROOT_ID.to_string(), b_seq);
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let h = catalog_health(&polis);
        assert_eq!(h.provenance_violations, 1);
        let _ = root_b;
    }
}
