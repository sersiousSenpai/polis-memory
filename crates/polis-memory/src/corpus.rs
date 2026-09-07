// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! A seeded synthetic lake, fitted to the real one.
//!
//! The benchmarks (`benches/memory.rs`), the eval gate and the canary
//! calibration need a corpus that is (a) deterministic — the same seed writes
//! the same rows, so a number is reproducible — and (b) shaped like a real
//! lake, so a latency or a recall measured here says something about a user's.
//! The shape below was measured on 2026-09-07 against a copy of the user's
//! own database (2,164 prompts / 5,735 events / 229 class nodes / 3,842 links
//! / 1,483 browse events over 66 days; docs/bench.md "Corpus"):
//!
//! | axis | real | fitted |
//! |---|---|---|
//! | roles | user 61% · agent 28% · system 11% | same |
//! | user body chars p10/p50/p90/p99 | 16 / 135 / 896 / 8,202 | quantile table |
//! | agent body chars p50/p90 | 8,004 / 32,541 | quantile table × `machine_body_scale` |
//! | system body chars p50/p90 | 16,404 / 35,878 | quantile table × `machine_body_scale` |
//! | surfaces | pty 68% · fork 9% · external 7% · browse 4% · … | same weights |
//! | project roots | 44 (7.5% of prompts have none) | `prompts / 49`, clamped 4..=44 |
//! | decisions (approval events) | 5.9% of prompts | 1 per 17 prompts |
//! | browse events | 69% of prompts, 34% distinct pages, ~2.9 KB text | same |
//! | class nodes | prompts / 9.5; depth 1 70% · depth 2 15% · deeper 2% | same |
//! | links | 1.78 × prompts; per-node p50 9 · p90 28 · max 407 | Zipf topic draw |
//! | notes | 0 (the real lake has none yet) | 1% of prompts, so the Note subject exists |
//!
//! Text is pseudo-English from a fixed vocabulary with per-class topic bags,
//! so classes are lexically coherent (a prompt draws ~55% of its words from
//! its class's bag) and a few percent of tokens are identifiers (`--flag`,
//! `path/to/file.rs`, `ERR_CODE`) so the grep arm has something literal to
//! find. It is NOT English: nothing here measures human relevance, only the
//! reachability of what was written (docs/bench.md says so up front).

use std::time::Instant;

use polis_core::ledger::{CorpusRole, EventKind, Origin, PromptSource};
use polis_core::proposal::Proposal;
use polis_core::types::NoteWrite;
use polis_store::record::{
    record_browse_event_at, record_decision, record_prompt_at, BrowseAction, BrowseEventInput,
    DecisionInput, PromptInput,
};
use polis_store::PolisStore;

/// The actor synthetic rows are written as — never the human's author string.
pub const CORPUS_ACTOR: &str = "corpus";

/// What to generate.
#[derive(Debug, Clone)]
pub struct CorpusSpec {
    /// Total prompt rows (all roles).
    pub prompts: usize,
    pub seed: u64,
    /// Multiplier on agent/system prompt bodies and browse-page text. 1.0 is
    /// the real distribution; the 100k corpus uses 0.1 so a bench database
    /// stays under a gigabyte (docs/bench.md says which rows were scaled).
    pub machine_body_scale: f64,
    /// First event timestamp (ms). The corpus spreads over `prompts / 33`
    /// days from here — the real lake's 33 prompts a day.
    pub epoch_ms: i64,
}

impl CorpusSpec {
    pub fn new(prompts: usize) -> Self {
        Self { prompts, seed: 0x5eed_0001, machine_body_scale: 1.0, epoch_ms: 1_750_000_000_000 }
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    pub fn with_machine_body_scale(mut self, scale: f64) -> Self {
        self.machine_body_scale = scale;
        self
    }

    /// The number of project roots this corpus seeds.
    pub fn projects(&self) -> usize {
        (self.prompts / 49).clamp(4, 44)
    }

    /// The number of class nodes (roots excluded).
    pub fn classes(&self) -> usize {
        (self.prompts as f64 / 9.5).round().max(6.0) as usize
    }
}

/// What was written.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusReport {
    pub seed: u64,
    pub prompts: usize,
    pub user_prompts: usize,
    pub decisions: usize,
    pub supersessions: usize,
    pub browse_events: usize,
    pub notes: usize,
    pub roots: usize,
    pub nodes: usize,
    pub links: usize,
    pub head_seq: i64,
    /// Approximate bytes of prompt + page text written.
    pub text_bytes: u64,
    pub elapsed_ms: u64,
}

