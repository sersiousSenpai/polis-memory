// SPDX-License-Identifier: Apache-2.0
//! The prompt-time injection floor, calibrated and gated on labelled prompts
//! (`bench/realistic/inject.json`): should-inject prompts must surface their
//! expected evidence, must-not-inject prompts (generic chores, unrelated
//! questions that share common words, another project's facts) must inject
//! nothing.
//!
//! The floor is chosen on the development split — the lowest floor whose
//! false-injection rate is at most 5% — and the shipped
//! `inject::DEFAULT_FLOOR` is held to the same bound on the heldout split.
//! `POLIS_WRITE_RESULTS=1` writes the sweep to `bench/results/inject-floor.json`
//! (dated when kept, e.g. `2026-10-07-inject-floor.json`).

use std::path::PathBuf;
use std::sync::Arc;

use polis_core::api::{IngestItem, IngestRequest, Scope};
use polis_core::host::NoHost;
use polis_core::MemoryApi;
use polis_llm::NoopSink;
use polis_memory::inject::{injection_for, InjectConfig, PromptSite, DEFAULT_FLOOR};
use polis_memory::PolisHandle;
use polis_store::PolisStore;
use serde_json::Value;

const MAX_FALSE_INJECTION: f64 = 0.05;
const MIN_HIT_RATE: f64 = 0.7;

fn fixture() -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/realistic/inject.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn seeded(fx: &Value) -> PolisHandle {
    let api = PolisHandle::new(Arc::new(PolisStore::open_in_memory().unwrap()), None, Arc::new(NoHost), Arc::new(NoopSink));
    let mut items = Vec::new();
    let filler = &fx["filler"];
    let mut ts = 1i64;
    for (t, template) in filler["templates"].as_array().unwrap().iter().enumerate() {
        for (m, module) in filler["modules"].as_array().unwrap().iter().enumerate() {
            items.push(IngestItem {
                body: template.as_str().unwrap().replace("{m}", module.as_str().unwrap()),
                ts: Some(ts),
                role: Some(if (t + m) % 2 == 0 { "user" } else { "assistant" }.into()),
                session: Some(format!("chore-{}", (t * 20 + m) % 37)),
                run: None,
                project: filler["project"].as_str().map(str::to_string),
            });
            ts += 1;
        }
    }
    for item in fx["corpus"].as_array().unwrap() {
        items.push(IngestItem {
            body: item["body"].as_str().unwrap().into(),
            ts: item["ts"].as_i64(),
            role: item["role"].as_str().map(str::to_string),
            session: item["session"].as_str().map(str::to_string),
            run: None,
            project: item["project"].as_str().map(str::to_string),
        });
    }
    for project in items.iter().filter_map(|i| i.project.clone()).collect::<std::collections::BTreeSet<_>>() {
        let batch: Vec<IngestItem> = items.iter().filter(|i| i.project.as_deref() == Some(&project)).cloned().collect();
        let receipt = api.ingest(&IngestRequest { items: batch, scope: Scope { project: Some(project), ..Scope::default() } }).unwrap();
        assert_eq!(receipt.skipped, 0);
    }
    api
}

#[derive(Debug, Default, Clone, Copy)]
struct Rates {
    hits: usize,
    positives: usize,
    false_injections: usize,
    negatives: usize,
}

impl Rates {
    fn hit_rate(self) -> f64 {
        self.hits as f64 / self.positives.max(1) as f64
    }
    fn false_rate(self) -> f64 {
        self.false_injections as f64 / self.negatives.max(1) as f64
    }
}

fn measure(api: &PolisHandle, fx: &Value, split: &str, floor: f64) -> (Rates, Vec<String>) {
    let project = fx["project"].as_str().unwrap();
    let site = PromptSite { session: Some("probe"), project: Some(project), before: None };
    let cfg = InjectConfig { floor, ..InjectConfig::default() };
    let mut rates = Rates::default();
    let mut misses = Vec::new();
    for case in fx["cases"].as_array().unwrap().iter().filter(|c| c["split"] == split) {
        let prompt = case["prompt"].as_str().unwrap();
        let injection = injection_for(api, prompt, &site, &cfg);
        if let Some(i) = &injection {
            assert!(i.text.len() <= cfg.max_bytes, "{} bytes", i.text.len());
        }
        if case["expectInject"].as_bool().unwrap() {
            rates.positives += 1;
            let found = injection.as_ref().is_some_and(|i| case["expected"].as_array().unwrap().iter().all(|e| i.text.contains(e.as_str().unwrap())));
            if found { rates.hits += 1 } else { misses.push(format!("missed {}", case["id"])) }
        } else {
            rates.negatives += 1;
            if injection.is_some() {
                rates.false_injections += 1;
                misses.push(format!("injected {}", case["id"]));
            }
        }
    }
    (rates, misses)
}

