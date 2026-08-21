//! Adaptive concurrency limiting for RPC endpoints.
//!
//! Endpoints differ wildly in what they sustain (free public nodes throttle
//! hard, local reth does not). Instead of a fixed retry backoff, the governor
//! starts from an initial limit and adapts: additive increase on success,
//! multiplicative decrease when the endpoint signals overload. This finds the
//! sustainable rate automatically instead of guessing it up front.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct Governor {
    sem: Arc<Semaphore>,
    limit: AtomicUsize,
    max: usize,
    successes: AtomicUsize,
}

impl Governor {
    /// Start conservative and let successful traffic push the limit up.
    pub fn new(initial: usize, max: usize) -> Self {
        let initial = initial.clamp(1, max);
        let gov = Governor {
            sem: Arc::new(Semaphore::new(initial)),
            limit: AtomicUsize::new(initial),
            max,
            successes: AtomicUsize::new(0),
        };
        tracing::debug!(limit = initial, max, "rate governor started");
        gov
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        self.sem.clone().acquire_owned().await.expect("governor semaphore closed")
    }

    /// Additive increase: every run of 8 clean fetches grows the limit by 25%
    /// (min +1), so a healthy endpoint ramps 4 -> ~30 within a few hundred
    /// blocks while a throttling one gets cut back long before that.
    pub fn record_success(&self) {
        if self.successes.fetch_add(1, Ordering::Relaxed) % 8 == 7 {
            let current = self.limit.load(Ordering::Relaxed);
            if current < self.max {
                let target = (current + (current / 4).max(1)).min(self.max);
                self.sem.add_permits(target - current);
                self.limit.store(target, Ordering::Relaxed);
                tracing::debug!(limit = target, "governor increased");
            }
        }
    }

    /// Multiplicative decrease: halve the limit on a rate-limit signal.
    pub async fn record_throttled(&self) {
        self.successes.store(0, Ordering::Relaxed);
        let current = self.limit.load(Ordering::Relaxed);
        if current <= 1 {
            return;
        }
        let target = (current / 2).max(1);
        for _ in target..current {
            let permit = self.sem.acquire().await.expect("governor semaphore closed");
            permit.forget();
        }
        self.limit.store(target, Ordering::Relaxed);
        tracing::warn!(limit = target, "endpoint throttling, governor decreased");
    }
}

/// Best-effort classification of transport/RPC errors that mean "slow down":
/// HTTP 429, JSON-RPC rate-limit codes (-32005, -32029), and timeouts.
/// Deliberately over-matches on message text; the cost of a false positive is
/// only a temporary throughput dip.
pub fn is_throttle_error(err: &eyre::Report) -> bool {
    let msg = format!("{err:#}").to_ascii_lowercase();
    msg.contains("429")
        || msg.contains("too many requests")
        || msg.contains("rate limit")
        || msg.contains("ratelimit")
        || msg.contains("-32005")
        || msg.contains("-32029")
        || msg.contains("timed out")
        || msg.contains("timeout")
}