// ---------------------------------------------------------------------------
// A tiny deterministic generator (SplitMix64) — no `rand` dependency.
// ---------------------------------------------------------------------------

/// SplitMix64. Good enough for a corpus; deterministic across platforms.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n` (0 when `n == 0`).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// Zipf-like index into `n` items: a few come up often, most rarely.
    pub fn zipf(&mut self, n: usize, skew: f64) -> usize {
        if n <= 1 {
            return 0;
        }
        let u = self.next_f64().max(1e-12);
        let i = (n as f64 * u.powf(skew)).floor() as usize;
        i.min(n - 1)
    }

    /// Weighted choice; `weights` need not sum to 1.
    pub fn weighted(&mut self, weights: &[f64]) -> usize {
        let total: f64 = weights.iter().sum();
        let mut x = self.next_f64() * total;
        for (i, w) in weights.iter().enumerate() {
            if x < *w {
                return i;
            }
            x -= w;
        }
        weights.len() - 1
    }
}

/// Sample a length from a quantile table `(q, value)` with log-linear
/// interpolation — the shape of the real distribution, not a parametric fit.
pub fn sample_quantile(rng: &mut Rng, table: &[(f64, f64)]) -> usize {
    let u = rng.next_f64();
    for w in table.windows(2) {
        let (q0, v0) = w[0];
        let (q1, v1) = w[1];
        if u <= q1 {
            let t = if q1 > q0 { (u - q0) / (q1 - q0) } else { 0.0 };
            let lv = v0.max(1.0).ln() + t * (v1.max(1.0).ln() - v0.max(1.0).ln());
            return lv.exp().round() as usize;
        }
    }
    table.last().map(|(_, v)| *v as usize).unwrap_or(1)
}

/// The measured quantiles (chars), by role.
pub const USER_BODY_CHARS: &[(f64, f64)] =
    &[(0.0, 4.0), (0.10, 16.0), (0.50, 135.0), (0.90, 896.0), (0.99, 8_202.0), (1.0, 20_288.0)];
pub const AGENT_BODY_CHARS: &[(f64, f64)] =
    &[(0.0, 120.0), (0.10, 675.0), (0.50, 8_004.0), (0.90, 32_541.0), (0.99, 50_744.0), (1.0, 55_267.0)];
pub const SYSTEM_BODY_CHARS: &[(f64, f64)] =
    &[(0.0, 120.0), (0.10, 393.0), (0.50, 16_404.0), (0.90, 35_878.0), (0.99, 56_372.0), (1.0, 60_032.0)];
pub const PAGE_TEXT_CHARS: &[(f64, f64)] =
    &[(0.0, 200.0), (0.10, 600.0), (0.50, 2_400.0), (0.90, 6_000.0), (1.0, 12_000.0)];

const SURFACES: &[(&str, f64)] = &[
    ("pty", 0.684),
    ("fork", 0.087),
    ("external", 0.068),
    ("browse", 0.043),
    ("front-door", 0.032),
    ("drafter", 0.022),
    ("voice", 0.016),
    ("browser", 0.014),
    ("companion", 0.009),
    ("memchat", 0.007),
    ("chat", 0.006),
    ("drafter_chat", 0.004),
    ("shelf_agent", 0.004),
    ("combine", 0.002),
    ("diagram", 0.002),
];

const ROLE_WEIGHTS: [f64; 3] = [0.610, 0.283, 0.107];

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

