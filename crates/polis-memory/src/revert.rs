// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Reversibility (Session B2, plan §5.2): the facade's side of
//! [`polis_store::PolisStore::revert_run`] — the run views the timeline
//! reads and the one undo — and the property test that is this session's
//! gate: random op sequences, applied as runs through the real staging and
//! apply paths, reverted newest-first, must leave the catalog byte-equal to
//! the snapshot taken before each run, with the chain and a full bundle
//! verifying throughout.
//!
//! The mechanics (marks, the journal, the per-op inverse, the horizon) live
//! in `polis-store`'s `runs.rs`; docs/ledger.md is the narrative.

use polis_core::types::{RevertReceipt, RunView};
use polis_core::MemoryError;
use polis_store::runs::RevertOutcome;

use crate::Polis;

/// The most runs one `list_runs` page returns.
pub const RUNS_PAGE_MAX: i64 = 500;

/// One run with its journal, or `None` for an unknown id.
pub fn run_view(polis: &Polis<'_>, id: i64) -> Result<Option<RunView>, MemoryError> {
    let store_err = |e: rusqlite::Error| MemoryError::Store(e.to_string());
    let Some(run) = polis.store.get_class_run(id).map_err(store_err)? else {
        return Ok(None);
    };
    let ops = polis.store.list_run_ops(id).map_err(store_err)?;
    Ok(Some(RunView { run, ops }))
}

/// Undo one run. A refusal is `Rejected` with the reason the caller acts on
/// ("revert run #N first", the horizon, an op with no image).
pub fn revert_run(polis: &Polis<'_>, id: i64) -> Result<RevertReceipt, MemoryError> {
    match polis
        .store
        .revert_run(id, polis.store.author())
        .map_err(|e| MemoryError::Store(e.to_string()))?
    {
        RevertOutcome::Reverted(receipt) => Ok(receipt),
        RevertOutcome::Rejected(reason) => Err(MemoryError::Rejected(reason)),
    }
}

#[cfg(test)]
mod property {
    //! The gate. Excluded from the equality, and why:
    //! - retired rows (`retired_by_run IS NOT NULL`): a revert MARKS what a
    //!   run created (a created node, a split's parts, a collapse's digest and
    //!   its citation links, an observation) rather than deleting it — the
    //!   marks are what `vacuum_retired` later removes;
    //! - `class_nodes.updated_at`: promote / merge / re-parenting stamp it
    //!   and the revert restores the parent and title, not the clock;
    //! - the journal itself (`class_runs`, `class_run_ops`), the chain
    //!   (append-only: every run's events and the `gardener_revert` stay),
    //!   the derived FTS / embedding tables, and autoincrement counters (a
    //!   reverted `file`'s link is deleted; the next id is higher).
    //!
    //! Everything else — every live node column but `updated_at`, every live
    //! link and observation, the `supersessions` rows, every prompt's body /
    //! gist / compaction marks, the archive — must come back exactly.
    use super::*;
    use crate::corpus::{seed_corpus, CorpusSpec, Rng};
    use crate::gardener::GIST_SOURCE_DETERMINISTIC;
    use polis_core::bundle::{verify_bundle, BundleScope};
    use polis_core::host::NoHost;
    use polis_core::proposal::{Proposal, SplitPart};
    use polis_core::types::ClassRunFinish;
    use polis_llm::NoopSink;
    use polis_store::runs::{MODE_ORGANIZE, REVERT_HORIZON_KEY};
    use polis_store::PolisStore;
    use std::sync::Arc;

    const ACTOR: &str = "classifier";

