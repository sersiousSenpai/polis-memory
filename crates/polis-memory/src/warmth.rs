// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Anti-decay without pins (plan §5.1): a branch is protected from collapse
//! and its prompts from compaction while it is WARM — served by an answer
//! pack or a context block inside the freshest slice of the lake's span —
//! or while a user note targets it. Nothing a person flips.
//!
//! The read path stays write-free: `record_pack` drops the served node and
//! link ids into an in-memory ring, and the gardener's tick flushes the ring
//! into `class_nodes.last_recalled_at` / `class_links.last_recalled_at`.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use polis_core::coldness::{is_warm, subtree_stats_protected, BranchStat, LakeEnvelope};
use polis_core::pack::AnswerPack;
use polis_core::types::ClassNode;
use polis_store::PolisStore;

/// The ring's ceiling: past it the oldest recalls are dropped (a recall that
/// old is about to be flushed anyway; the gardener ticks every 30 s).
pub const RECALL_LOG_MAX: usize = 4096;

#[derive(Default)]
struct RecallLog {
    nodes: HashMap<String, i64>,
    links: HashMap<i64, i64>,
}

fn log() -> &'static Mutex<RecallLog> {
    static LOG: OnceLock<Mutex<RecallLog>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(RecallLog::default()))
}

fn lock() -> std::sync::MutexGuard<'static, RecallLog> {
    log().lock().unwrap_or_else(|e| e.into_inner())
}

/// What an answer pack served, at `now_ms`: the resolved node, its links,
/// and the matched runners-up. One call at the end of the pack builder.
pub fn record_pack(pack: &AnswerPack, now_ms: i64) {
    let mut l = lock();
    if let Some(n) = &pack.node {
        l.nodes.insert(n.node.id.clone(), now_ms);
        for link in &n.links {
            l.links.insert(link.link.id, now_ms);
        }
    }
    for n in &pack.matched_nodes {
        l.nodes.insert(n.id.clone(), now_ms);
    }
    if l.nodes.len() + l.links.len() > RECALL_LOG_MAX {
        // Drop the oldest half by timestamp.
        let mut ts: Vec<i64> = l.nodes.values().chain(l.links.values()).copied().collect();
        ts.sort_unstable();
        let cut = ts[ts.len() / 2];
        l.nodes.retain(|_, t| *t > cut);
        l.links.retain(|_, t| *t > cut);
    }
}

/// How many recalls the ring holds (tests, the health line).
pub fn pending() -> usize {
    let l = lock();
    l.nodes.len() + l.links.len()
}

/// Flush the ring into the store (the gardener's tick). Returns the rows
/// stamped.
pub fn flush(store: &PolisStore) -> usize {
    let (nodes, links) = {
        let mut l = lock();
        (std::mem::take(&mut l.nodes), std::mem::take(&mut l.links))
    };
    if nodes.is_empty() && links.is_empty() {
        return 0;
    }
    let nodes: Vec<(String, i64)> = nodes.into_iter().collect();
    let links: Vec<(i64, i64)> = links.into_iter().collect();
    match store.bump_recalled(&nodes, &links) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "warmth flush failed");
            0
        }
    }
}

/// The nodes that are protected IN THEMSELVES: warm, or carrying a user
/// note. Roll it up the tree with `protected_branches` for the collapse and
/// compaction gates.
pub fn direct_protected(store: &PolisStore, env: LakeEnvelope) -> HashSet<String> {
    let mut out: HashSet<String> = store
        .node_warmth()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, last)| is_warm(*last, env))
        .map(|(id, _)| id)
        .collect();
    out.extend(store.noted_node_ids().unwrap_or_default());
    out
}

/// Every node whose subtree holds a protected node — the set the compaction
/// pass keeps its blade away from, and the `pinned` flag `auto_collapse_safe`
/// reads, fed by warmth. Returns the rolled stats too, so a caller needs one
/// pass over the tree.
pub fn protected_branches(
    nodes: &[ClassNode],
    direct_activity: &HashMap<String, (i64, Option<i64>)>,
    direct_protected: &HashSet<String>,
) -> (HashMap<String, BranchStat>, HashSet<String>) {
    let stats = subtree_stats_protected(nodes, direct_activity, direct_protected);
    let branches = stats.iter().filter(|(_, s)| s.pinned).map(|(id, _)| id.clone()).collect();
    (stats, branches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmth_is_lake_relative() {
        let env = LakeEnvelope { oldest: 0, newest: 1000 };
        assert!(is_warm(Some(900), env), "recalled in the freshest third");
        assert!(is_warm(Some(660), env));
        assert!(!is_warm(Some(600), env), "recalled before the freshest third");
        assert!(!is_warm(None, env));
    }

    #[test]
    fn a_warm_leaf_protects_its_branch_and_nothing_beside_it() {
        let n = |id: &str, parent: Option<&str>| ClassNode {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            kind: "node".into(),
            title: id.into(),
            summary: None,
            project_path: None,
            ip_name: None,
            status: "accepted".into(),
            pinned: false,
            curated_by: None,
            created_at: 0,
            updated_at: 0,
        };
        let nodes = vec![n("root", None), n("mid", Some("root")), n("leaf", Some("mid")), n("other", Some("root"))];
        let direct: HashSet<String> = ["leaf".to_string()].into_iter().collect();
        let (_, branches) = protected_branches(&nodes, &HashMap::new(), &direct);
        assert!(branches.contains("leaf") && branches.contains("mid") && branches.contains("root"));
        assert!(!branches.contains("other"));
    }
}