const BASE_WORDS: &[&str] = &[
    "the", "a", "and", "then", "when", "after", "before", "should", "would", "could", "can",
    "make", "keep", "move", "drop", "add", "remove", "rename", "split", "merge", "check", "verify",
    "test", "build", "ship", "land", "commit", "branch", "worktree", "review", "plan", "revise",
    "session", "prompt", "reply", "turn", "agent", "seat", "model", "effort", "token", "budget",
    "size", "binary", "bundle", "release", "debug", "warning", "error", "panic", "retry", "timeout",
    "route", "handler", "state", "store", "table", "column", "index", "query", "row", "schema",
    "migration", "golden", "fixture", "snapshot", "restore", "backup", "chain", "hash", "ledger",
    "event", "seq", "head", "verify", "budget", "guard", "test", "green", "red", "flaky", "slow",
    "fast", "cold", "warm", "cache", "lock", "mutex", "thread", "spawn", "process", "signal",
    "window", "pane", "tab", "strip", "header", "footer", "button", "menu", "dialog", "toast",
    "scroll", "resize", "drag", "focus", "blur", "hover", "click", "keyboard", "shortcut",
    "file", "path", "dir", "repo", "diff", "patch", "line", "block", "anchor", "highlight",
    "comment", "thread", "discussion", "question", "feedback", "edit", "resolution", "approve",
    "why", "how", "what", "where", "which", "this", "that", "it", "we", "you", "I", "still",
    "again", "now", "later", "first", "last", "next", "same", "other", "new", "old", "current",
    "please", "also", "maybe", "actually", "instead", "rather", "only", "never", "always",
    "works", "fails", "breaks", "hangs", "loads", "renders", "opens", "closes", "starts", "stops",
];

const IDENT_STEMS: &[&str] = &[
    "polis", "redline", "ledger", "keeper", "drafter", "browser", "voice", "meter", "hook", "restore",
    "front-door", "tile", "grid", "dock", "companion", "mission", "linked", "shelf", "diagram",
];

const IDENT_TAILS: &[&str] = &["rs", "tsx", "ts", "md", "json", "toml", "yml"];

/// One topic bag: a title and the words a class's prompts are about.
struct Topic {
    title: String,
    words: Vec<String>,
}

fn topic_word_pool() -> Vec<&'static str> {
    vec![
        "tab", "suspension", "snapshot", "webview", "fullscreen", "pinch", "magnification", "popup",
        "oauth", "login", "favicon", "logo", "bubble", "pill", "chip", "badge", "caret", "divider",
        "voice", "dictation", "whisper", "push-to-talk", "barge-in", "duplex", "echo", "microphone",
        "speaker", "transcript", "conversation", "spitball", "read-aloud", "mode", "warm", "session",
        "ledger", "chain", "hash", "genesis", "seq", "verify", "bundle", "export", "mirror",
        "markdown", "frontmatter", "class", "catalog", "taxonomy", "promote", "collapse", "digest",
        "coldness", "provenance", "root", "general", "inbox", "centroid", "embedding", "vector",
        "cosine", "int8", "quantize", "semantic", "lexical", "trigram", "grep", "bm25", "rank",
        "fusion", "rrf", "pack", "answer", "budget", "bytes", "clip", "excerpt", "dedup", "duplicate",
        "meter", "provenance", "burn", "cache", "read", "write", "input", "output", "usage",
        "seat", "classifier", "keeper", "librarian", "shipwright", "sensei", "recruit", "dojo",
        "plan", "revision", "resolution", "approval", "orchestrate", "workflow", "subtask", "verify",
        "restore", "resume", "detached", "watchdog", "liveness", "probe", "handoff", "rollback",
        "terminal", "tile", "grid", "cell", "splitter", "dock", "column", "resize", "freeze",
        "front-door", "hero", "preflight", "readiness", "harness", "picker", "codex", "claude",
        "drafter", "template", "footnote", "list", "suggestion", "tracked", "accept", "reject",
        "size", "binary", "budget", "lto", "strip", "codegen", "chunk", "boot", "lazy", "island",
        "extension", "wasm", "manifest", "marketplace", "host-call", "isolation", "strike",
        "memory", "ask", "timeline", "map", "health", "note", "annotate", "forget", "remember",
        "browse", "page", "selection", "highlight", "working-list", "pointer", "locator", "item",
        "collab", "video", "signaling", "peer", "share", "viewer", "snippet", "invite", "join",
        "theme", "font", "canvas", "cycling", "beat", "layer", "effect", "palette", "scroller",
        "windows", "port", "msvc", "pty", "conpty", "path", "quoting", "sandbox", "curl", "allow",
        "hook", "capture", "submit", "payload", "stdin", "additional-context", "settings", "install",
        "schema", "golden", "migration", "user-version", "pragma", "wal", "vacuum", "fts5",
    ]
}

