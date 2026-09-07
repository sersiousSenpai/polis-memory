// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The latency benches (docs/bench.md, plan §6.1): ingest, the answer pack
//! cold and warm, grep, the context block, and semantic search — over the
//! seeded synthetic corpus at the sizes the budget names.
//!
//!   cargo bench -p polis-memory                       # 1k (default)
//!   POLIS_BENCH_PROMPTS=10000 cargo bench -p polis-memory
//!   POLIS_BENCH_PROMPTS=100000 POLIS_BENCH_SCALE=0.1 cargo bench -p polis-memory
//!
//! The semantic arm runs on a deterministic bag-of-words embedder (CI has no
//! on-device model) — it measures the vector path's cost, not any model's
//! quality. Every corpus is written to a temp FILE (WAL), not memory, so the
//! numbers include the page cache a real install has.

use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

use polis_core::api::{IngestItem, IngestRequest};
use polis_core::host::NoHost;
use polis_core::MemoryApi;
use polis_memory::corpus::{seed_corpus, CorpusSpec, Rng};
use polis_memory::polis_embed::Embedder;
use polis_memory::polis_llm::NoopSink;
use polis_memory::polis_store::PolisStore;
use polis_memory::retrieval::{build_answer_pack, context_block};
use polis_memory::{Polis, PolisHandle};

/// A deterministic hashed bag-of-words embedder: 64 dims, one bucket per
/// token hash, L2-normalized. Cheap, stable, and no model download.
struct BagOfWords;

