// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Adjudication per op (plan §5.1): the deterministic rules that decide,
//! without a person, whether a proposal applies, is refused, or goes to an
//! adversarial verifier — and the screen that keeps a model's output inside
//! the closed op vocabulary and the seqs it was actually shown (§5.5).
//!
//! | op | deterministic rule | else |
//! |---|---|---|
//! | file | parent + target exist; the item's provenance root is the parent's root or `~general` | `Refuse("provenance")` |
//! | create | a sibling title with Jaccard ≥ 0.8 → refuse the create, redirect its filings to that sibling; two such siblings → enqueue their merge | apply |
//! | promote | new parent exists (or root), no cycle, depth ≤ 4 | refuse |
//! | split | every link belongs to the node; each part ≥ 3 links | refuse |
//! | merge | refuse cross-root; apply if title Jaccard ≥ 0.8 or (same parent ∧ centroid cos ≥ 0.85 ∧ both ≥ 3 links) | verify (adversarial, conf ≥ 0.8) |
//! | collapse | `auto_collapse_safe` ∧ items ≥ 5 ∧ cold (not warm) ∧ no user note | `Refuse("not_cold")` |
//! | supersede | decision kinds only (staging screens); same `(ref_kind, ref_id)` → apply; different `ref_kind` → refuse | verify |
//!
//! The centroid clause is C1's: `SimilarityOracle` is the seam it fills;
//! until then the oracle answers "unknown" and the clause never fires.

use std::collections::{HashMap, HashSet};

use polis_core::coldness::{auto_collapse_safe, BranchStat, LakeEnvelope};
use polis_core::proposal::Proposal;
use polis_core::types::{ClassNode, ClassProposalRow, DECISION_KINDS};
use polis_store::runs::{PROPOSAL_BACKOFF_RUNS, PROPOSAL_TTL_ATTEMPTS};
use polis_store::PolisStore;

use crate::organize::{root_id_for_path, GENERAL_ROOT_ID};

/// Title similarity that counts as "the same class".
pub const TITLE_JACCARD_DUP: f64 = 0.8;
/// The deepest a promote may leave a node (roots at depth 0).
pub const MAX_DEPTH: usize = 4;
/// A split's smallest part; a merge's smallest partner for the centroid clause.
pub const MIN_PART_LINKS: usize = 3;
/// The centroid clause's cosine floor (C1's oracle).
pub const CENTROID_COS: f64 = 0.85;
/// The fewest items a branch needs before a collapse compresses anything.
pub const COLLAPSE_MIN_ITEMS: i64 = 5;

/// C1's seam: how alike two classes are by their members' centroids.
/// `None` = unknown (no centroids yet), and the clause that needs it does
/// not fire.
pub trait SimilarityOracle: Send + Sync {
    fn cosine(&self, _node_a: &str, _node_b: &str) -> Option<f64> {
        None
    }
}

/// No centroids: every centroid question is unknown.
pub struct NoOracle;
impl SimilarityOracle for NoOracle {}

/// What the adjudicator decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Apply now, in this run.
    Apply,
    /// Send to the adversarial verifier (one batched spawn per run).
    Verify,
    /// Journaled as refused, `class_curate action=refuse`, dropped.
    Refuse(String),
    /// A `create` whose twin already exists: refuse it, file under the twin,
    /// and — when two near-identical siblings already exist — queue their
    /// merge instead.
    Redirect { to_node: String, enqueue_merge: Option<Vec<String>> },
}

/// Word-set Jaccard over lowercased titles.
pub fn jaccard(a: &str, b: &str) -> f64 {
    let words = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect()
    };
    let (wa, wb) = (words(a), words(b));
    if wa.is_empty() && wb.is_empty() {
        return 1.0;
    }
    let inter = wa.intersection(&wb).count() as f64;
    let union = wa.union(&wb).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

/// The catalog as the adjudicator reads it: one snapshot per run.
pub struct Catalog {
    pub nodes: Vec<ClassNode>,
    by_id: HashMap<String, usize>,
}

