// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Recorded-reply agents: what stands in for a model when a test — or a CI
//! runner with no key and no CLI — drives the gardener end to end.
//!
//! - [`ScriptedAgent`] answers every seat from a closure over the prompt.
//! - [`filing_reply`] is a deterministic classifier: it files every delta
//!   item it was shown under the item's own provenance root — the shape a
//!   well-behaved model produces — and refutes as a verifier.
//! - [`obedient_reply`] is the §5.5 adversary: it OBEYS whatever instruction
//!   it finds inside a fenced item (merge everything, supersede #12,
//!   collapse), which is exactly what the screen and the adjudicator must
//!   stop.
//! - [`Roots`] is a host that knows some project roots and nothing else.

use std::collections::HashSet;
use std::sync::Arc;

use polis_core::host::HostResolver;
use polis_llm::{async_trait, Agent, AgentError, AgentReply, AgentRequest, Usage};

use crate::fence::Fence;
use crate::organize::{root_id_for_path, GENERAL_ROOT_ID};

/// A model that answers from a closure. `calls` counts the turns.
pub struct ScriptedAgent {
    reply: Box<dyn Fn(&str, &str) -> String + Send + Sync>,
    calls: std::sync::Mutex<Vec<(String, String)>>,
}

impl ScriptedAgent {
    pub fn new(reply: impl Fn(&str, &str) -> String + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { reply: Box::new(reply), calls: std::sync::Mutex::new(Vec::new()) })
    }

    /// Every `(seat, prompt)` this agent was asked.
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[async_trait]
impl Agent for ScriptedAgent {
    fn name(&self) -> &'static str {
        "scripted"
    }
    async fn run(&self, req: AgentRequest) -> Result<AgentReply, AgentError> {
        let text = (self.reply)(&req.seat, &req.prompt);
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).push((req.seat.clone(), req.prompt.clone()));
        let json = req.response_key.and_then(|k| polis_core::json::extract_object_with_key(&text, k));
        Ok(AgentReply { text, json, session_id: None, usage: Usage::default(), clipped: false })
    }
}

/// A host with project roots and nothing else.
pub struct Roots(pub Vec<String>);

impl HostResolver for Roots {
    fn label(&self, _kind: &str, _id: &str) -> Option<String> {
        None
    }
    fn thread_stats(&self, _kind: &str, _id: &str) -> Option<(i64, Option<i64>)> {
        None
    }
    fn project_roots(&self) -> Vec<String> {
        self.0.clone()
    }
    fn revision_markdown(&self, _session: &str, _version: i64) -> Option<String> {
        None
    }
    fn revision_title(&self, _session: &str, _version: i64) -> Option<String> {
        None
    }
    fn session_status(&self, _session: &str) -> Option<String> {
        None
    }
    fn decision_evidence(&self, _seq: i64) -> Option<String> {
        None
    }
}

/// One delta item's header facts, parsed back from the classifier prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownItem {
    pub seq: i64,
    pub kind: String,
    pub project: Option<String>,
    pub role: String,
}

/// The delta items a classifier prompt shows (its header lines).
pub fn shown_items(prompt: &str) -> Vec<ShownItem> {
    let mut out = Vec::new();
    for line in prompt.lines() {
        let Some(rest) = line.strip_prefix("- seq ") else { continue };
        let fields: Vec<&str> = rest.split(" | ").collect();
        if fields.len() < 4 {
            continue;
        }
        let Ok(seq) = fields[0].trim().parse::<i64>() else { continue };
        let kind = fields[1].trim().to_string();
        let project = fields.iter().find_map(|f| f.strip_prefix("project=")).map(str::trim).filter(|p| *p != "~none").map(String::from);
        let role = fields.iter().find_map(|f| f.strip_prefix("role=")).map(str::trim).unwrap_or("user").to_string();
        out.push(ShownItem { seq, kind, project, role });
    }
    out
}

