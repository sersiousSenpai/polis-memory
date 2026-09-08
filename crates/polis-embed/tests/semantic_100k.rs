// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The §6.1 row "semantic search alone, 100k chunks: p50 < 60 ms, p95 <
//! 120 ms", measured directly: 100,000 stored int8 vectors, a fixed query
//! vector, brute force over all of them (the crossover the crate's ceiling
//! constant documents). Stores nothing but vectors, so it needs no
//! embedder and runs anywhere. The assertion is armed by `POLIS_BENCH_100K=1`
//! (CI's Ubuntu job); without it the numbers are printed and the budget
//! is not enforced, because a loaded laptop is not a benchmark machine.

use std::sync::Arc;
use std::time::Instant;

use polis_embed::{quantize, Embedder};
use polis_store::PolisStore;

/// Any text → the same unit vector (the query side of the measurement).
struct Fixed(Vec<f32>);
impl Embedder for Fixed {
    fn model_id(&self) -> String {
        "bench/fixed".into()
    }
    fn dim(&self) -> usize {
        self.0.len()
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|_| self.0.clone()).collect())
    }
}

fn noise(seed: &mut u64, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|_| {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            ((*seed >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0
        })
        .collect()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let ix = ((p * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[ix]
}

fn run(dim: usize, rows: usize) -> (f64, f64, f64) {
    let store = PolisStore::open_in_memory().expect("store");
    let mut seed = 0x243f_6a88_85a3_08d3u64 ^ dim as u64;
    let model = format!("bench/fixed-{dim}");
    let t0 = Instant::now();
    {
        // One transaction for the fill: the measurement is the scan, not
        // 100k commits.
        let mut conn = store.conn();
        let tx = conn.transaction().unwrap();
        {
            let mut ins = tx
                .prepare(
                    "INSERT INTO embeddings (target_kind, target_id, chunk_ix, char_start, char_len, dim, scale, vec, model, source_hash, created_at)
                     VALUES ('prompt', ?1, 0, 0, 1, ?2, ?3, ?4, ?5, 'h', 0)",
                )
                .unwrap();
            for id in 1..=rows as i64 {
                let q = quantize(&noise(&mut seed, dim));
                ins.execute(rusqlite::params![id, dim as i64, q.scale as f64, polis_embed::pack(&q), model]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let fill_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let embedder = FixedNamed { inner: Fixed(noise(&mut seed, dim)), model };
    // Warm: the first call builds the cache (the head key), and is reported
    // separately from the steady state the budget row names.
    let t = Instant::now();
    let first = polis_embed::semantic_search(&store, &embedder, "q", 24).expect("hits");
    assert_eq!(first.len(), 24);
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut samples = Vec::with_capacity(200);
    for _ in 0..200 {
        let t = Instant::now();
        let hits = polis_embed::semantic_search(&store, &embedder, "q", 24).expect("hits");
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(hits.len(), 24);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "semantic/100k dim={dim}: fill {fill_ms:.0} ms, cold {cold_ms:.1} ms, warm p50 {:.2} ms p95 {:.2} ms max {:.2} ms",
        percentile(&samples, 0.5),
        percentile(&samples, 0.95),
        samples.last().copied().unwrap_or(0.0)
    );
    (cold_ms, percentile(&samples, 0.5), percentile(&samples, 0.95))
}

/// The fixed embedder under the model id the rows were stored with.
struct FixedNamed {
    inner: Fixed,
    model: String,
}
impl Embedder for FixedNamed {
    fn model_id(&self) -> String {
        self.model.clone()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        self.inner.embed(texts)
    }
}

/// The dot bodies themselves, 100k × dim, so the scan's cost is
/// attributable: the arch path this machine took against the portable
/// (autovectorized) body.
fn dot_bodies(dim: usize) {
    let mut seed = 0x1234_5678_9abc_def0u64;
    let rows: Vec<Vec<i8>> = (0..100_000).map(|_| noise(&mut seed, dim).iter().map(|x| (x * 127.0) as i8).collect()).collect();
    let q: Vec<i8> = noise(&mut seed, dim).iter().map(|x| (x * 127.0) as i8).collect();
    let t = Instant::now();
    let mut acc = 0i64;
    for r in &rows {
        acc += polis_core::vec::dot_i8(&q, r) as i64;
    }
    let arch = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let mut acc2 = 0i64;
    for r in &rows {
        acc2 += polis_core::vec::dot_i8_portable(&q, r) as i64;
    }
    let portable = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(acc, acc2);
    eprintln!(
        "dot_i8 100k×{dim}: dot_i8 {arch:.2} ms ({:.1} ns/dot), dot_i8_portable {portable:.2} ms ({:.1} ns/dot)",
        arch * 1e6 / 100_000.0,
        portable * 1e6 / 100_000.0
    );
}

#[test]
fn semantic_search_alone_at_100k_chunks() {
    // `POLIS_BENCH_100K=1` arms the budget; any other value runs the full
    // 100k and prints (a CI runner's first reading, before it is gated).
    let armed = std::env::var("POLIS_BENCH_100K").map(|v| v == "1").unwrap_or(false);
    let rows = if std::env::var("POLIS_BENCH_100K").is_ok() { 100_000 } else { 20_000 };
    if rows == 100_000 {
        dot_bodies(256);
        dot_bodies(512);
    }
    for dim in [256usize, 512] {
        let (_cold, p50, p95) = run(dim, rows);
        if armed {
            assert!(p50 < 60.0, "dim {dim}: p50 {p50:.2} ms over the 60 ms row");
            assert!(p95 < 120.0, "dim {dim}: p95 {p95:.2} ms over the 120 ms row");
        }
    }
    let _ = Arc::new(0);
}