impl Catalog {
    pub fn load(store: &PolisStore) -> rusqlite::Result<Self> {
        let nodes = store.list_class_nodes()?;
        let by_id = nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();
        Ok(Self { nodes, by_id })
    }

    pub fn get(&self, id: &str) -> Option<&ClassNode> {
        self.by_id.get(id).map(|&i| &self.nodes[i])
    }

    /// The root above a node (itself when it is a root); `None` for an id
    /// the catalog does not hold.
    pub fn root_of(&self, id: &str) -> Option<String> {
        let mut cur = self.get(id)?;
        let mut guard = 0;
        while let Some(p) = cur.parent_id.as_deref() {
            guard += 1;
            if guard > 64 {
                return None;
            }
            cur = self.get(p)?;
        }
        Some(cur.id.clone())
    }

    pub fn depth_of(&self, id: &str) -> usize {
        let mut d = 0;
        let mut cur = self.get(id);
        while let Some(n) = cur {
            match n.parent_id.as_deref() {
                Some(p) => {
                    d += 1;
                    cur = self.get(p);
                }
                None => break,
            }
            if d > 64 {
                break;
            }
        }
        d
    }

    /// The deepest leaf under `id`, relative to `id` (0 = a leaf).
    pub fn subtree_height(&self, id: &str) -> usize {
        let mut kids: HashMap<&str, Vec<&str>> = HashMap::new();
        for n in &self.nodes {
            if let Some(p) = &n.parent_id {
                kids.entry(p.as_str()).or_default().push(n.id.as_str());
            }
        }
        fn h(id: &str, kids: &HashMap<&str, Vec<&str>>, depth: usize) -> usize {
            if depth > 64 {
                return 0;
            }
            kids.get(id).map(|cs| 1 + cs.iter().map(|c| h(c, kids, depth + 1)).max().unwrap_or(0)).unwrap_or(0)
        }
        // h counts edges to the deepest descendant + 1 per level; a leaf is 0.
        h(id, &kids, 0).saturating_sub(0)
    }

    pub fn is_descendant(&self, node: &str, ancestor: &str) -> bool {
        let mut cur = self.get(node).and_then(|n| n.parent_id.as_deref());
        let mut guard = 0;
        while let Some(p) = cur {
            if p == ancestor {
                return true;
            }
            guard += 1;
            if guard > 64 {
                return false;
            }
            cur = self.get(p).and_then(|n| n.parent_id.as_deref());
        }
        false
    }

    pub fn siblings_under(&self, parent: &str) -> Vec<&ClassNode> {
        self.nodes.iter().filter(|n| n.parent_id.as_deref() == Some(parent) && n.kind == "node").collect()
    }
}

// ---------------------------------------------------------------------------
// Additive ops (adjudicated before staging)
// ---------------------------------------------------------------------------

/// `file`: parent + target exist; the item's provenance root is the parent's
/// root or `~general`. An item with no project path has no provenance to
/// violate and files anywhere.
pub fn adjudicate_file(catalog: &Catalog, store: &PolisStore, parent_id: &str, target_kind: &str, target_id: &str) -> Verdict {
    let Some(root) = catalog.root_of(parent_id) else {
        return Verdict::Refuse(format!("parent {parent_id} does not exist"));
    };
    let seq: Option<i64> = target_id.parse().ok();
    let exists = match (target_kind, seq) {
        (_, Some(seq)) => store.seq_exists(seq).unwrap_or(false),
        // Session / mission / revision ids are the host's rows: the store
        // cannot check them, and a filing of an unknown id is harmless.
        _ => true,
    };
    if !exists {
        return Verdict::Refuse(format!("target {target_kind}:{target_id} does not exist"));
    }
    if root == GENERAL_ROOT_ID {
        return Verdict::Apply;
    }
    if let Some(seq) = seq {
        if let Ok(Some(project)) = store.item_project_path(seq) {
            let item_root = root_id_for_path(&project);
            if item_root != root {
                return Verdict::Refuse("provenance".to_string());
            }
        }
    }
    Verdict::Apply
}

