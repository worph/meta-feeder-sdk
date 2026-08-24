//! Per-upstream request budget — a shared token bucket.
//!
//! Lifted from the card feeder's `tmdb_budget.rs` (its `TmdbBudget` half; the
//! per-indexer registry has no counterpart here) and renamed, because the
//! mechanism is upstream-agnostic and the music feeder needs three independent
//! instances rather than one global.
//!
//! **Why three, not one.** MusicBrainz's 1 req/s is a hard published ceiling
//! enforced per source IP; ListenBrainz and the Internet Archive are far more
//! generous. A single shared bucket would either throttle all three to 1/s or
//! let MusicBrainz calls burst past a limit that gets the whole box blocked.
//!
//! Design notes, unchanged from the original:
//! - **Lazy refill.** Tokens accrue on each `acquire` from elapsed wall-clock,
//!   so there is no background timer task and no idle cost.
//! - **Cancel-safe.** The token is decremented synchronously under the lock
//!   right before `acquire` returns [`Lease::Granted`]; there is no `.await`
//!   between the decision to grant and the return, so a dropped `acquire`
//!   future (consumer disconnected) never spends a token.
//! - **Global pause on throttle.** One upstream 429/503 freezes *all* grants
//!   on that bucket until the Retry-After window elapses, via
//!   [`RateBudget::note_throttled`].
//!
//! The critical sections hold a plain `std::sync::Mutex` (no awaits inside),
//! while the waiting happens outside the lock on a `tokio::time::sleep` raced
//! against a `tokio::sync::Notify`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Outcome of a [`RateBudget::acquire`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lease {
    /// A token was granted; the caller may make one upstream call.
    Granted,
    /// The wait deadline elapsed before a token became available. The caller
    /// degrades best-effort — it does **not** call the upstream anyway.
    DeadlineExceeded,
}

struct BucketState {
    tokens: f64,
    capacity: f64,
    refill_per_sec: f64,
    last_refill: Instant,
    /// When set and in the future, all grants are frozen.
    paused_until: Option<Instant>,
}

impl BucketState {
    /// Accrue tokens for elapsed time since the last refill. Called under the
    /// lock before inspecting `tokens`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }
}

/// Shared token bucket gating one upstream's API calls across the process.
pub struct RateBudget {
    state: Mutex<BucketState>,
    /// Woken whenever a grant *might* now be possible (a token accrued, or a
    /// pause was cleared). Waiters re-check under the lock after waking; a
    /// missed wake only costs an extra bounded sleep.
    wake: Notify,
}

