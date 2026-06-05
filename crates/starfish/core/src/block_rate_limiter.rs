// Copyright (c) 2026 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

/// Rate limiter for own block proposals (GCRA / token-bucket): sustained
/// production of at most one block per `min_block_delay`, with bursts of up to
/// `block_rate_window / min_block_delay` back-to-back blocks after idle
/// periods. A burst capacity of 1 degenerates to a fixed minimum delay
/// between consecutive blocks.
///
/// State is a single theoretical-arrival time: the earliest instant at which
/// an ideal schedule emitting exactly one block per interval would emit next.
/// Idle time lowers it relative to `now` (accruing budget); each proposal
/// advances it by one interval.
pub(crate) struct BlockRateLimiter {
    /// Theoretical arrival time, UTC ms.
    tat_ms: u64,
    interval_ms: u64,
    burst: u64,
}

impl BlockRateLimiter {
    pub(crate) fn new(min_block_delay: Duration, burst: u64) -> Self {
        Self {
            tat_ms: 0,
            interval_ms: min_block_delay.as_millis().max(1) as u64,
            burst: burst.max(1),
        }
    }

    /// Whether a proposal at `now_ms` fits the rate envelope.
    pub(crate) fn is_conforming(&self, now_ms: u64) -> bool {
        self.tat_ms.saturating_sub(now_ms) <= (self.burst - 1) * self.interval_ms
    }

    /// Records a proposal at `now_ms`. Called for every own block, including
    /// forced proposals that bypass the conformance check; the cap bounds
    /// their overdraft so the next non-forced proposal waits at most one
    /// interval after the last block.
    pub(crate) fn record(&mut self, now_ms: u64) {
        self.tat_ms = (self.tat_ms.max(now_ms) + self.interval_ms)
            .min(now_ms + self.burst * self.interval_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: u64 = 50;
    const B: u64 = 40;

    fn limiter() -> BlockRateLimiter {
        BlockRateLimiter::new(Duration::from_millis(T), B)
    }

    #[test]
    fn burst_after_idle_then_reject() {
        let mut l = limiter();
        let now = 1_000_000;
        // Exactly B back-to-back proposals conform, the next does not.
        for _ in 0..B {
            assert!(l.is_conforming(now));
            l.record(now);
        }
        assert!(!l.is_conforming(now));
        // Budget for one more block regenerates after one interval.
        assert!(!l.is_conforming(now + T - 1));
        assert!(l.is_conforming(now + T));
    }

    #[test]
    fn sustained_rate_is_one_per_interval() {
        let mut l = limiter();
        let start = 1_000_000;
        // Drain the burst budget.
        for _ in 0..B {
            l.record(start);
        }
        // Under continuous attempts, conforming instants are spaced exactly T.
        let mut now = start;
        for _ in 0..10 {
            assert!(!l.is_conforming(now + T - 1));
            now += T;
            assert!(l.is_conforming(now));
            l.record(now);
        }
    }

    #[test]
    fn burst_one_degenerates_to_fixed_delay() {
        let mut l = BlockRateLimiter::new(Duration::from_millis(T), 1);
        let now = 1_000_000;
        assert!(l.is_conforming(now));
        l.record(now);
        // Identical to the old rule: blocked until exactly T has elapsed.
        assert!(!l.is_conforming(now));
        assert!(!l.is_conforming(now + T - 1));
        assert!(l.is_conforming(now + T));
    }

    #[test]
    fn forced_overdraft_is_capped() {
        let mut l = limiter();
        let now = 1_000_000;
        // Forced proposals record without a conformance check; the cap keeps
        // the next non-forced eligibility within one interval of the last.
        for _ in 0..(3 * B) {
            l.record(now);
        }
        assert!(!l.is_conforming(now));
        assert!(l.is_conforming(now + T));
    }

    #[test]
    fn seeding_by_replay_restores_budget_spent() {
        let mut l = limiter();
        let now = 1_000_000;
        // Replaying k recent block timestamps leaves budget for B - k more
        // (no time elapses here, so no budget regenerates in between).
        let k = 10;
        for _ in 0..k {
            l.record(now);
        }
        for _ in 0..(B - k) {
            assert!(l.is_conforming(now));
            l.record(now);
        }
        assert!(!l.is_conforming(now));
        // Timestamps older than the whole window leave the budget full.
        let mut l = limiter();
        l.record(now - B * T);
        for _ in 0..B {
            assert!(l.is_conforming(now));
            l.record(now);
        }
        assert!(!l.is_conforming(now));
    }
}