/// `create`: a sibling whose title is the same class (Jaccard ≥ 0.8) means
/// no new class — redirect to it; two such siblings already exist → their
/// merge is queued instead.
pub fn adjudicate_create(catalog: &Catalog, parent_id: &str, title: &str) -> Verdict {
    if catalog.get(parent_id).is_none() {
        return Verdict::Refuse(format!("parent {parent_id} does not exist"));
    }
    let twins: Vec<&ClassNode> = catalog
        .siblings_under(parent_id)
        .into_iter()
        .filter(|s| jaccard(&s.title, title) >= TITLE_JACCARD_DUP)
        .collect();
    match twins.len() {
        0 => Verdict::Apply,
        1 => Verdict::Redirect { to_node: twins[0].id.clone(), enqueue_merge: None },
        _ => Verdict::Redirect {
            to_node: twins[0].id.clone(),
            enqueue_merge: Some(twins.iter().map(|t| t.id.clone()).collect()),
        },
    }
}

// ---------------------------------------------------------------------------
// Structural ops (the work queue)
// ---------------------------------------------------------------------------

/// What a structural verdict needs beside the catalog.
pub struct Facts<'a> {
    pub store: &'a PolisStore,
    pub catalog: &'a Catalog,
    pub stats: &'a HashMap<String, BranchStat>,
    pub env: LakeEnvelope,
    /// Nodes protected in themselves (warm or noted) — `warmth::direct_protected`.
    pub protected: &'a HashSet<String>,
    pub oracle: &'a dyn SimilarityOracle,
}

