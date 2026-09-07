// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Timing glue over `polis_core::latency`: one call wraps a closure in a
//! `tracing` span carrying the measured `ms` AND records the sample into the
//! process-global ring the stats route reports. Every retrieval arm, every
//! write path and every gardener pass goes through here, so `docs/bench.md`'s
//! budget rows have a live counterpart in `/v1/context/stats`.

use std::time::Instant;

pub use polis_core::latency::{record, record_duration, report, report_for, reset, OpLatency, Timer};

/// Run `f` inside an `info_span!("polis.op")` whose `ms` field is filled in
/// when it returns, and record the same sample for `op`.
pub fn timed<T>(op: &'static str, f: impl FnOnce() -> T) -> T {
    let span = tracing::info_span!("polis.op", op, ms = tracing::field::Empty);
    let _guard = span.enter();
    let start = Instant::now();
    let out = f();
    let ms = elapsed_ms(start);
    span.record("ms", ms);
    record(op, ms);
    out
}

/// The async twin: the future runs instrumented by the span; the sample is
/// recorded when it resolves. (A span guard must never be held across an
/// `.await`, hence `Instrument` rather than `enter`.)
pub async fn timed_async<T, F: std::future::Future<Output = T>>(op: &'static str, fut: F) -> T {
    use tracing::Instrument;
    let span = tracing::info_span!("polis.op", op, ms = tracing::field::Empty);
    let start = Instant::now();
    let out = fut.instrument(span.clone()).await;
    let ms = elapsed_ms(start);
    span.record("ms", ms);
    record(op, ms);
    out
}

fn elapsed_ms(start: Instant) -> u32 {
    start.elapsed().as_millis().min(u32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_records_the_op_and_returns_the_value() {
        let v = timed("latency.test.timed", || 41 + 1);
        assert_eq!(v, 42);
        assert_eq!(report_for("latency.test.timed").map(|r| r.n), Some(1));
    }

    #[tokio::test]
    async fn timed_async_records_when_the_future_resolves() {
        let v = timed_async("latency.test.timed_async", async { "done" }).await;
        assert_eq!(v, "done");
        assert_eq!(report_for("latency.test.timed_async").map(|r| r.n), Some(1));
    }
}
