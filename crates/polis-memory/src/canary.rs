// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The canary: a frozen set of retrieval probes, DB-only, evaluated before
//! and after a gardener run so a run that made the memory less reachable is
//! caught by arithmetic rather than by a person (plan §5.3).
//!
//! Five subjects, every one a fact the lake already holds, so the set needs
//! no labels and no model:
//!
//! | subject | probe | hit |
//! |---|---|---|
//! | Decision | the class a filed decision sits under (its title; the host's evidence text when it has any) | the resolved node's links carry the decision seq |
//! | Supersession | the class linking the superseding decision | the new seq is in the pack unsuperseded; the old one, if present, carries `supersededBy` |
//! | Note | six words of a user note | the note is in the pack's notes |
//! | PromptSpan | an 8–12-token span of a recent user prompt, seeded by run id | the prompt's seq is a prompt hit (or absorbed into one as a duplicate) |
//! | ClassReach | a class title | the pack resolves that class, with links |
//!
//! ~200 `build_answer_pack` calls at `limit = 8`; the set is frozen at run
//! start (`freeze`) so before/after compare the same probes. What it
//! measures is reachability of what Polis recorded — not human relevance.
//! Today an UNFILED decision has no text of its own in the store and is
//! unreachable by design (the C-program's decision arm changes that); the
//! Decision subject therefore covers filed decisions, and the report says how
//! many recent decisions were skipped for having no class.
//!
//! Thresholds (calibrated in B1, docs/bench.md "Canary"): zero tolerance on
//! Supersession and Note regressions; aggregate regression when
//! `recall_after < recall_before − max(0.05, 3/N)`.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::corpus::Rng;
use crate::retrieval::build_answer_pack;
use crate::Polis;
mod policy;

#[derive(Debug, Clone)]
pub struct CanaryConfig {
    /// Newest filed decisions to probe.
    pub decisions: usize,
    /// Recent user prompts to span.
    pub spans: usize,
    /// Classes to reach (a seeded sample of the nodes that hold links).
    pub classes: usize,
    pub notes: usize,
    pub supersessions: usize,
    /// `build_answer_pack`'s limit.
    pub limit: i64,
    /// Span length range (tokens), inclusive.
    pub span_tokens: (usize, usize),
    /// Aggregate tolerance floor (`max(floor, 3/N)`).
    pub aggregate_floor: f64,
}