pub fn adjudicate_structural(prop: &ClassProposalRow, f: &Facts<'_>) -> Verdict {
    match prop.op.as_str() {
        "promote" => {
            let Some(node) = prop.node_id.as_deref() else { return Verdict::Refuse("no node".into()) };
            if f.catalog.get(node).is_none() {
                return Verdict::Refuse(format!("node {node} does not exist"));
            }
            match prop.parent_id.as_deref() {
                None => {}
                Some(p) if p == node => return Verdict::Refuse("cycle: a node cannot parent itself".into()),
                Some(p) => {
                    if f.catalog.get(p).is_none() {
                        return Verdict::Refuse(format!("new parent {p} does not exist"));
                    }
                    if f.catalog.is_descendant(p, node) {
                        return Verdict::Refuse("cycle: the new parent is inside the node's own subtree".into());
                    }
                    let new_depth = f.catalog.depth_of(p) + 1 + f.catalog.subtree_height(node);
                    if new_depth > MAX_DEPTH {
                        return Verdict::Refuse(format!("depth {new_depth} exceeds {MAX_DEPTH}"));
                    }
                }
            }
            Verdict::Apply
        }
        "split" => {
            let Some(node) = prop.node_id.as_deref() else { return Verdict::Refuse("no node".into()) };
            let parts: Vec<polis_core::proposal::SplitPart> =
                prop.extra_json.as_deref().and_then(|e| serde_json::from_str(e).ok()).unwrap_or_default();
            if parts.len() < 2 {
                return Verdict::Refuse("a split needs at least two parts".into());
            }
            let own: HashSet<i64> = f.store.list_class_links_for_node(node).unwrap_or_default().iter().map(|l| l.id).collect();
            for part in &parts {
                if part.link_ids.len() < MIN_PART_LINKS {
                    return Verdict::Refuse(format!("part \"{}\" has {} link(s); each part needs {MIN_PART_LINKS}", part.title, part.link_ids.len()));
                }
                if let Some(bad) = part.link_ids.iter().find(|l| !own.contains(l)) {
                    return Verdict::Refuse(format!("link {bad} does not belong to {node}"));
                }
            }
            Verdict::Apply
        }
        "merge" => {
            let ids: Vec<String> = prop.extra_json.as_deref().and_then(|e| serde_json::from_str(e).ok()).unwrap_or_default();
            if ids.len() < 2 {
                return Verdict::Refuse("a merge needs at least two nodes".into());
            }
            let mut roots = HashSet::new();
            for id in &ids {
                match f.catalog.root_of(id) {
                    Some(r) => {
                        roots.insert(r);
                    }
                    None => return Verdict::Refuse(format!("node {id} does not exist")),
                }
            }
            if roots.len() > 1 {
                return Verdict::Refuse("cross-root merge".into());
            }
            let (a, b) = (f.catalog.get(&ids[0]).unwrap(), f.catalog.get(&ids[1]).unwrap());
            if jaccard(&a.title, &b.title) >= TITLE_JACCARD_DUP {
                return Verdict::Apply;
            }
            if a.parent_id == b.parent_id {
                let links = |id: &str| f.stats.get(id).map(|s| s.item_count).unwrap_or(0) as usize;
                if links(&a.id) >= MIN_PART_LINKS && links(&b.id) >= MIN_PART_LINKS {
                    if let Some(cos) = f.oracle.cosine(&a.id, &b.id) {
                        if cos >= CENTROID_COS {
                            return Verdict::Apply;
                        }
                    }
                }
            }
            Verdict::Verify
        }
        "collapse" => {
            let Some(node) = prop.node_id.as_deref() else { return Verdict::Refuse("no node".into()) };
            let Some(stat) = f.stats.get(node) else { return Verdict::Refuse(format!("node {node} does not exist")) };
            if stat.item_count < COLLAPSE_MIN_ITEMS {
                return Verdict::Refuse("not_cold: too few items to compress".into());
            }
            // `stat.pinned` is the rolled-up PROTECTED flag (warm or noted
            // anywhere in the branch) since B3 — `auto_collapse_safe` reads it.
            if f.protected.contains(node) || !auto_collapse_safe(stat, f.env) {
                return Verdict::Refuse("not_cold".into());
            }
            Verdict::Apply
        }
        "supersede" => {
            let pair = prop
                .extra_json
                .as_deref()
                .and_then(|e| serde_json::from_str::<serde_json::Value>(e).ok())
                .and_then(|v| Some((v.get("old_seq")?.as_i64()?, v.get("new_seq")?.as_i64()?)));
            let Some((old, new)) = pair else { return Verdict::Refuse("malformed supersede".into()) };
            let refs = (f.store.ledger_event_ref(old), f.store.ledger_event_ref(new));
            match refs {
                (Ok(Some(o)), Ok(Some(n))) => {
                    if !DECISION_KINDS.contains(&o.0.as_str()) || !DECISION_KINDS.contains(&n.0.as_str()) {
                        return Verdict::Refuse("only decision events supersede".into());
                    }
                    if o.1 == n.1 && o.2 == n.2 && o.1.is_some() {
                        Verdict::Apply
                    } else if o.1 != n.1 {
                        Verdict::Refuse("different subjects".into())
                    } else {
                        Verdict::Verify
                    }
                }
                _ => Verdict::Refuse("a seq does not exist".into()),
            }
        }
        other => Verdict::Refuse(format!("unknown op {other}")),
    }
}

// ---------------------------------------------------------------------------
// The queue policy
// ---------------------------------------------------------------------------

/// How many runs a proposal waits after its `attempts`-th failed verification.
pub fn backoff_runs(attempts: i64) -> i64 {
    let ix = (attempts.max(1) - 1).min(PROPOSAL_BACKOFF_RUNS.len() as i64 - 1) as usize;
    PROPOSAL_BACKOFF_RUNS[ix]
}

