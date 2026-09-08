// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The corpus-derived eval (plan §6.2, item 1): the canary's generators at
//! full size plus browse-title reachability, scored as Recall@{5,10,20},
//! MRR, pack-contains-gold, bytes and tokens per pack, and per-arm
//! attribution — over a lake, with no labels and no model.
//!
//! What it measures, stated plainly: whether what Polis recorded can be
//! reached again through the answer pack, by the words that were written.
//! It does not measure human relevance; a probe that "misses" may have
//! returned something a person would have preferred. Leave-one-out filing
//! consistency is deferred to the C-program (it needs class centroids).
//!
//! Three entry points, all behind the `eval` feature's ignored tests:
//! `cargo test -p polis-memory --features eval -- --ignored eval_` writes
//! `bench/results/<date>-<sha>.json`; `POLIS_REAL_DB=<copy>` runs the real-DB
//! instrument (p50/p95 per op + the canary) and the canary calibration; and
//! `eval_gate` (not ignored, `--features eval`) is CI's PR gate against
//! `bench/results/baseline.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::canary::{self, CanaryConfig, CanaryReport, ProbeResult, Subject, SubjectScore};
use crate::latency::OpLatency;
use crate::Polis;

#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub canary: CanaryConfig,
    pub run_id: u64,
    /// Browse-title probes (the eval's extra subject).
    pub browse_titles: usize,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self { canary: CanaryConfig::default(), run_id: 1, browse_titles: 50 }
    }
}

/// A distribution summary over one measure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Dist {
    pub n: usize,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub max: f64,
}