impl Embedder for BagOfWords {
    fn model_id(&self) -> String {
        "bench-bag-of-words-64".to_string()
    }
    fn dim(&self) -> usize {
        64
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut v = vec![0f32; 64];
                for tok in t.split_whitespace() {
                    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                    for b in tok.bytes() {
                        h ^= b as u64;
                        h = h.wrapping_mul(0x100_0000_01b3);
                    }
                    v[(h % 64) as usize] += 1.0;
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
}

struct Lake {
    _dir: std::path::PathBuf,
    store: Arc<PolisStore>,
    queries: Vec<String>,
    needles: Vec<String>,
}

fn prompts() -> usize {
    std::env::var("POLIS_BENCH_PROMPTS").ok().and_then(|s| s.parse().ok()).unwrap_or(1_000)
}

fn scale() -> f64 {
    std::env::var("POLIS_BENCH_SCALE").ok().and_then(|s| s.parse().ok()).unwrap_or(1.0)
}

fn lake(n: usize) -> Lake {
    let dir = std::env::temp_dir().join(format!("polis-bench-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(PolisStore::open(&dir.join("bench.db")).unwrap());
    let report = seed_corpus(&store, &CorpusSpec::new(n).with_machine_body_scale(scale())).unwrap();
    eprintln!("corpus: {}", serde_json::to_string(&report).unwrap());
    // Queries: spans of recent user prompts (what the canary asks).
    let mut rng = Rng::new(99);
    let queries: Vec<String> = store
        .recent_user_prompts(200, 40)
        .unwrap()
        .into_iter()
        .map(|(_, _, body)| {
            let toks: Vec<&str> = body.split_whitespace().collect();
            let len = (8 + rng.below(5)).min(toks.len());
            let start = rng.below(toks.len() - len + 1);
            toks[start..start + len].join(" ")
        })
        .collect();
    // Needles: identifiers the corpus planted.
    let needles: Vec<String> = ["ERR_POLIS", "src/keeper", "--redline-limit", "ERR_TILE", "src/voice", "--drafter-mode"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    Lake { _dir: dir, store, queries, needles }
}

fn bench_all(c: &mut Criterion) {
    let n = prompts();
    let lake = lake(n);
    let tag = format!("{n}");
    let store = lake.store.clone();
    let handle = PolisHandle::new(store.clone(), None, Arc::new(NoHost), Arc::new(NoopSink));

    // answer pack, cold: a fresh connection per iteration (no page cache
    // for this process, no prepared statements).
    let path = lake._dir.join("bench.db");
    let mut qi = 0usize;
    c.bench_function(&format!("pack_cold/{tag}"), |b| {
        b.iter_batched(
            || {
                qi += 1;
                (PolisStore::open(&path).unwrap(), lake.queries[qi % lake.queries.len()].clone())
            },
            |(fresh, q)| {
                let polis = Polis::new(&fresh, None, &NoHost, &NoopSink);
                build_answer_pack(&polis, Some(&q), None, 8)
            },
            BatchSize::PerIteration,
        )
    });

    // answer pack, warm: the long-lived store.
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let mut qi = 0usize;
    c.bench_function(&format!("pack_warm/{tag}"), |b| {
        b.iter(|| {
            qi += 1;
            build_answer_pack(&polis, Some(&lake.queries[qi % lake.queries.len()]), None, 8)
        })
    });

    let mut ni = 0usize;
    c.bench_function(&format!("grep/{tag}"), |b| {
        b.iter(|| {
            ni += 1;
            store
                .grep_memory(&lake.needles[ni % lake.needles.len()], None, false, polis_core::types::GrepScope::All, 20)
                .unwrap()
        })
    });

    let mut qi = 0usize;
    c.bench_function(&format!("context/{tag}"), |b| {
        b.iter(|| {
            qi += 1;
            context_block(&polis, &lake.queries[qi % lake.queries.len()], None, 8_000)
        })
    });

    // semantic: index everything with the bag-of-words embedder first, then
    // time the search alone.
    let embedder: Arc<dyn Embedder> = Arc::new(BagOfWords);
    let polis_sem = Polis::new(&store, None, &NoHost, &NoopSink).with_embedder(Some(embedder.clone()));
    // Bounded: at 100k the backlog query (a correlated NOT EXISTS over the
    // growing embeddings table) makes full indexing a matter of hours, so
    // the bench indexes for at most POLIS_BENCH_INDEX_SECS (60) and says how
    // far it got — the search below runs over that many targets.
    let index_budget = Duration::from_secs(
        std::env::var("POLIS_BENCH_INDEX_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(60),
    );
    let index_started = std::time::Instant::now();
    let mut indexed = 0usize;
    loop {
        let done = polis_memory::index_tick(&polis_sem, 2_000);
        indexed += done;
        if done == 0 || index_started.elapsed() > index_budget {
            break;
        }
    }
    eprintln!(
        "semantic index: {indexed} targets in {:.1}s{}",
        index_started.elapsed().as_secs_f64(),
        if index_started.elapsed() > index_budget { " (budget hit; partial index)" } else { "" }
    );
    let mut qi = 0usize;
    c.bench_function(&format!("semantic/{tag}"), |b| {
        b.iter(|| {
            qi += 1;
            polis_memory::semantic_search(&polis_sem, &lake.queries[qi % lake.queries.len()], 24)
        })
    });
    let mut qi = 0usize;
    c.bench_function(&format!("pack_warm_fused/{tag}"), |b| {
        b.iter(|| {
            qi += 1;
            build_answer_pack(&polis_sem, Some(&lake.queries[qi % lake.queries.len()]), None, 8)
        })
    });

    // ingest: one item per iteration through the API (the hook's path).
    // LAST: criterion runs this tens of thousands of times, and every one
    // is a row in the lake the read benches above must not have seen.
    let mut i = 0u64;
    c.bench_function(&format!("ingest/{tag}"), |b| {
        b.iter_batched(
            || {
                i += 1;
                IngestRequest { items: vec![IngestItem { body: format!("bench ingest item {i} keeper ledger tile budget"), ..Default::default() }], ..Default::default() }
            },
            |req| handle.ingest(&req).unwrap(),
            BatchSize::SmallInput,
        )
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().measurement_time(Duration::from_secs(8)).warm_up_time(Duration::from_secs(2)).sample_size(30);
    targets = bench_all
}
criterion_main!(benches);