/// Whether a queued proposal has run out of attempts or lake-days.
pub fn expired(prop: &ClassProposalRow, lake_newest_ts: i64) -> Option<String> {
    if prop.attempts >= PROPOSAL_TTL_ATTEMPTS {
        return Some(format!("{} attempts", prop.attempts));
    }
    if let Some(exp) = prop.expires_lake_ts {
        if lake_newest_ts > exp {
            return Some("seven lake-days".to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// §5.5: the output screen
// ---------------------------------------------------------------------------

/// What one classifier prompt actually showed: the seqs (delta items), the
/// host ids (sessions, missions) those items carried, and the node ids of
/// the tree. Anything an op names outside this set is not knowledge the
/// model was given — it came from inside an item, or from nowhere.
#[derive(Debug, Default, Clone)]
pub struct Shown {
    pub seqs: HashSet<i64>,
    pub host_ids: HashSet<String>,
    pub nodes: HashSet<String>,
}

impl Shown {
    pub fn from_delta(tree: &[ClassNode], delta: &[polis_core::types::LakeItem]) -> Self {
        let mut s = Shown::default();
        for n in tree {
            s.nodes.insert(n.id.clone());
        }
        for it in delta {
            s.seqs.insert(it.seq);
            for id in [it.session_id.as_deref(), it.mission_id.as_deref(), it.thread_id.as_deref(), it.parent_session_id.as_deref()] {
                if let Some(id) = id.filter(|s| !s.is_empty()) {
                    s.host_ids.insert(id.to_string());
                }
            }
            if let Some(id) = it.ref_id.as_deref() {
                s.host_ids.insert(id.to_string());
            }
        }
        s
    }
}

/// Keep the ops that stay inside what was shown; return the rest with the
/// reason. The parser already enforces the closed op vocabulary (an unknown
/// op never becomes a `Proposal`); this is the seq / id half.
pub fn screen(proposals: Vec<Proposal>, shown: &Shown) -> (Vec<Proposal>, Vec<(Proposal, String)>) {
    let mut kept = Vec::new();
    let mut refused = Vec::new();
    let node_ok = |id: &str| shown.nodes.contains(id);
    for p in proposals {
        let verdict: Option<String> = match &p {
            Proposal::File { parent_id, target_id, .. } => {
                if !node_ok(parent_id) {
                    Some(format!("parent {parent_id} was not shown"))
                } else if let Ok(seq) = target_id.parse::<i64>() {
                    (!shown.seqs.contains(&seq)).then(|| format!("seq {seq} was not shown"))
                } else {
                    (!shown.host_ids.contains(target_id)).then(|| format!("id {target_id} was not shown"))
                }
            }
            Proposal::Create { parent_id, .. } => (!node_ok(parent_id)).then(|| format!("parent {parent_id} was not shown")),
            Proposal::Promote { node_id, new_parent_id, .. } => {
                if !node_ok(node_id) {
                    Some(format!("node {node_id} was not shown"))
                } else {
                    new_parent_id.as_deref().filter(|p| !node_ok(p)).map(|p| format!("parent {p} was not shown"))
                }
            }
            Proposal::Split { node_id, .. } => (!node_ok(node_id)).then(|| format!("node {node_id} was not shown")),
            Proposal::Merge { node_ids, .. } => node_ids.iter().find(|n| !node_ok(n)).map(|n| format!("node {n} was not shown")),
            Proposal::Collapse { node_id, .. } => (!node_ok(node_id)).then(|| format!("node {node_id} was not shown")),
            Proposal::Supersede { new_seq, .. } => {
                // The NEWER decision is the one the classifier just read in
                // the delta; the older may be anywhere in the record (staging
                // checks it is a decision).
                (!shown.seqs.contains(new_seq)).then(|| format!("seq {new_seq} was not shown"))
            }
        };
        match verdict {
            None => kept.push(p),
            Some(reason) => refused.push((p, reason)),
        }
    }
    (kept, refused)
}

/// The subject ids an op names, for the journal's `subject_ids`.
pub fn subjects_of(p: &Proposal) -> Vec<String> {
    use polis_store::runs::subject;
    match p {
        Proposal::File { parent_id, target_id, .. } => {
            let mut v = vec![subject::node(parent_id)];
            if let Ok(seq) = target_id.parse::<i64>() {
                v.push(subject::seq(seq));
            }
            v
        }
        Proposal::Create { parent_id, .. } => vec![subject::node(parent_id)],
        Proposal::Promote { node_id, .. } | Proposal::Split { node_id, .. } | Proposal::Collapse { node_id, .. } => vec![subject::node(node_id)],
        Proposal::Merge { node_ids, .. } => node_ids.iter().map(|n| subject::node(n)).collect(),
        Proposal::Supersede { old_seq, new_seq, .. } => vec![subject::seq(*old_seq), subject::seq(*new_seq)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, parent: Option<&str>, title: &str) -> ClassNode {
        ClassNode {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            kind: "node".into(),
            title: title.into(),
            summary: None,
            project_path: None,
            ip_name: None,
            status: "accepted".into(),
            pinned: false,
            curated_by: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn catalog(nodes: Vec<ClassNode>) -> Catalog {
        let by_id = nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect();
        Catalog { nodes, by_id }
    }

    #[test]
    fn jaccard_is_over_word_sets() {
        assert_eq!(jaccard("Loop Engineering", "loop engineering"), 1.0);
        assert!(jaccard("Auth: Clerk webhooks", "Clerk auth webhooks") >= 0.8);
        assert!(jaccard("Deploy", "Billing") < 0.1);
    }

    #[test]
    fn create_redirects_to_a_twin_and_queues_a_merge_of_two_twins() {
        let c = catalog(vec![node("root", None, "redline"), node("a", Some("root"), "Loop Engineering")]);
        assert_eq!(adjudicate_create(&c, "root", "Loop Engineering"), Verdict::Redirect { to_node: "a".into(), enqueue_merge: None });
        assert_eq!(adjudicate_create(&c, "root", "Billing"), Verdict::Apply);
        let c2 = catalog(vec![
            node("root", None, "redline"),
            node("a", Some("root"), "Loop Engineering"),
            node("b", Some("root"), "loop engineering"),
        ]);
        match adjudicate_create(&c2, "root", "Loop engineering") {
            Verdict::Redirect { enqueue_merge: Some(ids), .. } => assert_eq!(ids, vec!["a".to_string(), "b".to_string()]),
            v => panic!("{v:?}"),
        }
        assert!(matches!(adjudicate_create(&c, "ghost", "x"), Verdict::Refuse(_)));
    }

    #[test]
    fn promote_refuses_cycles_and_depth() {
        let c = catalog(vec![
            node("root", None, "r"),
            node("a", Some("root"), "a"),
            node("b", Some("a"), "b"),
            node("c", Some("b"), "c"),
            node("d", Some("c"), "d"),
            node("x", Some("root"), "x"),
        ]);
        let row = |node: &str, parent: Option<&str>| ClassProposalRow {
            id: 1,
            run_id: None,
            op: "promote".into(),
            node_id: Some(node.into()),
            parent_id: parent.map(String::from),
            title: None,
            summary: None,
            extra_json: None,
            rationale: None,
            status: "proposed".into(),
            created_at: 0,
            attempts: 0,
            next_after_run: None,
            expires_lake_ts: None,
        };
        let stats = HashMap::new();
        let protected = HashSet::new();
        let store = PolisStore::open_in_memory().unwrap();
        let f = Facts { store: &store, catalog: &c, stats: &stats, env: LakeEnvelope { oldest: 0, newest: 1 }, protected: &protected, oracle: &NoOracle };
        assert_eq!(adjudicate_structural(&row("b", Some("root")), &f), Verdict::Apply);
        assert!(matches!(adjudicate_structural(&row("a", Some("c")), &f), Verdict::Refuse(r) if r.contains("cycle")));
        // x has height 0; under d it would sit at depth 5.
        assert!(matches!(adjudicate_structural(&row("x", Some("d")), &f), Verdict::Refuse(r) if r.contains("depth")));
        assert_eq!(adjudicate_structural(&row("d", None), &f), Verdict::Apply, "promote to a root");
    }

    #[test]
    fn merge_applies_on_title_refuses_cross_root_else_verifies() {
        let c = catalog(vec![
            node("r1", None, "one"),
            node("r2", None, "two"),
            node("a", Some("r1"), "Clerk Auth"),
            node("b", Some("r1"), "clerk auth"),
            node("d", Some("r1"), "Deploys"),
            node("e", Some("r2"), "Deploys"),
        ]);
        let row = |ids: &[&str]| ClassProposalRow {
            id: 1,
            run_id: None,
            op: "merge".into(),
            node_id: Some(ids[0].into()),
            parent_id: None,
            title: None,
            summary: None,
            extra_json: Some(serde_json::to_string(ids).unwrap()),
            rationale: None,
            status: "proposed".into(),
            created_at: 0,
            attempts: 0,
            next_after_run: None,
            expires_lake_ts: None,
        };
        let stats = HashMap::new();
        let protected = HashSet::new();
        let store = PolisStore::open_in_memory().unwrap();
        let f = Facts { store: &store, catalog: &c, stats: &stats, env: LakeEnvelope { oldest: 0, newest: 1 }, protected: &protected, oracle: &NoOracle };
        assert_eq!(adjudicate_structural(&row(&["a", "b"]), &f), Verdict::Apply);
        assert!(matches!(adjudicate_structural(&row(&["d", "e"]), &f), Verdict::Refuse(r) if r == "cross-root merge"));
        assert_eq!(adjudicate_structural(&row(&["a", "d"]), &f), Verdict::Verify);
    }

    #[test]
    fn the_screen_refuses_what_was_not_shown() {
        let tree = vec![node("root", None, "r")];
        let delta = vec![polis_core::types::LakeItem {
            seq: 7,
            ts: 0,
            kind: "prompt".into(),
            surface: None,
            origin: None,
            role: None,
            session_id: Some("sess-1".into()),
            mission_id: None,
            project_path: None,
            ref_kind: None,
            ref_id: None,
            body: None,
            thread_kind: None,
            thread_id: None,
            parent_session_id: None,
            model: None,
        }];
        let shown = Shown::from_delta(&tree, &delta);
        let props = vec![
            Proposal::File { parent_id: "root".into(), sub_class: None, target_kind: "prompt".into(), target_id: "7".into(), note: None, rationale: None },
            Proposal::File { parent_id: "root".into(), sub_class: None, target_kind: "prompt".into(), target_id: "12".into(), note: None, rationale: None },
            Proposal::File { parent_id: "root".into(), sub_class: None, target_kind: "session".into(), target_id: "sess-1".into(), note: None, rationale: None },
            Proposal::Merge { node_ids: vec!["root".into(), "ghost".into()], title: None, parent_id: None, rationale: None },
            Proposal::Supersede { old_seq: 3, new_seq: 12, rationale: None },
            Proposal::Collapse { node_id: "ghost".into(), summary: "x".into(), cite_seqs: vec![], rationale: None },
        ];
        let (kept, refused) = screen(props, &shown);
        assert_eq!(kept.len(), 2);
        assert_eq!(refused.len(), 4);
        assert!(refused.iter().all(|(_, r)| r.contains("was not shown")));
    }

    #[test]
    fn the_queue_policy_is_one_two_four_then_expiry() {
        assert_eq!(backoff_runs(1), 1);
        assert_eq!(backoff_runs(2), 2);
        assert_eq!(backoff_runs(3), 4);
        assert_eq!(backoff_runs(9), 4);
        let row = |attempts: i64, exp: Option<i64>| ClassProposalRow {
            id: 1,
            run_id: None,
            op: "merge".into(),
            node_id: None,
            parent_id: None,
            title: None,
            summary: None,
            extra_json: None,
            rationale: None,
            status: "proposed".into(),
            created_at: 0,
            attempts,
            next_after_run: None,
            expires_lake_ts: exp,
        };
        assert!(expired(&row(3, None), 0).is_some());
        assert!(expired(&row(1, Some(100)), 101).is_some());
        assert!(expired(&row(1, Some(100)), 100).is_none());
    }
}