impl RateBudget {
    /// Build a budget sustaining `refill_per_sec` grants/second with a burst
    /// ceiling of `capacity` tokens. The bucket starts full, so the first
    /// burst is served immediately.
    pub fn new(refill_per_sec: f64, capacity: f64) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BucketState {
                tokens: capacity,
                capacity,
                refill_per_sec,
                last_refill: Instant::now(),
                paused_until: None,
            }),
            wake: Notify::new(),
        })
    }

    /// Wait for one token, giving up after `deadline`. Cancel-safe: dropping
    /// the returned future before it resolves [`Lease::Granted`] spends no
    /// token.
    pub async fn acquire(&self, deadline: Duration) -> Lease {
        let give_up_at = Instant::now() + deadline;
        loop {
            // Decide-or-compute-wait, all under the lock (no awaits inside).
            let sleep_for = {
                let mut st = self.state.lock().expect("rate budget mutex poisoned");
                let now = Instant::now();
                if let Some(until) = st.paused_until {
                    if now >= until {
                        st.paused_until = None;
                    }
                }
                let paused = st.paused_until;
                if paused.is_none() {
                    st.refill(now);
                    if st.tokens >= 1.0 {
                        // Atomic grant: decrement and return with no await between.
                        st.tokens -= 1.0;
                        return Lease::Granted;
                    }
                }
                if now >= give_up_at {
                    return Lease::DeadlineExceeded;
                }
                let until_deadline = give_up_at.saturating_duration_since(now);
                let wait = match paused {
                    // Frozen: wake when the pause is scheduled to lift.
                    Some(until) => until.saturating_duration_since(now),
                    // Throttled: wake when the next whole token should accrue.
                    None => {
                        let needed = 1.0 - st.tokens;
                        let secs = if st.refill_per_sec > 0.0 {
                            needed / st.refill_per_sec
                        } else {
                            until_deadline.as_secs_f64()
                        };
                        Duration::from_secs_f64(secs.max(0.0))
                    }
                };
                wait.min(until_deadline)
            };
            tokio::select! {
                _ = tokio::time::sleep(sleep_for) => {}
                _ = self.wake.notified() => {}
            }
            if Instant::now() >= give_up_at {
                return Lease::DeadlineExceeded;
            }
        }
    }

    /// Feed back an upstream throttle response (429, or MusicBrainz's 503):
    /// pause all grants on this bucket until `now + retry_after`. Extends an
    /// existing pause but never shortens it.
    pub fn note_throttled(&self, retry_after: Duration) {
        let until = Instant::now() + retry_after;
        {
            let mut st = self.state.lock().expect("rate budget mutex poisoned");
            st.paused_until = Some(match st.paused_until {
                Some(existing) if existing > until => existing,
                _ => until,
            });
        }
        // Wake waiters so they recompute their sleep against the new pause.
        self.wake.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn grants_up_to_capacity_immediately() {
        let b = RateBudget::new(3.0, 3.0);
        for _ in 0..3 {
            assert_eq!(b.acquire(Duration::from_secs(10)).await, Lease::Granted);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn next_token_waits_for_refill() {
        let b = RateBudget::new(2.0, 2.0); // 2/s sustained
        for _ in 0..2 {
            assert_eq!(b.acquire(Duration::from_secs(10)).await, Lease::Granted);
        }
        // Bucket empty; next grant must wait ~0.5 s for one token at 2/s.
        let start = Instant::now();
        assert_eq!(b.acquire(Duration::from_secs(10)).await, Lease::Granted);
        assert!(
            start.elapsed() >= Duration::from_millis(450),
            "expected to wait ~0.5s for refill, waited {:?}",
            start.elapsed()
        );
    }

    /// The MusicBrainz shape specifically: 1/s with a burst of 2 must not let
    /// three calls through inside one second. This is the property that keeps
    /// the box off their block-list.
    #[tokio::test(start_paused = true)]
    async fn musicbrainz_shape_holds_one_per_second() {
        let b = RateBudget::new(1.0, 1.0);
        let start = Instant::now();
        for _ in 0..4 {
            assert_eq!(b.acquire(Duration::from_secs(30)).await, Lease::Granted);
        }
        // 4 grants from a 2-token bucket refilling at 1/s ⇒ at least 2 s.
        assert!(
            start.elapsed() >= Duration::from_millis(1_900),
            "4 calls at 1/s (burst 2) must take ~2s, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_exceeded_when_drained() {
        let b = RateBudget::new(0.001, 1.0); // effectively no refill within the test
        assert_eq!(b.acquire(Duration::from_secs(1)).await, Lease::Granted);
        assert_eq!(
            b.acquire(Duration::from_millis(200)).await,
            Lease::DeadlineExceeded
        );
    }

    #[tokio::test(start_paused = true)]
    async fn note_throttled_pauses_grants_even_with_tokens_available() {
        let b = RateBudget::new(100.0, 100.0);
        b.note_throttled(Duration::from_secs(2));
        assert_eq!(
            b.acquire(Duration::from_millis(500)).await,
            Lease::DeadlineExceeded
        );
        // After the pause window, grants resume.
        assert_eq!(b.acquire(Duration::from_secs(5)).await, Lease::Granted);
    }

    /// A pause is extended, never shortened — otherwise a late short retry
    /// would cancel an earlier long one and walk straight back into the block.
    #[tokio::test(start_paused = true)]
    async fn a_shorter_retry_after_does_not_shorten_an_existing_pause() {
        let b = RateBudget::new(100.0, 100.0);
        b.note_throttled(Duration::from_secs(10));
        b.note_throttled(Duration::from_secs(1));
        assert_eq!(
            b.acquire(Duration::from_secs(3)).await,
            Lease::DeadlineExceeded
        );
    }
}