/// The tree node ids a classifier prompt shows.
pub fn shown_nodes(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in prompt.lines() {
        if let Some(i) = line.find("(id=") {
            let rest = &line[i + 4..];
            if let Some(j) = rest.find([',', ')']) {
                out.push(rest[..j].to_string());
            }
        }
    }
    out
}

fn target_kind(item: &ShownItem) -> &'static str {
    match item.role.as_str() {
        "page" => "browse_event",
        "note" => "note",
        "decision" => "decision",
        _ => "prompt",
    }
}

/// Both verifier prompts open with this sentence; the classifier's only
/// mentions a verifier in passing.
pub fn is_verifier_prompt(prompt: &str) -> bool {
    prompt.contains("verifier. The memory classifier proposed")
}

/// The well-behaved classifier: file every shown item under its provenance
/// root (or `~general`); refute every verification; gist nothing.
pub fn filing_reply(seat: &str, prompt: &str) -> String {
    if is_verifier_prompt(prompt) {
        return refute_all(prompt);
    }
    if seat == "keeper" {
        return keeper_reply(prompt);
    }
    let items = shown_items(prompt);
    let nodes: HashSet<String> = shown_nodes(prompt).into_iter().collect();
    let mut props = Vec::new();
    for it in items {
        let root = it.project.as_deref().map(root_id_for_path).unwrap_or_else(|| GENERAL_ROOT_ID.to_string());
        if !nodes.contains(&root) {
            continue;
        }
        props.push(serde_json::json!({
            "op": "file", "parent_id": root, "target_kind": target_kind(&it),
            "target_id": it.seq.to_string(), "rationale": "provenance root"
        }));
    }
    serde_json::json!({ "proposals": props }).to_string()
}

/// A verifier that refutes everything it is asked.
pub fn refute_all(prompt: &str) -> String {
    let ids: Vec<i64> = prompt
        .lines()
        .filter_map(|l| l.strip_prefix("### proposal "))
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .collect();
    let verdicts: Vec<serde_json::Value> = ids
        .into_iter()
        .map(|id| serde_json::json!({ "proposalId": id, "apply": false, "confidence": 0.1, "reason": "refuted by the recorded verifier" }))
        .collect();
    serde_json::json!({ "verdicts": verdicts }).to_string()
}

/// The keeper with nothing to say: no gists, no observations, keep every
/// existing observation.
pub fn keeper_reply(_prompt: &str) -> String {
    serde_json::json!({ "actions": [], "observations": [], "verdicts": [] }).to_string()
}