impl Dist {
    pub fn of(mut xs: Vec<f64>) -> Self {
        if xs.is_empty() {
            return Self::default();
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let n = xs.len();
        let pick = |q: f64| xs[((q * n as f64).ceil() as usize).clamp(1, n) - 1];
        Self { n, mean: xs.iter().sum::<f64>() / n as f64, p50: pick(0.50), p95: pick(0.95), max: xs[n - 1] }
    }
}

/// The lake the eval ran over.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LakeShape {
    pub source: String,
    pub prompts: i64,
    pub events: i64,
    pub nodes: usize,
    pub browse_events: i64,
    pub notes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalReport {
    pub schema: u32,
    pub date: String,
    pub sha: String,
    pub machine: String,
    pub lake: LakeShape,
    pub run_id: u64,
    pub probes: usize,
    /// Over the probes that have a ranked list (prompt spans and browse
    /// titles — a node's links are filing order, not a ranking): the share
    /// found within the top k.
    pub recall_at: BTreeMap<String, f64>,
    pub mrr: f64,
    /// Over every probe: found anywhere in the pack.
    pub pack_contains_gold: f64,
    pub bytes_per_pack: Dist,
    pub tokens_per_pack: Dist,
    /// How many gold prompt hits each arm annotated (only fused hits carry
    /// arms; a lexical-only lake reports `lexical_only`).
    pub arm_attribution: BTreeMap<String, u64>,
    pub subjects: Vec<SubjectScore>,
    pub canary: CanaryReport,
    pub latency: Vec<OpLatency>,
    /// `None` until the C-program's centroids exist.
    pub filing_consistency: Option<f64>,
    pub notes: Vec<String>,
}

fn lake_shape(polis: &Polis<'_>, source: &str) -> LakeShape {
    let db = polis.store;
    let stats = crate::retrieval::build_stats(polis);
    LakeShape {
        source: source.to_string(),
        prompts: stats.total_prompts,
        events: stats.total_events,
        nodes: db.list_class_nodes().map(|v| v.len()).unwrap_or(0),
        browse_events: stats.by_kind.iter().find(|(k, _)| k == "browse_event").map(|(_, c)| *c).unwrap_or(0),
        notes: db.list_user_notes(false, 100_000).map(|v| v.len()).unwrap_or(0),
    }
}

/// Run the eval over `polis`. `source` names the lake in the report.
pub fn run_eval(polis: &Polis<'_>, cfg: &EvalConfig, source: &str) -> EvalReport {
    // The lake's shape BEFORE the probes (and before a caller's ingest
    // exercise adds rows to it).
    let lake = lake_shape(polis, source);
    let set = canary::freeze(polis, cfg.run_id, &cfg.canary);
    let (canary_report, mut results) = canary::evaluate(polis, &set, &cfg.canary);
    let extra = canary::browse_title_probes(polis, cfg.browse_titles);
    results.extend(extra.iter().map(|p| canary::run_probe(polis, p, cfg.canary.limit)));
    score(polis, cfg, lake, canary_report, results)
}

fn score(polis: &Polis<'_>, cfg: &EvalConfig, lake: LakeShape, canary: CanaryReport, results: Vec<ProbeResult>) -> EvalReport {
    let ranked: Vec<&ProbeResult> = results
        .iter()
        .filter(|r| matches!(r.subject, Subject::PromptSpan | Subject::BrowseTitle))
        .collect();
    let mut recall_at = BTreeMap::new();
    for k in [5usize, 10, 20] {
        let found = ranked.iter().filter(|r| r.rank.map(|x| x <= k).unwrap_or(false)).count();
        recall_at.insert(k.to_string(), if ranked.is_empty() { 0.0 } else { found as f64 / ranked.len() as f64 });
    }
    let mrr = if ranked.is_empty() {
        0.0
    } else {
        ranked.iter().map(|r| r.rank.map(|x| 1.0 / x as f64).unwrap_or(0.0)).sum::<f64>() / ranked.len() as f64
    };
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
    let mut arm_attribution: BTreeMap<String, u64> = BTreeMap::new();
    for r in results.iter().filter(|r| r.hit && r.subject == Subject::PromptSpan) {
        if r.arms.is_empty() {
            *arm_attribution.entry("lexical_only".to_string()).or_default() += 1;
        }
        for a in &r.arms {
            *arm_attribution.entry(a.clone()).or_default() += 1;
        }
    }
    let bytes: Vec<f64> = results.iter().map(|r| r.pack_bytes as f64).collect();
    let tokens: Vec<f64> = bytes.iter().map(|b| b / 4.0).collect();
    let mut notes = vec![
        "Measures reachability of what Polis recorded through the answer pack, by the words written — not human relevance.".to_string(),
        "Unfiled decisions are unreachable today (no text of their own in the store); the Decision subject covers filed ones.".to_string(),
        "Leave-one-out filing consistency: deferred to the C-program (needs class centroids).".to_string(),
    ];
    if polis.embedder.is_none() {
        notes.push("No embedder configured: the semantic arm was absent for every probe.".to_string());
    }
    EvalReport {
        schema: 1,
        date: today(),
        sha: git_sha(),
        machine: machine(),
        lake,
        run_id: cfg.run_id,
        probes: results.len(),
        recall_at,
        mrr,
        pack_contains_gold: if results.is_empty() { 0.0 } else { results.iter().filter(|r| r.hit).count() as f64 / results.len() as f64 },
        bytes_per_pack: Dist::of(bytes),
        tokens_per_pack: Dist::of(tokens),
        arm_attribution,
        subjects,
        canary,
        latency: crate::latency::report(),
        filing_consistency: None,
        notes,
    }
}

/// `YYYY-MM-DD` (UTC) without a date crate: civil-from-days.
pub fn today() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn machine() -> String {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    format!("{} {} ({cpus} cpus)", std::env::consts::OS, std::env::consts::ARCH)
}

/// Write the report as `<dir>/<date>-<sha>.json`; returns the path.
pub fn write_results(report: &EvalReport, dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}-{}.json", report.date, report.sha));
    std::fs::write(&path, serde_json::to_string_pretty(report).unwrap_or_default())?;
    Ok(path)
}

/// The repo's `bench/results/` directory.
pub fn results_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/results")
}

// ---------------------------------------------------------------------------
// The PR gate
// ---------------------------------------------------------------------------