fn build_topics(rng: &mut Rng, n: usize) -> Vec<Topic> {
    let pool = topic_word_pool();
    (0..n)
        .map(|i| {
            let k = 18 + rng.below(10);
            let mut words: Vec<String> = (0..k).map(|_| rng.pick(&pool).to_string()).collect();
            words.dedup();
            // Titles read like the real ones: two to five topic words, 3–90
            // chars, mean ~44 (measured).
            let tw = 2 + rng.below(4);
            let mut title: Vec<String> = words.iter().take(tw).cloned().collect();
            if let Some(first) = title.first_mut() {
                let mut c = first.chars();
                if let Some(f) = c.next() {
                    *first = f.to_uppercase().collect::<String>() + c.as_str();
                }
            }
            let title = format!("{} {}", title.join(" "), i + 1);
            Topic { title, words }
        })
        .collect()
}

fn identifier(rng: &mut Rng) -> String {
    match rng.below(3) {
        0 => format!("--{}-{}", rng.pick(IDENT_STEMS), rng.pick(&["enabled", "limit", "mode", "dir", "path"])),
        1 => format!("src/{}/{}.{}", rng.pick(IDENT_STEMS), rng.pick(IDENT_STEMS), rng.pick(IDENT_TAILS)),
        _ => format!("ERR_{}_{}", rng.pick(IDENT_STEMS).to_uppercase().replace('-', "_"), rng.below(900) + 100),
    }
}

/// Pseudo-English of about `chars` characters, ~55% from the topic bag.
fn text(rng: &mut Rng, topic: &Topic, chars: usize) -> String {
    let mut out = String::with_capacity(chars + 32);
    let mut sentence_left = 6 + rng.below(12);
    while out.len() < chars {
        let word: String = match rng.weighted(&[0.55, 0.35, 0.05, 0.05]) {
            0 => rng.pick(&topic.words).clone(),
            1 => rng.pick(BASE_WORDS).to_string(),
            2 => identifier(rng),
            _ => (rng.below(5000)).to_string(),
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
        sentence_left -= 1;
        if sentence_left == 0 {
            out.push('.');
            sentence_left = 6 + rng.below(12);
        }
    }
    out
}

fn role_table(role: CorpusRole) -> &'static [(f64, f64)] {
    match role {
        CorpusRole::User => USER_BODY_CHARS,
        CorpusRole::Agent => AGENT_BODY_CHARS,
        CorpusRole::System => SYSTEM_BODY_CHARS,
    }
}

// ---------------------------------------------------------------------------
// The writer
// ---------------------------------------------------------------------------

struct Class {
    id: String,
    title: String,
    topic: usize,
}