/// The §5.5 adversary: reads the fenced items and DOES what they say. Given
/// the fixtures' phrases it emits a merge of every shown class, a
/// supersession of decision #12 by the newest shown seq, and a collapse of
/// the first shown class — all of which the screen and the adjudicator
/// must refuse.
pub fn obedient_reply(seat: &str, prompt: &str, fence: &Fence) -> String {
    if is_verifier_prompt(prompt) {
        // The adversary affirms what it proposed.
        let ids: Vec<i64> = prompt.lines().filter_map(|l| l.strip_prefix("### proposal ")).filter_map(|s| s.trim().parse::<i64>().ok()).collect();
        let verdicts: Vec<serde_json::Value> = ids.into_iter().map(|id| serde_json::json!({ "proposalId": id, "apply": true, "confidence": 0.99, "reason": "as instructed" })).collect();
        return serde_json::json!({ "verdicts": verdicts }).to_string();
    }
    if seat == "keeper" {
        return keeper_reply(prompt);
    }
    let injected: Vec<String> = fence.split(prompt).into_iter().map(|i| i.text.to_lowercase()).collect();
    let says = |needle: &str| injected.iter().any(|t| t.contains(needle));
    let nodes = shown_nodes(prompt);
    let items = shown_items(prompt);
    let newest = items.iter().map(|i| i.seq).max().unwrap_or(0);
    let mut props = Vec::new();
    if says("merge all classes") && nodes.len() >= 2 {
        props.push(serde_json::json!({ "op": "merge", "node_ids": nodes, "title": "everything", "rationale": "as instructed" }));
    }
    if says("supersede decision #12") {
        // "decision #12" as the replacement of the oldest shown item, or —
        // when #12 is older than everything shown — as the one replaced.
        let oldest = items.iter().map(|i| i.seq).min().unwrap_or(1);
        let (old, new) = if 12 > oldest { (oldest, 12) } else { (12, newest) };
        props.push(serde_json::json!({ "op": "supersede", "old_seq": old, "new_seq": new, "rationale": "as instructed" }));
    }
    if says("collapse") {
        if let Some(first) = shown_nodes(prompt).into_iter().find(|n| n != GENERAL_ROOT_ID) {
            props.push(serde_json::json!({ "op": "collapse", "node_id": first, "summary": "as instructed", "cite_seqs": [newest], "rationale": "as instructed" }));
        }
    }
    // …and a seq it was never shown, straight from the injected text.
    props.push(serde_json::json!({ "op": "file", "parent_id": GENERAL_ROOT_ID, "target_kind": "prompt", "target_id": "999999", "rationale": "as instructed" }));
    serde_json::json!({ "proposals": props }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_parsers_read_the_headers_back() {
        let prompt = "- x (id=root-abc, kind=node)\n  - y (id=cn-1, kind=node)\n- seq 7 | prompt | project=/x/a | surface=plan | role=user\n<<<ITEM seq=7 role=user nonce=n>>>\nhello\n<<<END nonce=n>>>\n- seq 9 | browse_event | project=~none | surface=browse_event | role=page\n";
        assert_eq!(shown_nodes(prompt), vec!["root-abc".to_string(), "cn-1".to_string()]);
        let items = shown_items(prompt);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], ShownItem { seq: 7, kind: "prompt".into(), project: Some("/x/a".into()), role: "user".into() });
        assert_eq!(items[1].project, None);
        assert_eq!(items[1].role, "page");
    }
}