#[test]
fn inject_floor_eval_gate() {
    let fx = fixture();
    let api = seeded(&fx);
    let floors: Vec<f64> = (1..=19).map(|i| i as f64 * 0.05).collect();
    let mut sweep = Vec::new();
    let mut calibrated = None;
    for &floor in &floors {
        let (dev, _) = measure(&api, &fx, "development", floor);
        let (held, _) = measure(&api, &fx, "heldout", floor);
        if calibrated.is_none() && dev.false_rate() <= MAX_FALSE_INJECTION {
            calibrated = Some(floor);
        }
        sweep.push(serde_json::json!({
            "floor": (floor * 100.0).round() / 100.0,
            "development": { "hitRate": dev.hit_rate(), "falseInjectionRate": dev.false_rate() },
            "heldout": { "hitRate": held.hit_rate(), "falseInjectionRate": held.false_rate() },
        }));
        println!("floor {floor:.2} · dev hit {:.2} false {:.2} · heldout hit {:.2} false {:.2}", dev.hit_rate(), dev.false_rate(), held.hit_rate(), held.false_rate());
    }
    let (held, misses) = measure(&api, &fx, "heldout", DEFAULT_FLOOR);
    let (dev, dev_misses) = measure(&api, &fx, "development", DEFAULT_FLOOR);
    println!("DEFAULT_FLOOR {DEFAULT_FLOOR} · calibrated on development {calibrated:?}");
    println!("development misses: {dev_misses:?}\nheldout misses: {misses:?}");

    if std::env::var("POLIS_WRITE_RESULTS").is_ok_and(|v| v == "1") {
        let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/results/inject-floor.json");
        let doc = serde_json::json!({
            "schema": "polis.inject-floor/1",
            "fixture": "bench/realistic/inject.json",
            "method": "lexical floor: IDF-weighted term coverage x rarest-term specificity; chosen on development as the lowest floor with false injection <= 5%; held to the same bound on heldout",
            "defaultFloor": DEFAULT_FLOOR,
            "calibratedOnDevelopment": calibrated,
            "atDefault": {
                "development": { "hitRate": dev.hit_rate(), "falseInjectionRate": dev.false_rate(), "misses": dev_misses },
                "heldout": { "hitRate": held.hit_rate(), "falseInjectionRate": held.false_rate(), "misses": misses },
            },
            "sweep": sweep,
            "modelCalls": 0,
            "embedder": "none",
        });
        std::fs::write(out, format!("{}\n", serde_json::to_string_pretty(&doc).unwrap())).unwrap();
    }

    let calibrated = calibrated.expect("some floor keeps development false injections within bound");
    assert!(DEFAULT_FLOOR + 1e-9 >= calibrated, "DEFAULT_FLOOR {DEFAULT_FLOOR} is below the development-calibrated floor {calibrated}");
    assert!(held.false_rate() <= MAX_FALSE_INJECTION, "heldout false injections {:.2} at the default floor: {misses:?}", held.false_rate());
    assert!(held.hit_rate() >= MIN_HIT_RATE, "heldout hit rate {:.2} at the default floor: {misses:?}", held.hit_rate());
}

/// The prompt's own session is never echoed back to it, and a prompt the
/// record has nothing on injects nothing.
#[test]
fn injection_skips_the_current_session_and_says_nothing_when_unsure() {
    let fx = fixture();
    let api = seeded(&fx);
    let project = fx["project"].as_str();
    let cfg = InjectConfig::default();
    let prompt = "what port does the Bluebird API listen on?";
    let elsewhere = injection_for(&api, prompt, &PromptSite { session: Some("probe"), project, before: None }, &cfg).expect("a fact from another session");
    assert!(elsewhere.text.contains("9090"));
    assert!(elsewhere.text.starts_with("Polis memory:"));
    assert!(elsewhere.text.contains(&format!("#{}", elsewhere.cited[0].0)));
    let same = injection_for(&api, prompt, &PromptSite { session: Some("fact-port"), project, before: None }, &cfg);
    assert!(same.as_ref().is_none_or(|i| !i.text.contains("9090")), "the fact's own session already has it in context: {same:?}");
    assert!(injection_for(&api, "continue", &PromptSite { session: Some("probe"), project, before: None }, &cfg).is_none());
    // An earlier asking of the same question is not the answer to it.
    api.ingest(&IngestRequest {
        items: vec![IngestItem { body: "what port does the Bluebird API listen on?".into(), session: Some("asked-before".into()), project: project.map(str::to_string), ..Default::default() }],
        scope: Scope { project: project.map(str::to_string), ..Scope::default() },
    })
    .unwrap();
    let site = PromptSite { session: Some("probe"), project, before: None };
    let candidates = polis_memory::inject::scored_candidates(&api, "which port does the Bluebird API listen on?", &site);
    assert!(candidates.iter().any(|c| c.body.contains("9090")), "{candidates:?}");
    assert!(!candidates.iter().any(|c| c.body.ends_with('?')), "a past question is not a candidate: {candidates:?}");
    let again = injection_for(&api, "which port does the Bluebird API listen on?", &site, &cfg).expect("asking again still finds the answer");
    assert!(again.text.contains("9090"), "{}", again.text);
    let other_project = injection_for(&api, prompt, &PromptSite { session: Some("probe"), project: Some("/work/heron"), before: None }, &cfg);
    assert!(other_project.as_ref().is_none_or(|i| !i.text.contains("9090")), "scoped to the prompt's project: {other_project:?}");
}