/// What `bench/results/baseline.json` commits for the gate: the synthetic
/// corpus's Recall@10 and the p95 budgets per op (ms) the gate enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Baseline {
    pub schema: u32,
    pub date: String,
    pub sha: String,
    pub machine: String,
    /// The gate corpus (`corpus.prompts`, `corpus.seed`).
    pub gate_corpus: GateCorpus,
    pub gate: GateNumbers,
    /// Everything else measured on that day, for the record.
    #[serde(default)]
    pub measured: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateCorpus {
    pub prompts: usize,
    pub seed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateNumbers {
    /// Recall@10 on the gate corpus when the baseline was taken.
    pub recall_at_10: f64,
    /// Allowed drop (points, as a fraction) before the gate fails.
    pub recall_tolerance: f64,
    /// p95 ceilings (ms) per op, on the gate corpus.
    pub p95_ms: BTreeMap<String, u32>,
}

/// Compare a fresh report with the baseline. `Err` lists every breach.
pub fn gate(report: &EvalReport, baseline: &Baseline) -> Result<(), Vec<String>> {
    let mut breaches = Vec::new();
    let r10 = report.recall_at.get("10").copied().unwrap_or(0.0);
    let floor = baseline.gate.recall_at_10 - baseline.gate.recall_tolerance;
    if r10 < floor {
        breaches.push(format!("Recall@10 {r10:.3} is below the baseline's {:.3} − {:.3}", baseline.gate.recall_at_10, baseline.gate.recall_tolerance));
    }
    for (op, ceiling) in &baseline.gate.p95_ms {
        match report.latency.iter().find(|l| &l.op == op) {
            Some(l) if l.p95_ms > *ceiling => breaches.push(format!("{op} p95 {} ms is over the {ceiling} ms budget", l.p95_ms)),
            Some(_) => {}
            None => breaches.push(format!("{op} was never recorded — the eval no longer exercises it")),
        }
    }
    if breaches.is_empty() {
        Ok(())
    } else {
        Err(breaches)
    }
}

/// The ops the gate watches and the §6.1 p95 ceilings (ms) applied to the
/// 1k gate corpus — the 10k rows used as-is, which is conservative at 1k.
pub fn gate_budget() -> BTreeMap<String, u32> {
    [("ingest.item", 20), ("pack", 200), ("grep", 100), ("context", 300)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// Exercise the ops the gate budgets, on top of the eval's own packs:
/// `ingest.item` (a batch of fresh prompts), `grep` (identifiers the corpus
/// planted), `context` (the grounding block for a few probes).
pub fn exercise_budgeted_ops(polis: &Polis<'_>, api: &dyn polis_core::MemoryApi, queries: &[String]) {
    use polis_core::api::{GrepRequest, IngestItem, IngestRequest};
    let items: Vec<IngestItem> = (0..100)
        .map(|i| IngestItem { body: format!("eval ingest sample {i} keeper ledger tile budget"), ..Default::default() })
        .collect();
    let _ = api.ingest(&IngestRequest { items, ..Default::default() });
    for needle in ["ERR_POLIS", "src/keeper", "--redline-limit", "ERR_TILE", "src/voice"] {
        let _ = api.grep(&GrepRequest { literal: needle.to_string(), ..Default::default() });
    }
    for q in queries.iter().take(20) {
        let _ = crate::retrieval::context_block(polis, q, None, 8_000);
    }
}

// ---------------------------------------------------------------------------
// Calibration: the canary's noise floor
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Calibration {
    pub runs: usize,
    pub aggregate_mean: f64,
    pub aggregate_stddev: f64,
    pub subjects: BTreeMap<String, (f64, f64)>,
    pub n_per_run: Dist,
    pub set_ms: Dist,
    /// The plan's rule at this N: `max(0.05, 3/N)`.
    pub rule_tolerance: f64,
    /// 3 × the observed aggregate stddev — what the floor would need to be to
    /// stay above the noise with the same run-id reseeding.
    pub three_sigma: f64,
}

/// Freeze + evaluate the canary `runs` times with different run ids, so the
/// PromptSpan spans (the only seeded subject) move — the spread is the noise
/// a real before/after comparison must clear.
pub fn calibrate(polis: &Polis<'_>, cfg: &CanaryConfig, runs: usize) -> Calibration {
    let mut aggregates = Vec::new();
    let mut per: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut ns = Vec::new();
    let mut ms = Vec::new();
    let mut last_n = 1usize;
    for run in 1..=runs.max(1) as u64 {
        let set = canary::freeze(polis, run, cfg);
        let started = Instant::now();
        let (report, _) = canary::evaluate(polis, &set, cfg);
        ms.push(started.elapsed().as_millis() as f64);
        aggregates.push(report.recall);
        ns.push(report.n as f64);
        last_n = report.n.max(1);
        for s in &report.subjects {
            per.entry(s.subject.as_str().to_string()).or_default().push(s.recall);
        }
    }
    let (mean, sd) = mean_sd(&aggregates);
    Calibration {
        runs: runs.max(1),
        aggregate_mean: mean,
        aggregate_stddev: sd,
        subjects: per.into_iter().map(|(k, v)| (k, mean_sd(&v))).collect(),
        n_per_run: Dist::of(ns),
        set_ms: Dist::of(ms),
        rule_tolerance: cfg.aggregate_floor.max(3.0 / last_n as f64),
        three_sigma: 3.0 * sd,
    }
}

fn mean_sd(xs: &[f64]) -> (f64, f64) {
    if xs.is_empty() {
        return (0.0, 0.0);
    }
    let mean = xs.iter().sum::<f64>() / xs.len() as f64;
    let var = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / xs.len() as f64;
    (mean, var.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dist_and_civil_date_are_sane() {
        let d = Dist::of(vec![3.0, 1.0, 2.0, 10.0]);
        assert_eq!(d.n, 4);
        assert_eq!(d.max, 10.0);
        assert_eq!(d.p50, 2.0);
        assert_eq!(Dist::of(vec![]).n, 0);
        let t = today();
        assert_eq!(t.len(), 10);
        assert!(t.starts_with("20"));
    }

    #[test]
    fn the_gate_reads_recall_and_every_budgeted_op() {
        let mut latency = vec![OpLatency { op: "pack".into(), n: 1, p50_ms: 1, p95_ms: 5, max_ms: 5 }];
        let report = |latency: Vec<OpLatency>, r10: f64| EvalReport {
            schema: 1,
            date: today(),
            sha: "x".into(),
            machine: "m".into(),
            lake: LakeShape::default(),
            run_id: 1,
            probes: 0,
            recall_at: [("10".to_string(), r10)].into_iter().collect(),
            mrr: 0.0,
            pack_contains_gold: 0.0,
            bytes_per_pack: Dist::default(),
            tokens_per_pack: Dist::default(),
            arm_attribution: BTreeMap::new(),
            subjects: vec![],
            canary: CanaryReport { run_id: 1, head_seq: 0, n: 0, hits: 0, recall: 0.0, subjects: vec![], unfiled_decisions: 0, packs: 0, elapsed_ms: 0, p50_ms: 0, p95_ms: 0 },
            latency,
            filing_consistency: None,
            notes: vec![],
        };
        let baseline = Baseline {
            schema: 1,
            date: today(),
            sha: "x".into(),
            machine: "m".into(),
            gate_corpus: GateCorpus { prompts: 1_000, seed: 1 },
            gate: GateNumbers { recall_at_10: 0.90, recall_tolerance: 0.02, p95_ms: [("pack".to_string(), 200u32)].into_iter().collect() },
            measured: serde_json::Value::Null,
        };
        assert!(gate(&report(latency.clone(), 0.89), &baseline).is_ok());
        assert!(gate(&report(latency.clone(), 0.87), &baseline).is_err());
        latency[0].p95_ms = 201;
        assert!(gate(&report(latency, 0.95), &baseline).is_err());
        assert!(gate(&report(vec![], 0.95), &baseline).is_err(), "an unrecorded op is a breach");
    }
}

/// The feature-gated instruments: the results writer, the gate, the real-DB
/// instrument, the calibration. Run with `--features eval`.
#[cfg(all(test, feature = "eval"))]
mod instruments {
    use super::*;
    use crate::corpus::{seed_corpus, CorpusSpec};
    use polis_core::host::NoHost;
    use polis_llm::NoopSink;
    use polis_store::PolisStore;
    use std::sync::Arc;

    /// The gate corpus: 1k prompts, one fixed seed.
    pub const GATE_PROMPTS: usize = 1_000;
    pub const GATE_SEED: u64 = 0x5eed_0001;

    fn gate_run() -> (EvalReport, crate::corpus::CorpusReport) {
        crate::latency::reset();
        let store = Arc::new(PolisStore::open_in_memory().unwrap());
        let corpus = seed_corpus(&store, &CorpusSpec::new(GATE_PROMPTS).with_seed(GATE_SEED)).unwrap();
        let handle = crate::PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink));
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let cfg = EvalConfig::default();
        let mut report = run_eval(&polis, &cfg, &format!("synthetic-{GATE_PROMPTS}-seed-{GATE_SEED:#x}"));
        let queries: Vec<String> = canary::freeze(&polis, cfg.run_id, &cfg.canary).probes.iter().map(|p| p.query.clone()).collect();
        exercise_budgeted_ops(&polis, &handle, &queries);
        report.latency = crate::latency::report();
        (report, corpus)
    }

    /// CI's PR gate: the deterministic synthetic corpus against the
    /// committed baseline.
    #[test]
    fn eval_gate() {
        let (report, _) = gate_run();
        let path = results_dir().join("baseline.json");
        let baseline: Baseline = serde_json::from_str(&std::fs::read_to_string(&path).expect("bench/results/baseline.json")).unwrap();
        assert_eq!(baseline.gate_corpus.prompts, GATE_PROMPTS, "the baseline was taken on another corpus size");
        assert_eq!(baseline.gate_corpus.seed, GATE_SEED, "the baseline was taken on another seed");
        if let Err(breaches) = gate(&report, &baseline) {
            panic!("eval gate failed:\n  {}\nreport: {}", breaches.join("\n  "), serde_json::to_string_pretty(&report).unwrap());
        }
    }

    /// Write today's results file for the synthetic corpus (and print the
    /// numbers the baseline is taken from).
    #[test]
    #[ignore]
    fn eval_synthetic_writes_results() {
        let (report, corpus) = gate_run();
        let path = write_results(&report, &results_dir()).unwrap();
        println!("wrote {}", path.display());
        println!("corpus: {}", serde_json::to_string(&corpus).unwrap());
        println!("recall@: {:?} mrr {:.3} contains {:.3}", report.recall_at, report.mrr, report.pack_contains_gold);
        println!("subjects: {:?}", report.subjects);
        println!("latency: {}", serde_json::to_string(&report.latency).unwrap());
        println!("canary: n={} recall={:.3} p50={} p95={} ms", report.canary.n, report.canary.recall, report.canary.p50_ms, report.canary.p95_ms);
    }

    /// The canary's noise floor on the synthetic corpus.
    #[test]
    #[ignore]
    fn eval_canary_calibration_synthetic() {
        let store = PolisStore::open_in_memory().unwrap();
        seed_corpus(&store, &CorpusSpec::new(GATE_PROMPTS).with_seed(GATE_SEED)).unwrap();
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let cal = calibrate(&polis, &CanaryConfig::default(), 10);
        println!("calibration(synthetic): {}", serde_json::to_string_pretty(&cal).unwrap());
    }

    fn real_db_copy() -> Option<(tempdir::TempDirGuard, PathBuf)> {
        let src = std::env::var("POLIS_REAL_DB").ok()?;
        let dir = tempdir::TempDirGuard::new("polis-eval");
        let dst = dir.path().join("real.db");
        std::fs::copy(&src, &dst).expect("copy the real DB");
        Some((dir, dst))
    }

    /// The real-DB instrument: open a COPY, time every op, run the canary,
    /// print p50/p95. `POLIS_REAL_DB=<path to a copy of the backup>`.
    #[test]
    #[ignore]
    fn eval_real_db_instrument() {
        let Some((_guard, path)) = real_db_copy() else {
            eprintln!("POLIS_REAL_DB not set — skipping");
            return;
        };
        crate::latency::reset();
        let store = Arc::new(PolisStore::open(&path).expect("open the copy"));
        let handle = crate::PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink));
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let cfg = EvalConfig::default();
        let set = canary::freeze(&polis, cfg.run_id, &cfg.canary);
        let queries: Vec<String> = set.probes.iter().map(|p| p.query.clone()).collect();
        // Cold: the first pack after open.
        let t = Instant::now();
        let _ = crate::retrieval::build_answer_pack(&polis, queries.first().map(String::as_str), None, 8);
        let cold_ms = t.elapsed().as_millis();
        let mut report = run_eval(&polis, &cfg, "real-db-copy");
        exercise_budgeted_ops(&polis, &handle, &queries);
        report.latency = crate::latency::report();
        println!("lake: {}", serde_json::to_string(&report.lake).unwrap());
        println!("pack cold (first after open): {cold_ms} ms");
        for l in &report.latency {
            println!("{:<28} n={:<5} p50={:<6} p95={:<6} max={} ms", l.op, l.n, l.p50_ms, l.p95_ms, l.max_ms);
        }
        println!("recall@: {:?} mrr {:.3} contains {:.3}", report.recall_at, report.mrr, report.pack_contains_gold);
        println!("subjects: {:?}", report.subjects);
        println!("canary: n={} recall={:.3} p50={} p95={} ms elapsed={} ms unfiled_decisions={}", report.canary.n, report.canary.recall, report.canary.p50_ms, report.canary.p95_ms, report.canary.elapsed_ms, report.canary.unfiled_decisions);
        println!("bytes/pack: {:?}", report.bytes_per_pack);
        let path = write_results(&report, &results_dir().join("real")).unwrap();
        println!("wrote {}", path.display());
    }

    /// The canary's noise floor on the real DB copy.
    #[test]
    #[ignore]
    fn eval_canary_calibration_real_db() {
        let Some((_guard, path)) = real_db_copy() else {
            eprintln!("POLIS_REAL_DB not set — skipping");
            return;
        };
        let store = PolisStore::open(&path).expect("open the copy");
        let polis = Polis::new(&store, None, &NoHost, &NoopSink);
        let cal = calibrate(&polis, &CanaryConfig::default(), 10);
        println!("calibration(real): {}", serde_json::to_string_pretty(&cal).unwrap());
    }

    /// Diagnostic: the same lake as a FILE (WAL, as an install has it) and in
    /// memory, the same queries — how much of the bench's file-backed cost
    /// is SQLite's page cache. Prints per-op medians for: file as opened,
    /// file after a WAL checkpoint, file with a 64 MB page cache, memory.
    #[test]
    #[ignore]
    fn eval_file_vs_memory_diagnostic() {
        use crate::corpus::{seed_corpus, CorpusSpec};
        let dir = tempdir::TempDirGuard::new("polis-fvm");
        let path = dir.path().join("lake.db");
        let file = PolisStore::open(&path).unwrap();
        seed_corpus(&file, &CorpusSpec::new(GATE_PROMPTS).with_seed(GATE_SEED)).unwrap();
        let mem = PolisStore::open_in_memory().unwrap();
        seed_corpus(&mem, &CorpusSpec::new(GATE_PROMPTS).with_seed(GATE_SEED)).unwrap();
        let queries: Vec<String> = {
            let polis = Polis::new(&mem, None, &NoHost, &NoopSink);
            canary::freeze(&polis, 1, &CanaryConfig::default()).probes.iter().filter(|p| p.subject == Subject::PromptSpan).map(|p| p.query.clone()).collect()
        };
        let needles = ["ERR_POLIS", "src/keeper", "--redline-limit", "ERR_TILE", "src/voice", "--drafter-mode"];
        let measure = |store: &PolisStore, label: &str| {
            let polis = Polis::new(store, None, &NoHost, &NoopSink);
            let mut pack_ms = Vec::new();
            for q in queries.iter().take(30) {
                let t = Instant::now();
                let _ = crate::retrieval::build_answer_pack(&polis, Some(q), None, 8);
                pack_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let mut grep_ms = Vec::new();
            for n in needles.iter().cycle().take(18) {
                let t = Instant::now();
                let _ = store.grep_memory(n, None, false, polis_core::types::GrepScope::All, 20);
                grep_ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{label:<34} pack p50 {:6.1} ms   grep p50 {:6.1} ms", Dist::of(pack_ms).p50, Dist::of(grep_ms).p50);
        };
        measure(&file, "file (as opened, WAL after seeding)");
        file.conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        measure(&file, "file (after WAL checkpoint)");
        file.conn().execute_batch("PRAGMA cache_size = -65536").unwrap();
        measure(&file, "file (64 MB page cache)");
        file.conn().execute_batch("PRAGMA mmap_size = 268435456").unwrap();
        measure(&file, "file (64 MB cache + 256 MB mmap)");
        measure(&mem, "memory");
    }

    mod tempdir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        /// A temp dir removed on drop (no `tempfile` dependency).
        pub struct TempDirGuard(PathBuf);
        impl TempDirGuard {
            pub fn new(tag: &str) -> Self {
                static N: AtomicU64 = AtomicU64::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                let p = std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()));
                std::fs::create_dir_all(&p).unwrap();
                Self(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}

/// C2: every embedding provider this machine can build, measured on the
/// real corpus with the same instruments (docs/bench.md "Embedding
/// providers"). `POLIS_REAL_DB=<copy>`, `POLIS_MODELS_DIR=<root>` (where a
/// first-use download may land), `POLIS_EVAL_PROVIDERS=apple-sentence,
/// apple-contextual,model2vec,fastembed` (default: all four; an unbuildable
/// one is reported as unavailable, not skipped silently).
#[cfg(all(test, feature = "eval"))]
mod provider_instruments {
    use super::*;
    use crate::canary::{Gold, Subject};
    use polis_core::host::NoHost;
    use polis_embed::{Embedder, ProviderChoice, ProviderKind};
    use polis_llm::NoopSink;
    use polis_store::PolisStore;
    use std::sync::Arc;
    use std::time::Instant;

    fn build(name: &str, root: &Path) -> Result<Option<Arc<dyn Embedder>>, String> {
        Ok(match name {
            "apple-sentence" => polis_embed::apple_named(ProviderKind::AppleSentence),
            "apple-contextual" => polis_embed::apple_named(ProviderKind::AppleContextual),
            "model2vec" => polis_embed::select(ProviderChoice::Model2Vec, Some(root), true)?,
            "fastembed" => polis_embed::select(ProviderChoice::FastEmbed, Some(root), true)?,
            "remote" => polis_embed::select(ProviderChoice::Remote, Some(root), true)?,
            other => return Err(format!("unknown provider {other}")),
        })
    }

    /// `POLIS_REAL_DB=<copy> POLIS_MODELS_DIR=<root> cargo test -p polis-memory --features eval[,fastembed] -- --ignored eval_real_db_providers --nocapture`
    #[test]
    #[ignore]
    fn eval_real_db_providers() {
        let Some(src) = std::env::var("POLIS_REAL_DB").ok() else {
            eprintln!("POLIS_REAL_DB not set — skipping");
            return;
        };
        let root = std::env::var("POLIS_MODELS_DIR").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir().join("polis-c2-models"));
        let names: Vec<String> = std::env::var("POLIS_EVAL_PROVIDERS")
            .unwrap_or_else(|_| "apple-sentence,apple-contextual,model2vec,fastembed".into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let mut out: Vec<serde_json::Value> = Vec::new();
        for name in &names {
            let embedder = match build(name, &root) {
                Ok(Some(e)) => e,
                Ok(None) => {
                    eprintln!("{name}: unavailable on this machine");
                    out.push(serde_json::json!({"provider": name, "available": false}));
                    continue;
                }
                Err(e) => {
                    eprintln!("{name}: {e}");
                    out.push(serde_json::json!({"provider": name, "available": false, "error": e}));
                    continue;
                }
            };
            let model = embedder.model_id();
            let dim = embedder.dim();
            // A fresh copy per provider: the index is rebuilt under this model.
            let dst = std::env::temp_dir().join(format!("polis-c2-prov-{}-{}.db", std::process::id(), name));
            let _ = std::fs::remove_file(&dst);
            std::fs::copy(&src, &dst).expect("copy the real DB");
            let store = PolisStore::open(&dst).expect("open the copy");
            // Embed time per chunk, measured directly on 256 real prompt heads
            // (the stored index may already hold this model's rows, which
            // would make the tick's timing say nothing).
            let heads: Vec<String> = {
                let conn = store.conn();
                let mut st = conn.prepare("SELECT substr(fts_text, 1, 400) FROM prompts WHERE COALESCE(role,'user') <> 'agent' AND LENGTH(fts_text) > 0 ORDER BY id DESC LIMIT 256").unwrap();
                st.query_map([], |r| r.get::<_, String>(0)).unwrap().collect::<Result<_, _>>().unwrap()
            };
            let t = Instant::now();
            let _ = embedder.embed(&heads).expect("embed");
            let embed_ms_per_chunk = t.elapsed().as_secs_f64() * 1000.0 / heads.len().max(1) as f64;
            // Index the whole corpus under this model.
            let t = Instant::now();
            let mut targets = 0usize;
            loop {
                let n = polis_embed::index_tick(&store, embedder.as_ref(), 512);
                if n == 0 {
                    break;
                }
                targets += n;
            }
            let index_ms = t.elapsed().as_secs_f64() * 1000.0;
            let (rows, stored_dim) = store
                .models_in_index()
                .unwrap_or_default()
                .into_iter()
                .find(|(m, _, _)| *m == model)
                .map(|(_, d, n)| (n, d))
                .unwrap_or((0, dim as i64));
            let index_bytes = rows * (stored_dim + 4);
            let polis = Polis::new(&store, None, &NoHost, &NoopSink).with_embedder(Some(embedder.clone()));
            // Semantic arm alone: PromptSpan probes, top-10 by cosine.
            let set = crate::canary::freeze(&polis, 7, &CanaryConfig::default());
            let (mut n_span, mut hit_span) = (0usize, 0usize);
            for p in set.probes.iter().filter(|p| p.subject == Subject::PromptSpan) {
                let Gold::PromptSeq { seq } = &p.gold else { continue };
                n_span += 1;
                let hits = polis_embed::semantic_search(&store, embedder.as_ref(), &p.query, 10).unwrap_or_default();
                let ids: Vec<i64> = hits.iter().filter(|h| h.target_kind == "prompt").map(|h| h.target_id).collect();
                let seqs = store.seqs_for_prompt_ids(&ids).unwrap_or_default();
                if seqs.values().any(|s| s == seq) {
                    hit_span += 1;
                }
            }
            let semantic_r10 = if n_span == 0 { 0.0 } else { hit_span as f64 / n_span as f64 };
            // Fused: the whole eval with this embedder on the pack.
            let report = run_eval(&polis, &EvalConfig::default(), "real");
            let fused_r10 = report.recall_at.get("10").copied().unwrap_or(0.0);
            // C1's calibration under this model.
            let tree = store.list_class_nodes().unwrap();
            let cal = crate::filing::leave_one_out(&store, &model, &tree).expect("calibration");
            let chosen = cal.chosen.as_ref().map(|g| serde_json::json!({"t1": g.t1, "margin": g.margin, "precision": g.precision, "coverage": g.coverage}));
            let best = cal.best.as_ref().map(|g| serde_json::json!({"t1": g.t1, "margin": g.margin, "precision": g.precision, "coverage": g.coverage}));
            eprintln!(
                "{name}: model={model} dim={dim} embed {embed_ms_per_chunk:.2} ms/chunk · index {targets} targets / {rows} rows in {index_ms:.0} ms ({index_bytes} B) · semantic R@10 {semantic_r10:.3} ({hit_span}/{n_span}) · fused R@10 {fused_r10:.3} MRR {:.3} · canary {:.3} · filing consistency {:.3} chosen {chosen:?} best {best:?}",
                report.mrr, report.canary.recall, cal.consistency
            );
            out.push(serde_json::json!({
                "provider": name, "available": true, "model": model, "dim": dim,
                "embed_ms_per_chunk": embed_ms_per_chunk, "index_targets": targets, "index_rows": rows, "index_ms": index_ms, "index_bytes": index_bytes,
                "semantic_recall_at_10": semantic_r10, "semantic_probes": n_span,
                "fused_recall_at": report.recall_at, "fused_mrr": report.mrr, "arm_attribution": report.arm_attribution,
                "canary_recall": report.canary.recall,
                "filing_consistency": cal.consistency, "filing_population": cal.population, "filing_coverable": cal.coverable,
                "filing_chosen": chosen, "filing_best": best,
                "pack_p50_ms": report.latency.iter().find(|l| l.op == "pack").map(|l| l.p50_ms),
            }));
            drop(polis);
            drop(store);
            let _ = std::fs::remove_file(&dst);
        }
        let doc = serde_json::json!({ "schema": 1, "date": today(), "sha": git_sha(), "machine": machine(), "providers": out });
        let dir = results_dir();
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join(format!("{}-{}-providers.json", today(), git_sha()));
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        eprintln!("wrote {}", path.display());
    }
}