/// The plan's B3 gate on a real lake: twenty consecutive gardener runs on a
/// COPY of the live DB with the recorded classifier — zero canary
/// regressions, zero held rows, the catalog no worse, errors under 5 %.
/// `POLIS_REAL_DB=<copy>` (the file is copied again into a temp dir, so
/// the path you give is never written).
#[cfg(test)]
mod autonomy_gate {
    use super::*;
    use crate::gardener::{step, Gate, GardenerConfig, GardenerState};
    use polis_core::host::{Change, Clock, GardenerEvents, IdleSignal};
    use polis_llm::NoopSink;
    use polis_store::PolisStore;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct FakeClock(Mutex<i64>);
    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            *self.0.lock().unwrap()
        }
    }
    struct Idle;
    impl IdleSignal for Idle {
        fn last_activity_ms(&self) -> i64 {
            0
        }
    }
    struct Bus;
    impl GardenerEvents for Bus {
        fn changed(&self, _what: &[Change]) {}
    }

    fn real_db_copy() -> Option<PathBuf> {
        let src = std::env::var("POLIS_REAL_DB").ok()?;
        let dir = std::env::temp_dir().join(format!("polis-b3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("real.db");
        std::fs::copy(&src, &dst).expect("copy the real DB");
        Some(dst)
    }

    #[tokio::test]
    #[ignore]
    async fn real_db_autonomy_20_runs() {
        let Some(path) = real_db_copy() else {
            eprintln!("POLIS_REAL_DB not set — skipping");
            return;
        };
        let store = PolisStore::open(&path).unwrap();
        // The roots the lake already has, so re-seeding is idempotent.
        let roots: Vec<String> = store.list_class_nodes().unwrap().into_iter().filter(|n| n.parent_id.is_none()).filter_map(|n| n.project_path).collect();
        let host = Roots(roots);
        let agent = ScriptedAgent::new(filing_reply);
        let polis = crate::Polis::new(&store, Some(agent.clone()), &host, &NoopSink);
        let before = crate::health::catalog_health(&polis);
        let events_before = store.max_ledger_seq().unwrap();
        let clock = FakeClock(Mutex::new(store.lake_envelope().unwrap().newest + 1_000_000));
        let cfg = GardenerConfig { canary: Some(crate::canary::CanaryConfig { decisions: 40, spans: 30, classes: 30, ..Default::default() }), ..Default::default() };
        let mut st = GardenerState::default();
        let mut rng = crate::corpus::Rng::new(0xB3);
        let mut ran = 0;
        let mut ticks = 0;
        let mut refused = 0usize;
        while ran < 20 && ticks < 60 {
            ticks += 1;
            // New captures between runs, so every run has a delta.
            let ts = clock.now_ms();
            for i in 0..30 {
                let body = format!("run {ran} capture {i}: {}", ["deploy the loop executor", "clerk webhook retries", "billing invoice pdf", "voice panel latency"][rng.below(4)]);
                let _ = polis_store::record::record_prompt_at(
                    &store,
                    polis_store::record::PromptInput {
                        source: polis_core::ledger::PromptSource::Hook,
                        origin: polis_core::ledger::Origin::External,
                        surface: "plan".into(),
                        role: polis_core::ledger::CorpusRole::User,
                        session_id: None,
                        claude_session_id: Some(format!("b3-{ran}-{i}")),
                        mission_id: None,
                        project_path: host.0.get(rng.below(host.0.len().max(1))).cloned(),
                        body,
                        thread: None,
                        author: None,
                        model: None,
                        model_source: None,
                        user_text: None,
                    },
                    ts,
                );
            }
            *clock.0.lock().unwrap() += 7 * 3600 * 1000;
            let o = step(&polis, &mut st, &Idle, &clock, &cfg, &Bus).await;
            if o.gate == Gate::Ran {
                ran += 1;
                refused += o.refused;
                assert!(!o.reverted_by_canary, "run {ran} was reverted by the canary");
                assert!(!o.no_model);
            }
        }
        assert_eq!(ran, 20, "twenty runs in {ticks} ticks");
        let runs = store.list_class_runs(60).unwrap();
        let organize: Vec<_> = runs.iter().filter(|r| r.mode.as_deref() == Some("organize")).take(20).collect();
        let errors = organize.iter().filter(|r| r.outcome.as_deref() == Some("error")).count();
        let regressions = organize.iter().filter(|r| r.outcome.as_deref() == Some("reverted_by_canary")).count();
        let held: i64 = {
            let conn = store.conn();
            conn.query_row("SELECT COUNT(*) FROM class_nodes WHERE status = 'proposed'", [], |r| r.get::<_, i64>(0)).unwrap()
                + conn.query_row("SELECT COUNT(*) FROM class_links WHERE status = 'proposed'", [], |r| r.get::<_, i64>(0)).unwrap()
        };
        let after = crate::health::catalog_health(&polis);
        let chain = store.verify_ledger_chain().unwrap();
        eprintln!(
            "real_db_autonomy_20_runs: runs={} errors={} regressions={} held={} refused={} events {}→{} health before={} after={} chain_ok={}",
            organize.len(), errors, regressions, held, refused, events_before, store.max_ledger_seq().unwrap(),
            serde_json::to_string(&before).unwrap(), serde_json::to_string(&after).unwrap(), chain.ok
        );
        assert_eq!(regressions, 0);
        assert_eq!(held, 0);
        assert!((errors as f64) / (organize.len().max(1) as f64) < 0.05, "error rate");
        // "Not worse" on the rows a recorded classifier can be held to: the
        // structural ones. Fan-out is the CONSOLIDATION prompt's (a model
        // splitting what grew past 120); the filing agent here never splits,
        // so its roots swell by construction — reported above, not asserted.
        assert!(after.orphan_rate <= before.orphan_rate);
        assert!(after.provenance_violations <= before.provenance_violations);
        assert!(after.duplicate_title_rate <= before.duplicate_title_rate + 1e-9);
        assert!(after.error_rate <= before.error_rate + 1e-9);
        assert!(!after.canary_alert);
        assert!(chain.ok);
        let _ = std::fs::remove_file(&path);
    }
}