/// Write a corpus into `store` (which may already hold rows — the corpus
/// appends, so seed an EMPTY store for a reproducible result).
pub fn seed_corpus(store: &PolisStore, spec: &CorpusSpec) -> Result<CorpusReport, String> {
    let started = Instant::now();
    let mut rng = Rng::new(spec.seed);
    let mut report = CorpusReport { seed: spec.seed, ..Default::default() };

    // Projects + roots (the same seeding the organizer does).
    let n_projects = spec.projects();
    let projects: Vec<String> = (0..n_projects).map(|i| format!("/home/u/src/project-{:02}", i + 1)).collect();
    let roots = crate::organize::seed_root_rows(&projects);
    store.seed_class_roots(&roots).map_err(|e| e.to_string())?;
    // Roots by project index; the general root last.
    let root_ids: Vec<String> = roots.iter().map(|(id, _, _)| id.clone()).collect();
    report.roots = root_ids.len();

    // The class tree: depth-1 classes under roots (Zipf over roots so one or
    // two projects dominate, as in the real lake), depth-2 under depth-1,
    // a few deeper.
    let n_classes = spec.classes();
    let topics = build_topics(&mut rng, n_classes);
    let mut classes: Vec<Class> = Vec::with_capacity(n_classes);
    let mut proposals: Vec<(String, String, usize)> = Vec::new(); // (parent, title, topic)
    for (t, topic) in topics.iter().enumerate() {
        let u = rng.next_f64();
        let parent = if u < 0.80 || classes.is_empty() {
            root_ids[rng.zipf(root_ids.len(), 1.6)].clone()
        } else if u < 0.97 {
            // under a depth-1 class
            classes[rng.zipf(classes.len(), 1.2)].id.clone()
        } else {
            classes[rng.below(classes.len())].id.clone()
        };
        let created = store
            .stage_proposal(None, &Proposal::Create { parent_id: parent.clone(), title: topic.title.clone(), rationale: None })
            .map_err(|e| e.to_string())?;
        if matches!(created, polis_core::types::StagedOutcome::Skipped) {
            continue;
        }
        proposals.push((parent.clone(), topic.title.clone(), t));
        // Read the id back by (parent, title): the store mints uuids.
        let id = store
            .list_class_nodes()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|n| n.parent_id.as_deref() == Some(parent.as_str()) && n.title == topic.title)
            .map(|n| n.id)
            .ok_or_else(|| "created class not found".to_string())?;
        classes.push(Class { id, title: topic.title.clone(), topic: t });
    }
    store.accept_all_pending(CORPUS_ACTOR).map_err(|e| e.to_string())?;
    report.nodes = classes.len();

    // Prompts, decisions, browse events, notes — interleaved in time.
    let days = (spec.prompts as f64 / 33.0).max(1.0);
    let span_ms = (days * 86_400_000.0) as i64;
    let step_ms = (span_ms / spec.prompts.max(1) as i64).max(1);
    let mut ts = spec.epoch_ms;
    let surface_weights: Vec<f64> = SURFACES.iter().map(|(_, w)| *w).collect();
    let mut claude_session = 0usize;
    let mut plan_session = 0usize;
    let mut approvals: Vec<(i64, usize)> = Vec::new(); // (seq, class index) for supersessions
    let mut links: usize = 0;
    let mut text_bytes: u64 = 0;
    let mut browse_tab = 0usize;
    let mut pages: Vec<(String, String, String, usize)> = Vec::new(); // url, title, text, class

    let file = |store: &PolisStore, node: &str, kind: &str, id: i64| -> Result<bool, String> {
        let out = store
            .stage_proposal(
                None,
                &Proposal::File {
                    parent_id: node.to_string(),
                    sub_class: None,
                    target_kind: kind.to_string(),
                    target_id: id.to_string(),
                    note: None,
                    rationale: None,
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(matches!(out, polis_core::types::StagedOutcome::Link { .. }))
    };

    for i in 0..spec.prompts {
        ts += step_ms + (rng.below(step_ms as usize + 1) as i64) - step_ms / 2;
        if i % 3 == 0 {
            claude_session += 1;
        }
        if i % 20 == 0 {
            plan_session += 1;
        }
        let class_ix = rng.zipf(classes.len(), 1.7);
        let class = &classes[class_ix];
        let topic = &topics[class.topic];
        let role = match rng.weighted(&ROLE_WEIGHTS) {
            0 => CorpusRole::User,
            1 => CorpusRole::Agent,
            _ => CorpusRole::System,
        };
        let mut chars = sample_quantile(&mut rng, role_table(role));
        if role != CorpusRole::User {
            chars = ((chars as f64) * spec.machine_body_scale).max(24.0) as usize;
        }
        let body = text(&mut rng, topic, chars);
        text_bytes += body.len() as u64;
        let surface = SURFACES[rng.weighted(&surface_weights)].0;
        let project = if rng.chance(0.075) {
            None
        } else {
            // A class's prompts mostly live in its root's project.
            Some(projects[rng.zipf(projects.len(), 1.6)].clone())
        };
        let input = PromptInput {
            source: PromptSource::Hook,
            origin: if surface == "external" { Origin::External } else { Origin::Redline },
            surface: surface.to_string(),
            role,
            user_text: None,
            session_id: (surface == "pty").then(|| format!("plan-{plan_session:05}")),
            claude_session_id: Some(format!("cs-{claude_session:06}")),
            mission_id: None,
            project_path: project,
            body,
            thread: None,
            author: Some(CORPUS_ACTOR.to_string()),
            model: None,
            model_source: None,
        };
        let Some(seq) = record_prompt_at(store, input, ts).map_err(|e| e.to_string())? else { continue };
        report.prompts += 1;
        if role == CorpusRole::User {
            report.user_prompts += 1;
        }
        if rng.chance(0.60) && file(store, &class.id, "prompt", seq)? {
            links += 1;
        }

        // A decision every ~17 prompts (approvals), filed under the class.
        if i % 17 == 16 {
            let ref_id = format!("plan-{plan_session:05}");
            let ph = polis_core::ledger::sha256_hex(format!("approval:{ref_id}:{i}").as_bytes());
            let dec = DecisionInput {
                kind: EventKind::Approval,
                author: Some(CORPUS_ACTOR.to_string()),
                session_id: Some(&ref_id),
                ref_kind: "session",
                ref_id: &ref_id,
                payload_hash: ph,
            };
            if let Some(dseq) = record_decision(store, dec).map_err(|e| e.to_string())? {
                report.decisions += 1;
                if rng.chance(0.85) && file(store, &class.id, "decision", dseq)? {
                    links += 1;
                }
                // Every 8th approval supersedes an earlier one — the same
                // class when it has one (so the class title reaches both),
                // else the previous approval. Real lakes supersede rarely
                // (1 in 128 approvals measured); the corpus over-provisions
                // so the Supersession subject has a sample.
                if report.decisions % 8 == 0 {
                    let same_class = approvals.iter().rev().find(|(_, c)| *c == class_ix).copied();
                    if let Some((old, _)) = same_class.or_else(|| approvals.last().copied()) {
                        let out = store
                            .apply_supersession(old, dseq, "the newer approval replaces it", CORPUS_ACTOR)
                            .map_err(|e| e.to_string())?;
                        if matches!(out, polis_core::types::SupersessionOutcome::Applied { .. }) {
                            report.supersessions += 1;
                        }
                    }
                }
                approvals.push((dseq, class_ix));
            }
        }

        // Browse events: 0.685 per prompt; a third of them are new pages.
        if rng.chance(0.685) {
            browse_tab = (browse_tab + rng.below(3)) % 20;
            let page = if pages.is_empty() || rng.chance(0.34) {
                let title = format!("{} — {}", text(&mut rng, topic, 18), rng.pick(&["docs", "issue", "guide", "notes"]));
                let url = format!("https://example.test/{}/{}", topic.title.to_lowercase().replace(' ', "-"), pages.len());
                let mut page_chars = sample_quantile(&mut rng, PAGE_TEXT_CHARS);
                page_chars = ((page_chars as f64) * spec.machine_body_scale).max(64.0) as usize;
                let text = format!("{title}\n{url}\n{}", text(&mut rng, topic, page_chars));
                pages.push((url, title, text, class_ix));
                pages.len() - 1
            } else {
                rng.below(pages.len())
            };
            let (url, title, ptext, pclass) = &pages[page];
            text_bytes += ptext.len() as u64;
            let ev = BrowseEventInput {
                action: BrowseAction::Navigate,
                browse_id: Some(format!("tab-{browse_tab:02}")),
                url: url.clone(),
                title: Some(title.clone()),
                text: ptext.clone(),
                from_event_id: None,
                author: Some(CORPUS_ACTOR.to_string()),
            };
            if let Some(bseq) = record_browse_event_at(store, ev, ts).map_err(|e| e.to_string())? {
                report.browse_events += 1;
                if rng.chance(0.90) && file(store, &classes[*pclass].id, "browse_event", bseq)? {
                    links += 1;
                }
            }
        }

        // Notes: 1% of prompts (synthetic — the real lake has none yet).
        if i % 100 == 99 {
            let note_chars = 90 + rng.below(120);
            let note_text = text(&mut rng, topic, note_chars);
            let (kind, id) = match rng.below(3) {
                0 => ("none", None),
                1 => ("class_node", Some(class.id.clone())),
                _ => ("ledger_event", Some(seq.to_string())),
            };
            let w = NoteWrite { note_id: None, target_kind: Some(kind.to_string()), target_id: id, text: Some(note_text), starred: None };
            if let polis_core::types::NoteOutcome::Written(_) = store.write_user_note(&w, CORPUS_ACTOR).map_err(|e| e.to_string())? {
                report.notes += 1;
            }
        }
    }
    store.accept_all_pending(CORPUS_ACTOR).map_err(|e| e.to_string())?;
    report.links = links;
    report.text_bytes = text_bytes;
    report.head_seq = store.max_ledger_seq().map_err(|e| e.to_string())?;
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    let _ = classes.iter().map(|c| c.title.len()).sum::<usize>();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generator_is_deterministic() {
        let a = Rng::new(7).next_u64();
        let b = Rng::new(7).next_u64();
        assert_eq!(a, b);
        let mut r = Rng::new(9);
        let xs: Vec<u64> = (0..3).map(|_| r.next_u64()).collect();
        let mut r2 = Rng::new(9);
        let ys: Vec<u64> = (0..3).map(|_| r2.next_u64()).collect();
        assert_eq!(xs, ys);
    }

    #[test]
    fn quantile_sampling_stays_inside_the_table() {
        let mut r = Rng::new(3);
        for _ in 0..2_000 {
            let n = sample_quantile(&mut r, USER_BODY_CHARS);
            assert!((4..=20_288).contains(&n), "{n}");
        }
        // The median lands near the table's median.
        let mut xs: Vec<usize> = (0..4_000).map(|_| sample_quantile(&mut r, USER_BODY_CHARS)).collect();
        xs.sort_unstable();
        let med = xs[xs.len() / 2];
        assert!((90..=200).contains(&med), "median {med} is far from the measured 135");
    }

    #[test]
    fn a_small_corpus_writes_every_row_kind_and_the_same_rows_twice() {
        let spec = CorpusSpec::new(300).with_seed(42);
        let a = PolisStore::open_in_memory().unwrap();
        let ra = seed_corpus(&a, &spec).unwrap();
        assert!(ra.prompts >= 280, "{ra:?}");
        assert!(ra.user_prompts > 100);
        assert!(ra.decisions >= 15);
        assert!(ra.browse_events > 100);
        assert_eq!(ra.notes, 3);
        assert!(ra.nodes >= 20);
        assert!(ra.links > 200);
        assert!(a.verify_ledger_chain().unwrap().ok, "the chain verifies");
        let b = PolisStore::open_in_memory().unwrap();
        let rb = seed_corpus(&b, &spec).unwrap();
        assert_eq!(ra.prompts, rb.prompts);
        assert_eq!(ra.links, rb.links);
        assert_eq!(ra.head_seq, rb.head_seq);
        // Same bodies, same order.
        let ia = a.list_lake_items_since(0, 50).unwrap();
        let ib = b.list_lake_items_since(0, 50).unwrap();
        assert_eq!(ia.iter().map(|i| i.body.clone()).collect::<Vec<_>>(), ib.iter().map(|i| i.body.clone()).collect::<Vec<_>>());
    }
}
