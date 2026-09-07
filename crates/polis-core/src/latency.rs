// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The latency vocabulary: a fixed-size ring of millisecond samples per named
//! operation, a process-global registry, and the report `/v1/context/stats`
//! and `MemoryApi::stats` expose (`ContextStats::latency`).
//!
//! Pure: `std::sync` + `std::time::Instant`, nothing else. The store, the
//! facade and the HTTP server all record into the same registry (every arm of
//! the answer pack, every route), so one process — a host's daemon, the
//! standalone daemon, a benchmark — reports one table. A ring, not a
//! histogram: 256 samples per op keeps the memory bounded and the percentiles
//! current (an op that was slow an hour ago and fast since reads fast).
//!
//! Measurement program §6.1 (docs/bench.md): the budget rows are p50/p95 per
//! op; this is where those numbers come from at runtime.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Samples kept per op. Percentiles are over the newest `RING_CAPACITY`.
pub const RING_CAPACITY: usize = 256;

/// One op's row in the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpLatency {
    pub op: String,
    /// Samples recorded since process start (not capped by the ring).
    pub n: u64,
    pub p50_ms: u32,
    pub p95_ms: u32,
    /// The largest sample still in the ring.
    pub max_ms: u32,
}

#[derive(Debug, Clone)]
struct Ring {
    samples: Vec<u32>,
    next: usize,
    total: u64,
}

impl Ring {
    fn new() -> Self {
        Self { samples: Vec::with_capacity(RING_CAPACITY), next: 0, total: 0 }
    }

    fn push(&mut self, ms: u32) {
        if self.samples.len() < RING_CAPACITY {
            self.samples.push(ms);
        } else {
            self.samples[self.next] = ms;
        }
        self.next = (self.next + 1) % RING_CAPACITY;
        self.total += 1;
    }

    fn report(&self, op: &str) -> OpLatency {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        OpLatency {
            op: op.to_string(),
            n: self.total,
            p50_ms: percentile(&sorted, 0.50),
            p95_ms: percentile(&sorted, 0.95),
            max_ms: sorted.last().copied().unwrap_or(0),
        }
    }
}

/// Nearest-rank percentile over an ASCENDING slice; 0 for an empty one.
/// `q` in `0.0..=1.0`.
pub fn percentile(sorted: &[u32], q: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let q = q.clamp(0.0, 1.0);
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

fn registry() -> &'static Mutex<BTreeMap<String, Ring>> {
    static REG: OnceLock<Mutex<BTreeMap<String, Ring>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Record one sample for `op`.
pub fn record(op: &str, ms: u32) {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.entry(op.to_string()).or_insert_with(Ring::new).push(ms);
}

/// Record a measured duration for `op` (saturating at `u32::MAX` ms).
pub fn record_duration(op: &str, elapsed: std::time::Duration) {
    record(op, elapsed.as_millis().min(u32::MAX as u128) as u32);
}

/// Every op recorded in this process, sorted by name.
pub fn report() -> Vec<OpLatency> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.iter().map(|(op, ring)| ring.report(op)).collect()
}

/// One op's row, if it has recorded anything.
pub fn report_for(op: &str) -> Option<OpLatency> {
    let reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.get(op).map(|r| r.report(op))
}

/// Forget every sample — a benchmark's or an eval's clean slate. The registry
/// is process-global, so two tests in one binary see each other's samples;
/// assert on `report_for` of an op name only you record.
pub fn reset() {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    reg.clear();
}

/// A guard that records the elapsed time for `op` when it is dropped (or
/// explicitly stopped). `let _t = Timer::start("pack");` at the top of a
/// function times the whole function, early returns included.
#[must_use = "a Timer records on drop — bind it to a name"]
pub struct Timer {
    op: String,
    start: Instant,
    armed: bool,
}

impl Timer {
    pub fn start(op: impl Into<String>) -> Self {
        Self { op: op.into(), start: Instant::now(), armed: true }
    }

    /// Milliseconds so far, without recording.
    pub fn elapsed_ms(&self) -> u32 {
        self.start.elapsed().as_millis().min(u32::MAX as u128) as u32
    }

    /// Record now and return the sample; the drop then records nothing.
    pub fn stop(mut self) -> u32 {
        let ms = self.elapsed_ms();
        record(&self.op, ms);
        self.armed = false;
        ms
    }

    /// Drop without recording — for a path that was not the op after all.
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if self.armed {
            record(&self.op, self.elapsed_ms());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_is_nearest_rank() {
        let s: Vec<u32> = (1..=100).collect();
        assert_eq!(percentile(&s, 0.50), 50);
        assert_eq!(percentile(&s, 0.95), 95);
        assert_eq!(percentile(&s, 1.0), 100);
        assert_eq!(percentile(&s, 0.0), 1);
        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.95), 7);
    }

    #[test]
    fn the_ring_keeps_the_newest_samples_and_counts_all_of_them() {
        let op = "latency.test.ring";
        for ms in 0..(RING_CAPACITY as u32 + 100) {
            record(op, ms);
        }
        let row = report_for(op).expect("recorded");
        assert_eq!(row.n, RING_CAPACITY as u64 + 100);
        // The 100 smallest samples fell out of the ring.
        assert_eq!(row.max_ms, RING_CAPACITY as u32 + 99);
        assert!(row.p50_ms >= 100, "p50 {} is over evicted samples", row.p50_ms);
        assert!(report().iter().any(|r| r.op == op));
    }

    #[test]
    fn the_timer_records_on_drop_and_not_after_disarm() {
        let op = "latency.test.timer";
        {
            let _t = Timer::start(op);
        }
        assert_eq!(report_for(op).map(|r| r.n), Some(1));
        Timer::start(op).disarm();
        assert_eq!(report_for(op).map(|r| r.n), Some(1));
        let ms = Timer::start(op).stop();
        assert!(ms < 1_000);
        assert_eq!(report_for(op).map(|r| r.n), Some(2));
    }

    #[test]
    fn the_row_serializes_camel_case() {
        let v = serde_json::to_value(OpLatency { op: "pack".into(), n: 3, p50_ms: 1, p95_ms: 2, max_ms: 2 }).unwrap();
        assert_eq!(v["p50Ms"], 1);
        assert_eq!(v["maxMs"], 2);
    }
}