    fn rows(store: &PolisStore, sql: &str) -> Vec<String> {
        let conn = store.conn();
        let mut stmt = conn.prepare(sql).unwrap();
        let n = stmt.column_count();
        let out = stmt
            .query_map([], |r| {
                let mut cells = Vec::with_capacity(n);
                for i in 0..n {
                    cells.push(format!("{:?}", r.get_ref(i).unwrap().to_owned()));
                }
                Ok(cells.join("|"))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        out
    }

    /// The live catalog + lake state the property compares.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Snapshot {
        nodes: Vec<String>,
        links: Vec<String>,
        observations: Vec<String>,
        supersessions: Vec<String>,
        prompts: Vec<String>,
        archive: Vec<String>,
    }

    fn snapshot(store: &PolisStore) -> Snapshot {
        Snapshot {
            nodes: rows(
                store,
                "SELECT id, parent_id, kind, title, summary, project_path, ip_name, status, pinned, curated_by, created_at
                 FROM class_nodes WHERE retired_by_run IS NULL ORDER BY id",
            ),
            links: rows(
                store,
                "SELECT id, node_id, target_kind, target_id, note, status, created_at
                 FROM class_links WHERE retired_by_run IS NULL ORDER BY id",
            ),
            observations: rows(
                store,
                "SELECT id, node_id, summary, cite_seqs, created_seq, pinned, dismissed, created_at
                 FROM class_observations WHERE retired_by_run IS NULL ORDER BY id",
            ),
            supersessions: rows(store, "SELECT old_seq, new_seq, event_seq FROM supersessions ORDER BY old_seq"),
            prompts: rows(store, "SELECT id, body, gist, compacted_at, original_bytes, gist_source FROM prompts ORDER BY id"),
            archive: rows(store, "SELECT prompt_id, body_hash FROM prompt_archive ORDER BY prompt_id"),
        }
    }

    fn seqs(store: &PolisStore, sql: &str) -> Vec<i64> {
        let conn = store.conn();
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get::<_, i64>(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }

    struct World {
        store: Arc<PolisStore>,
        rng: Rng,
        prompt_seqs: Vec<i64>,
        decision_seqs: Vec<i64>,
        counter: usize,
    }

    impl World {
        fn new(seed: u64) -> Self {
            let store = Arc::new(PolisStore::open_in_memory().unwrap());
            seed_corpus(&store, &CorpusSpec::new(240).with_seed(seed)).unwrap();
            // Flush anything the corpus left `proposed`: the first run's
            // accept-all would otherwise flip pre-existing rows, which no
            // revert of that run could undo.
            store.accept_all_pending(ACTOR).unwrap();
            let prompt_seqs = seqs(&store, "SELECT seq FROM ledger_events WHERE kind = 'prompt' ORDER BY seq");
            let decision_seqs = seqs(
                &store,
                "SELECT seq FROM ledger_events WHERE kind IN ('approval', 'resolution', 'review_verdict') ORDER BY seq",
            );
            Self { store, rng: Rng::new(seed ^ 0xb2), prompt_seqs, decision_seqs, counter: 0 }
        }

        fn classes(&self) -> Vec<polis_core::types::ClassNode> {
            self.store.list_class_nodes().unwrap().into_iter().filter(|n| n.parent_id.is_some() && n.kind == "node").collect()
        }

        fn roots(&self) -> Vec<String> {
            self.store.list_class_nodes().unwrap().into_iter().filter(|n| n.parent_id.is_none()).map(|n| n.id).collect()
        }

        /// One run of 2–5 random ops through the real stage → accept → apply
        /// path, plus the compaction / observation primitives. Returns the
        /// run id.
        fn run(&mut self) -> i64 {
            let run_id = self.store.insert_class_run_with(MODE_ORGANIZE, None, None).unwrap();
            let n_ops = 2 + self.rng.below(4);
            let mut applied = 0i64;
            for _ in 0..n_ops {
                self.counter += 1;
                let classes = self.classes();
                if classes.is_empty() {
                    break;
                }
                match self.rng.below(9) {
                    0 | 1 => {
                        // file
                        let node = self.rng.pick(&classes).id.clone();
                        let seq = *self.rng.pick(&self.prompt_seqs);
                        let p = Proposal::File {
                            parent_id: node,
                            sub_class: None,
                            target_kind: "prompt".into(),
                            target_id: seq.to_string(),
                            note: None,
                            rationale: None,
                        };
                        self.store.stage_proposal(Some(run_id), &p).unwrap();
                    }
                    2 => {
                        // create (under a class or a root; sometimes with a
                        // sub-class filing, which creates + files at once)
                        let parent = if self.rng.chance(0.5) { self.rng.pick(&classes).id.clone() } else { self.rng.pick(&self.roots()).clone() };
                        if self.rng.chance(0.5) {
                            let p = Proposal::Create { parent_id: parent, title: format!("topic {}", self.counter), rationale: None };
                            self.store.stage_proposal(Some(run_id), &p).unwrap();
                        } else {
                            let seq = *self.rng.pick(&self.prompt_seqs);
                            let p = Proposal::File {
                                parent_id: parent,
                                sub_class: Some(format!("sub {}", self.counter)),
                                target_kind: "prompt".into(),
                                target_id: seq.to_string(),
                                note: None,
                                rationale: None,
                            };
                            self.store.stage_proposal(Some(run_id), &p).unwrap();
                        }
                    }
                    3 => {
                        // promote to a root (never under its own subtree)
                        let node = self.rng.pick(&classes).id.clone();
                        let root = self.rng.pick(&self.roots()).clone();
                        let p = Proposal::Promote { node_id: node, new_parent_id: Some(root), rationale: None };
                        self.store.stage_proposal(Some(run_id), &p).unwrap();
                    }
                    4 => {
                        // split a node with >= 2 links into two parts
                        let candidates: Vec<_> = classes
                            .iter()
                            .filter_map(|c| {
                                let links = self.store.list_class_links_for_node(&c.id).unwrap();
                                (links.len() >= 2).then(|| (c.id.clone(), links.iter().map(|l| l.id).collect::<Vec<_>>()))
                            })
                            .collect();
                        if let Some((node, links)) = candidates.get(self.rng.below(candidates.len().max(1))).cloned() {
                            let (a, b) = links.split_at(links.len() / 2);
                            let p = Proposal::Split {
                                node_id: node,
                                into: vec![
                                    SplitPart { title: format!("part a {}", self.counter), link_ids: a.to_vec() },
                                    SplitPart { title: format!("part b {}", self.counter), link_ids: b.to_vec() },
                                ],
                                rationale: None,
                            };
                            self.store.stage_proposal(Some(run_id), &p).unwrap();
                        }
                    }
                    5 => {
                        // merge two siblings
                        let a = self.rng.pick(&classes).clone();
                        let siblings: Vec<_> = classes.iter().filter(|c| c.parent_id == a.parent_id && c.id != a.id).cloned().collect();
                        if !siblings.is_empty() {
                            let b = self.rng.pick(&siblings).clone();
                            let p = Proposal::Merge {
                                node_ids: vec![a.id.clone(), b.id.clone()],
                                title: Some(format!("merged {}", self.counter)),
                                parent_id: None,
                                rationale: None,
                            };
                            self.store.stage_proposal(Some(run_id), &p).unwrap();
                        }
                    }
                    6 => {
                        // collapse a class into a digest citing a few seqs
                        let node = self.rng.pick(&classes).id.clone();
                        let cites: Vec<i64> = (0..3).map(|_| *self.rng.pick(&self.prompt_seqs)).collect();
                        let p = Proposal::Collapse { node_id: node, summary: format!("digest {}", self.counter), cite_seqs: cites, rationale: None };
                        self.store.stage_proposal(Some(run_id), &p).unwrap();
                    }
                    7 => {
                        // compact a warm prompt (deterministic gist)
                        let warm = seqs(&self.store, "SELECT id FROM prompts WHERE gist IS NULL AND LENGTH(body) > 40 ORDER BY id");
                        if !warm.is_empty() {
                            let id = *self.rng.pick(&warm);
                            let done = self
                                .store
                                .compact_prompt_body_in_run(run_id, id, &format!("gist {}", self.counter), "cold", GIST_SOURCE_DETERMINISTIC, "keeper")
                                .unwrap();
                            if done.is_some() {
                                applied += 1;
                            }
                        }
                    }
                    _ => {
                        // observe
                        let node = self.rng.pick(&classes).id.clone();
                        let seq = *self.rng.pick(&self.prompt_seqs);
                        let done = self
                            .store
                            .insert_class_observation_in_run(run_id, &node, &format!("pattern {}", self.counter), &[seq], "keeper")
                            .unwrap();
                        if done.is_some() {
                            applied += 1;
                        }
                    }
                }
            }
            // A supersession now and then (staged like the classifier's,
            // applied like the verifier's).
            if self.decision_seqs.len() >= 2 && self.rng.chance(0.5) {
                let i = self.rng.below(self.decision_seqs.len() - 1);
                let j = i + 1 + self.rng.below(self.decision_seqs.len() - i - 1);
                let p = Proposal::Supersede { old_seq: self.decision_seqs[i], new_seq: self.decision_seqs[j], rationale: Some("newer".into()) };
                self.store.stage_proposal(Some(run_id), &p).unwrap();
            }
            // The organize path: accept the additive rows, apply the
            // structural queue under this run.
            self.store.accept_all_pending(ACTOR).unwrap();
            for prop in self.store.list_class_proposals().unwrap() {
                if self.store.apply_class_proposal_in_run(prop.id, ACTOR, Some(run_id)).unwrap().is_some() {
                    applied += 1;
                }
            }
            self.store
                .finish_class_run_with(
                    run_id,
                    &ClassRunFinish { status: "done".into(), summary: "property run".into(), ops: Some(applied), outcome: Some("done".into()), ..Default::default() },
                )
                .unwrap();
            run_id
        }
    }

    fn polis(store: &PolisStore) -> Polis<'_> {
        Polis::new(store, None, &NoHost, &NoopSink)
    }

    /// Random op sequence → apply as runs → revert newest-first ≡ each
    /// run's pre-snapshot; chain green after every step; a full bundle with
    /// the new kinds verifies. Several seeds.
    #[test]
    fn apply_then_revert_is_the_identity() {
        for seed in [1u64, 2, 3, 4] {
            let mut w = World::new(seed);
            let mut snaps = Vec::new();
            let mut runs = Vec::new();
            let mut journaled = 0usize;
            for _ in 0..6 {
                snaps.push(snapshot(&w.store));
                let id = w.run();
                runs.push(id);
                journaled += w.store.list_run_ops(id).unwrap().len();
                assert!(w.store.verify_ledger_chain().unwrap().ok, "seed {seed}: chain after run {id}");
            }
            assert!(journaled >= 12, "seed {seed}: the runs journaled only {journaled} ops — the generator is too quiet to prove anything");
            let after_all = snapshot(&w.store);
            assert_ne!(after_all, snaps[0], "seed {seed}: the runs changed nothing");
            for (k, &id) in runs.iter().enumerate().rev() {
                let p = polis(&w.store);
                let receipt = revert_run(&p, id).unwrap_or_else(|e| panic!("seed {seed}: revert of run {id} refused: {e:?}"));
                assert_eq!(receipt.run_id, id);
                assert!(receipt.reverted_ops >= 1, "seed {seed}: run {id} reverted nothing");
                let now = snapshot(&w.store);
                assert_eq!(now, snaps[k], "seed {seed}: after reverting run {id} (index {k}) the catalog differs from its pre-snapshot");
                assert!(w.store.verify_ledger_chain().unwrap().ok, "seed {seed}: chain after reverting {id}");
                // The run and every op say so.
                let view = run_view(&p, id).unwrap().unwrap();
                assert_eq!(view.run.outcome.as_deref(), Some("reverted"));
                assert!(view.ops.iter().all(|o| o.outcome == "reverted" && o.reverted_by_run == Some(receipt.revert_run_id)));
                // Twice is a refusal, not a fault.
                assert!(matches!(revert_run(&p, id), Err(MemoryError::Rejected(_))));
            }
            // A full export carries the revert events and verifies.
            let p = polis(&w.store);
            let bundle = crate::bundle::build_bundle(&p, &BundleScope::Full).unwrap();
            let kinds: std::collections::BTreeSet<&str> = bundle.events.iter().map(|e| e.kind.as_str()).collect();
            assert!(kinds.contains("gardener_revert"), "seed {seed}: {kinds:?}");
            let v = verify_bundle(&bundle);
            assert!(v.ok && v.full_chain, "seed {seed}: {v:?}");
        }
    }

    /// A later run that touched the same subjects blocks the earlier revert
    /// by name; reverting newest-first clears the way.
    #[test]
    fn a_later_overlapping_run_must_be_reverted_first() {
        let w = World::new(11);
        let classes = w.classes();
        let node = classes[0].id.clone();
        let seq_a = w.prompt_seqs[0];
        let seq_b = w.prompt_seqs[1];
        let file = |run: i64, seq: i64| {
            w.store
                .stage_proposal(
                    Some(run),
                    &Proposal::File { parent_id: node.clone(), sub_class: None, target_kind: "prompt".into(), target_id: seq.to_string(), note: None, rationale: None },
                )
                .unwrap();
            w.store.accept_all_pending(ACTOR).unwrap();
        };
        let a = w.store.insert_class_run_with(MODE_ORGANIZE, None, None).unwrap();
        file(a, seq_a);
        let b = w.store.insert_class_run_with(MODE_ORGANIZE, None, None).unwrap();
        file(b, seq_b);
        let p = polis(&w.store);
        match revert_run(&p, a) {
            Err(MemoryError::Rejected(reason)) => assert!(reason.contains(&format!("revert run #{b} first")), "{reason}"),
            other => panic!("expected a refusal naming run {b}, got {other:?}"),
        }
        revert_run(&p, b).unwrap();
        revert_run(&p, a).unwrap();
        assert!(w.store.list_class_links_for_node(&node).unwrap().iter().all(|l| l.target_id != seq_a.to_string() && l.target_id != seq_b.to_string()));
    }

    /// Past the vacuum horizon a revert is refused, and says so.
    #[test]
    fn vacuum_moves_the_horizon_and_reverts_behind_it_are_refused() {
        let mut w = World::new(21);
        let first = w.run();
        for _ in 0..3 {
            w.run();
        }
        let report = w.store.vacuum_retired(1).unwrap();
        assert!(report.horizon_run >= first, "{report:?}");
        assert_eq!(w.store.revert_horizon().unwrap(), Some(report.horizon_run));
        let p = polis(&w.store);
        match revert_run(&p, first) {
            Err(MemoryError::Rejected(reason)) => assert!(reason.contains("horizon"), "{reason}"),
            other => panic!("expected a horizon refusal, got {other:?}"),
        }
        // The journal rows behind the horizon lost their images, kept their record.
        let ops = w.store.list_run_ops(first).unwrap();
        assert!(!ops.is_empty());
        let images: i64 = w
            .store
            .conn()
            .query_row("SELECT COUNT(*) FROM class_run_ops WHERE run_id <= ?1 AND pre_image IS NOT NULL", [report.horizon_run], |r| r.get(0))
            .unwrap();
        assert_eq!(images, 0);
        assert!(w.store.conn().query_row("SELECT value FROM polis_meta WHERE key = ?1", [REVERT_HORIZON_KEY], |r| r.get::<_, String>(0)).is_ok());
    }

    /// The store's readers never show a retired row: after a collapse the
    /// subtree is gone from every read, and back after the revert.
    #[test]
    fn readers_hide_retired_rows_and_a_revert_brings_them_back() {
        let w = World::new(31);
        let classes = w.classes();
        let victim = classes.iter().find(|c| !w.store.list_class_links_for_node(&c.id).unwrap().is_empty()).cloned().unwrap();
        let links_before = w.store.list_class_links_for_node(&victim.id).unwrap().len();
        let run = w.store.insert_class_run_with(MODE_ORGANIZE, None, None).unwrap();
        w.store
            .stage_proposal(
                Some(run),
                &Proposal::Collapse { node_id: victim.id.clone(), summary: "cold".into(), cite_seqs: vec![w.prompt_seqs[0]], rationale: None },
            )
            .unwrap();
        for prop in w.store.list_class_proposals().unwrap() {
            w.store.apply_class_proposal_in_run(prop.id, ACTOR, Some(run)).unwrap();
        }
        assert!(w.store.get_class_node(&victim.id).unwrap().is_none(), "a retired node is invisible");
        assert!(w.store.list_class_nodes().unwrap().iter().all(|n| n.id != victim.id));
        assert!(w.store.list_class_links_for_node(&victim.id).unwrap().is_empty());
        let p = polis(&w.store);
        revert_run(&p, run).unwrap();
        assert!(w.store.get_class_node(&victim.id).unwrap().is_some());
        assert_eq!(w.store.list_class_links_for_node(&victim.id).unwrap().len(), links_before);
        // The digest the collapse made is retired, not deleted, and hidden.
        assert!(w.store.list_class_nodes().unwrap().iter().all(|n| n.kind != "digest" || n.title != "cold"));
    }
}