impl Default for CanaryConfig {
    fn default() -> Self {
        Self {
            decisions: 100,
            spans: 50,
            classes: 50,
            notes: 50,
            supersessions: 50,
            limit: 8,
            span_tokens: (8, 12),
            aggregate_floor: 0.05,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    Decision,
    Supersession,
    Note,
    PromptSpan,
    ClassReach,
    /// Not part of the canary (no gardener op touches browse titles); the
    /// eval adds it for the reachability picture.
    BrowseTitle,
    /// Source role/scope, budget and deliberate empty-scope invariants.
    EvidencePolicy,
    /// Cited assertions at independently frozen valid/known boundaries.
    TemporalClaim,
}

impl Subject {
    pub fn as_str(self) -> &'static str {
        match self {
            Subject::Decision => "decision",
            Subject::Supersession => "supersession",
            Subject::Note => "note",
            Subject::PromptSpan => "prompt_span",
            Subject::ClassReach => "class_reach",
            Subject::BrowseTitle => "browse_title",
            Subject::EvidencePolicy => "evidence_policy",
            Subject::TemporalClaim => "temporal_claim",
        }
    }

    /// Zero tolerance: any drop is a regression.
    pub fn zero_tolerance(self) -> bool {
        matches!(self, Subject::Supersession | Subject::Note | Subject::EvidencePolicy | Subject::TemporalClaim)
    }
}

/// What a probe expects to find.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Gold {
    /// A prompt hit carrying this seq (or absorbing it as a duplicate).
    PromptSeq { seq: i64 },
    /// The resolved node is `node_id` and its links carry `seq`.
    NodeLink { node_id: String, seq: i64 },
    /// `new` present and unsuperseded; `old` absent or marked superseded.
    Supersession { old: i64, new: i64 },
    Note { id: i64 },
    /// The resolved node is `node_id`, with at least one link.
    Node { node_id: String },
    /// The PAGE came back: a browse hit with this seq, or the same URL (the
    /// pack keeps one hit per page, so a revisit's seq is absorbed by an
    /// earlier view of the same page).
    Browse { seq: i64, url: String },
    ClaimAt { id: String, scope: polis_core::api::Scope, roles: Vec<String>, valid_at: i64, known_at: i64, expected: bool },
    EvidencePolicy { scope: polis_core::api::Scope, roles: Vec<String>, max_bytes: usize, expected_seq: Option<i64>, expect_empty: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub subject: Subject,
    pub query: String,
    pub gold: Gold,
}

/// The frozen set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanarySet {
    pub run_id: u64,
    pub head_seq: i64,
    pub probes: Vec<Probe>,
    /// Recent decisions that had no class link and were skipped.
    pub unfiled_decisions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub subject: Subject,
    pub hit: bool,
    /// 1-based rank inside the relevant ranked list (prompt hits, browse
    /// hits, node links) when the subject has one.
    pub rank: Option<usize>,
    pub ms: u32,
    pub pack_bytes: usize,
    /// Arms that found the gold prompt hit (empty when the hit came from the
    /// lexical arm alone — fusion only annotates when the semantic arm ran).
    pub arms: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubjectScore {
    pub subject: Subject,
    pub n: usize,
    pub hits: usize,
    pub recall: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryReport {
    pub run_id: u64,
    pub head_seq: i64,
    pub n: usize,
    pub hits: usize,
    pub recall: f64,
    pub subjects: Vec<SubjectScore>,
    pub unfiled_decisions: usize,
    pub packs: usize,
    pub elapsed_ms: u64,
    pub p50_ms: u32,
    pub p95_ms: u32,
}

fn tokens(s: &str) -> Vec<&str> {
    s.split_whitespace().filter(|t| t.chars().any(char::is_alphanumeric)).collect()
}

/// Freeze the set at the current head. Selection is seeded by `run_id`
/// (span offsets, the class sample), so the same run id freezes the same set.
pub fn freeze(polis: &Polis<'_>, run_id: u64, cfg: &CanaryConfig) -> CanarySet {
    let db = polis.store;
    let mut rng = Rng::new(run_id ^ 0xCA9A_0000_0000_0001);
    let mut probes = Vec::new();
    let head_seq = db.max_ledger_seq().unwrap_or(0);

    // Decision: the newest filed decisions, one probe each; unfiled ones
    // counted, not probed. The host's evidence text wins when it has any.
    let mut unfiled = 0usize;
    let filed = db.recent_decision_links(cfg.decisions as i64).unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for (node_id, title, seq) in filed {
        if !seen.insert(seq) {
            continue;
        }
        let evidence = polis.host.decision_evidence(seq).filter(|t| tokens(t).len() >= 3);
        let query = match evidence {
            Some(t) => tokens(&t).into_iter().take(10).collect::<Vec<_>>().join(" "),
            None => title.clone(),
        };
        probes.push(Probe { subject: Subject::Decision, query, gold: Gold::NodeLink { node_id, seq } });
    }
    // Count the recent decisions that no class links (the C5 gap).
    if let Ok(items) = db.query_ledger_events(&polis_core::types::LedgerFilters {
        limit: Some(cfg.decisions as i64),
        ..Default::default()
    }) {
        for it in items.iter().filter(|it| polis_core::types::DECISION_KINDS.contains(&it.event.kind.as_str())) {
            if db.nodes_linking_seq(it.event.seq).map(|v| v.is_empty()).unwrap_or(true) {
                unfiled += 1;
            }
        }
    }

    // Supersession: query by the class linking the NEW decision (else the old).
    for (old, new, _event) in db.list_supersessions(cfg.supersessions as i64).unwrap_or_default() {
        let via = db.nodes_linking_seq(new).unwrap_or_default().into_iter().next()
            .or_else(|| db.nodes_linking_seq(old).unwrap_or_default().into_iter().next());
        if let Some((_, title)) = via {
            probes.push(Probe { subject: Subject::Supersession, query: title, gold: Gold::Supersession { old, new } });
        }
    }

    // Note: six words of the note.
    for note in db.list_user_notes(false, cfg.notes as i64).unwrap_or_default() {
        let words: Vec<&str> = tokens(&note.text).into_iter().take(6).collect();
        if words.len() >= 2 {
            probes.push(Probe { subject: Subject::Note, query: words.join(" "), gold: Gold::Note { id: note.id } });
        }
    }

    // PromptSpan: recent user prompts with enough tokens; the span's length
    // and offset come from the run id.
    let (lo, hi) = cfg.span_tokens;
    let candidates = db.recent_user_prompts((cfg.spans * 3) as i64, (lo * 4) as i64).unwrap_or_default();
    let mut spans = 0usize;
    for (seq, _id, body) in candidates {
        if spans >= cfg.spans {
            break;
        }
        let toks = tokens(&body);
        if toks.len() < lo {
            continue;
        }
        let len = lo + rng.below(hi - lo + 1);
        let len = len.min(toks.len());
        let start = rng.below(toks.len() - len + 1);
        let query = toks[start..start + len].join(" ");
        probes.push(Probe { subject: Subject::PromptSpan, query, gold: Gold::PromptSeq { seq } });
        spans += 1;
    }

    // ClassReach: a seeded sample of the nodes that hold links.
    let mut nodes = db.nodes_with_links().unwrap_or_default();
    // Fisher–Yates with the seeded generator, then take the sample.
    for i in (1..nodes.len()).rev() {
        let j = rng.below(i + 1);
        nodes.swap(i, j);
    }
    for (node, _links) in nodes.into_iter().take(cfg.classes) {
        probes.push(Probe { subject: Subject::ClassReach, query: node.title.clone(), gold: Gold::Node { node_id: node.id } });
    }

    probes.extend(policy::freeze(polis, run_id, head_seq));
    CanarySet { run_id, head_seq, probes, unfiled_decisions: unfiled }
}

/// Browse-title reachability probes (the eval's extra subject).
pub fn browse_title_probes(polis: &Polis<'_>, n: usize) -> Vec<Probe> {
    polis
        .store
        .recent_browse_titles(n as i64)
        .unwrap_or_default()
        .into_iter()
        .map(|(seq, title, url)| {
            let query = tokens(&title).into_iter().take(8).collect::<Vec<_>>().join(" ");
            Probe { subject: Subject::BrowseTitle, query, gold: Gold::Browse { seq, url } }
        })
        .filter(|p| !p.query.is_empty())
        .collect()
}

/// Run one probe.
pub fn run_probe(polis: &Polis<'_>, probe: &Probe, limit: i64) -> ProbeResult {
    if let Some(result) = policy::run(polis, probe, limit) { return result; }
    let start = Instant::now();
    let pack = build_answer_pack(polis, Some(&probe.query), None, limit);
    let ms = start.elapsed().as_millis().min(u32::MAX as u128) as u32;
    let pack_bytes = serde_json::to_vec(&pack).map(|v| v.len()).unwrap_or(0);
    let mut arms = Vec::new();
    let (hit, rank) = match &probe.gold {
        Gold::PromptSeq { seq } => {
            let pos = pack
                .prompt_hits
                .iter()
                .position(|h| h.item.seq == *seq || h.duplicate_of.contains(seq));
            if let Some(i) = pos {
                arms = pack.prompt_hits[i].arms.iter().map(|a| a.arm.as_str().to_string()).collect();
            }
            (pos.is_some(), pos.map(|i| i + 1))
        }
        // A node's links are in filing order, not a ranking — a hit, not a rank.
        Gold::NodeLink { node_id, seq } => match &pack.node {
            Some(n) if &n.node.id == node_id => {
                (n.links.iter().any(|l| l.link.target_id == seq.to_string()), None)
            }
            _ => (false, None),
        },
        Gold::Supersession { old, new } => {
            let new_s = new.to_string();
            let old_s = old.to_string();
            let mut new_ok = false;
            let mut old_ok = true;
            if let Some(n) = &pack.node {
                for l in &n.links {
                    if l.link.target_id == new_s {
                        new_ok = l.superseded_by.is_none();
                    }
                    if l.link.target_id == old_s && l.superseded_by.is_none() {
                        old_ok = false;
                    }
                }
            }
            for h in &pack.prompt_hits {
                if h.item.seq == *new && h.superseded_by.is_none() {
                    new_ok = true;
                }
                if h.item.seq == *old && h.superseded_by.is_none() {
                    old_ok = false;
                }
            }
            (new_ok && old_ok, None)
        }
        Gold::Note { id } => (pack.notes.iter().any(|n| n.id == *id), None),
        Gold::Node { node_id } => (
            pack.node.as_ref().map(|n| &n.node.id == node_id && !n.links.is_empty()).unwrap_or(false),
            None,
        ),
        Gold::Browse { seq, url } => {
            let pos = pack.browse_hits.iter().position(|h| h.seq == Some(*seq) || &h.url == url);
            (pos.is_some(), pos.map(|i| i + 1))
        }
        Gold::ClaimAt { .. } | Gold::EvidencePolicy { .. } => unreachable!("policy probes return before ordinary pack matching"),
    };
    ProbeResult { subject: probe.subject, hit, rank, ms, pack_bytes, arms }
}

/// Evaluate the frozen set: per-subject and aggregate recall, plus the
/// set's own latency (docs/bench.md "canary set" row).
pub fn evaluate(polis: &Polis<'_>, set: &CanarySet, cfg: &CanaryConfig) -> (CanaryReport, Vec<ProbeResult>) {
    let started = Instant::now();
    let results: Vec<ProbeResult> = set.probes.iter().map(|p| run_probe(polis, p, cfg.limit)).collect();
    let mut by: BTreeMap<Subject, (usize, usize)> = BTreeMap::new();
    for r in &results {
        let e = by.entry(r.subject).or_insert((0, 0));
        e.0 += 1;
        if r.hit {
            e.1 += 1;
        }
    }
    let subjects: Vec<SubjectScore> = by
        .into_iter()
        .map(|(subject, (n, hits))| SubjectScore { subject, n, hits, recall: if n == 0 { 0.0 } else { hits as f64 / n as f64 } })
        .collect();
    let n = results.len();
    let hits = results.iter().filter(|r| r.hit).count();
    let mut ms: Vec<u32> = results.iter().map(|r| r.ms).collect();
    ms.sort_unstable();
    let report = CanaryReport {
        run_id: set.run_id,
        head_seq: set.head_seq,
        n,
        hits,
        recall: if n == 0 { 0.0 } else { hits as f64 / n as f64 },
        subjects,
        unfiled_decisions: set.unfiled_decisions,
        packs: n,
        elapsed_ms: started.elapsed().as_millis() as u64,
        p50_ms: polis_core::latency::percentile(&ms, 0.50),
        p95_ms: polis_core::latency::percentile(&ms, 0.95),
    };
    (report, results)
}

/// The nodes whose probes went from hit to miss between two evaluations of
/// the SAME frozen set — the subjects a regression quarantines (B3 §5.3).
/// Prompt-span probes name no node; their misses count in the aggregate and
/// quarantine nothing.
pub fn regressed_subjects(set: &CanarySet, before: &[ProbeResult], after: &[ProbeResult]) -> Vec<String> {
    let mut out = Vec::new();
    for ((probe, b), a) in set.probes.iter().zip(before).zip(after) {
        if b.hit && !a.hit {
            match &probe.gold {
                Gold::NodeLink { node_id, .. } | Gold::Node { node_id } => out.push(node_id.clone()),
                _ => {}
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The frozen verdict a run row keeps (`class_runs.canary_json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanaryVerdict {
    pub before: CanaryReport,
    pub after: CanaryReport,
    pub regression: Option<String>,
    pub quarantined: Vec<String>,
}

/// The §5.3 rule. `Some(reason)` when `after` regressed against `before`.
pub fn regression(before: &CanaryReport, after: &CanaryReport, cfg: &CanaryConfig) -> Option<String> {
    for b in &before.subjects {
        if !b.subject.zero_tolerance() {
            continue;
        }
        if let Some(a) = after.subjects.iter().find(|a| a.subject == b.subject) {
            if a.recall < b.recall {
                return Some(format!(
                    "{} recall fell {:.3} → {:.3} (zero tolerance)",
                    b.subject.as_str(),
                    b.recall,
                    a.recall
                ));
            }
        }
    }
    let n = after.n.max(1) as f64;
    let tolerance = cfg.aggregate_floor.max(3.0 / n);
    if after.recall < before.recall - tolerance {
        return Some(format!(
            "aggregate recall fell {:.3} → {:.3}, below the {:.3} tolerance",
            before.recall, after.recall, tolerance
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{seed_corpus, CorpusSpec};
    use polis_core::host::NoHost;
    use polis_llm::NoopSink;
    use polis_store::PolisStore;

    fn seeded() -> PolisStore {
        let store = PolisStore::open_in_memory().unwrap();
        seed_corpus(&store, &CorpusSpec::new(400).with_seed(11)).unwrap();
        store
    }

    #[test]
    fn the_set_freezes_every_subject_and_the_same_set_for_the_same_run_id() {
        let store = seeded();
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let cfg = CanaryConfig { decisions: 20, spans: 20, classes: 20, ..Default::default() };
        let a = freeze(&polis, 7, &cfg);
        let b = freeze(&polis, 7, &cfg);
        assert_eq!(a.probes.len(), b.probes.len());
        assert_eq!(
            a.probes.iter().map(|p| p.query.clone()).collect::<Vec<_>>(),
            b.probes.iter().map(|p| p.query.clone()).collect::<Vec<_>>()
        );
        for s in [Subject::Decision, Subject::Note, Subject::PromptSpan, Subject::ClassReach] {
            assert!(a.probes.iter().any(|p| p.subject == s), "no {s:?} probe");
        }
        // A different run id spans differently.
        let c = freeze(&polis, 8, &cfg);
        let qa: Vec<_> = a.probes.iter().filter(|p| p.subject == Subject::PromptSpan).map(|p| &p.query).collect();
        let qc: Vec<_> = c.probes.iter().filter(|p| p.subject == Subject::PromptSpan).map(|p| &p.query).collect();
        assert_ne!(qa, qc);
    }

    #[test]
    fn the_seeded_corpus_is_mostly_reachable_and_the_report_adds_up() {
        let store = seeded();
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let cfg = CanaryConfig { decisions: 20, spans: 20, classes: 20, ..Default::default() };
        let set = freeze(&polis, 1, &cfg);
        let (report, results) = evaluate(&polis, &set, &cfg);
        assert_eq!(report.n, results.len());
        assert_eq!(report.hits, results.iter().filter(|r| r.hit).count());
        assert_eq!(report.subjects.iter().map(|s| s.n).sum::<usize>(), report.n);
        assert!(report.recall > 0.5, "recall {:.2} on a coherent synthetic lake: {:?}", report.recall, report.subjects);
        let spans = report.subjects.iter().find(|s| s.subject == Subject::PromptSpan).unwrap();
        assert!(spans.recall >= 0.8, "a span of a prompt finds the prompt: {spans:?}");
    }

    #[test]
    fn the_regression_rule_is_the_plans() {
        let cfg = CanaryConfig::default();
        let mk = |recall: f64, note: f64, n: usize| CanaryReport {
            run_id: 0,
            head_seq: 0,
            n,
            hits: (recall * n as f64) as usize,
            recall,
            subjects: vec![SubjectScore { subject: Subject::Note, n: 10, hits: (note * 10.0) as usize, recall: note }],
            unfiled_decisions: 0,
            packs: n,
            elapsed_ms: 0,
            p50_ms: 0,
            p95_ms: 0,
        };
        // Within tolerance (N=200 → max(0.05, 0.015) = 0.05).
        assert!(regression(&mk(0.90, 1.0, 200), &mk(0.86, 1.0, 200), &cfg).is_none());
        assert!(regression(&mk(0.90, 1.0, 200), &mk(0.84, 1.0, 200), &cfg).is_some());
        // A small set widens the tolerance to 3/N.
        assert!(regression(&mk(0.90, 1.0, 20), &mk(0.76, 1.0, 20), &cfg).is_none());
        // A note regression is never tolerated.
        assert!(regression(&mk(0.90, 1.0, 200), &mk(0.90, 0.9, 200), &cfg).is_some());
    }
}
