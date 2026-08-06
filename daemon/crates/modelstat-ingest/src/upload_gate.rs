//! How many batch uploads may be in flight at once — decided by the SERVER, not
//! by us.
//!
//! Each `/v1/ingest/raw` POST makes the edge summarise a session with an LLM, so
//! a POST costs seconds and a backlog of sessions uploaded one at a time crawls.
//! Sending them concurrently is the fix, but "how many" is not ours to pick: the
//! edge load-sheds behind global and per-scope semaphores and answers `429` with
//! a `Retry-After` when it is genuinely full. Guess too high and every daemon in
//! a fleet hammers a saturated server; guess too low and the backlog crawls for
//! no reason.
//!
//! So the limit ADAPTS, the way congestion control does:
//!   · additive increase — after [`WINS_TO_GROW`] consecutive commits, allow one
//!     more, up to [`MAX_CONCURRENCY`];
//!   · multiplicative decrease — on a throttle, halve it at once, never below
//!     [`MIN_CONCURRENCY`] (which is the old sequential behaviour, so the floor
//!     is always something known to work).
//!
//! Backing off is only half of it: the caller still honours `Retry-After` on the
//! throttled request itself. This governs how many requests exist, that governs
//! when one retries.
//!
//! Lives beside the retry logic on purpose. A `429` is visible in exactly one
//! place — the upload's own response — and a limiter that cannot see it would
//! have to be told, which is how the signal gets lost.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Where a fresh daemon starts. Two, not one: the common case is a healthy
/// server, and one concurrent upload is what made a cold-start backlog take
/// hours. Not higher, because a cold start is also when many daemons wake at
/// once after a fleet-wide update.
pub const START_CONCURRENCY: usize = 2;
/// Ceiling. Deliberately small: the work behind each POST is an LLM call on a
/// shared edge, so the useful range is a handful, and a low ceiling bounds the
/// damage a bug here can do.
pub const MAX_CONCURRENCY: usize = 6;
/// Floor — the sequential behaviour this replaced, so the worst case is what
/// shipped before rather than a stall.
pub const MIN_CONCURRENCY: usize = 1;
/// Commits in a row before allowing one more in flight. Slow to grow, quick to
/// shrink: overshooting costs a stranger's server, undershooting costs us time.
pub const WINS_TO_GROW: usize = 8;

/// An adaptive in-flight limit for batch uploads. Cheap to clone (`Arc` inside);
/// every clone shares one limit, which is the point — the ceiling belongs to the
/// server, not to a handle.
#[derive(Debug)]
pub struct UploadGate {
    /// Held at [`MAX_CONCURRENCY`] permits and shrunk by FORGETTING them, so the
    /// number available is the current limit.
    sem: Arc<Semaphore>,
    /// The limit we intend, which `sem` catches up to as permits come back.
    target: AtomicUsize,
    /// Permits a shrink wanted to remove but could not, because they were in
    /// flight at the time. Paid off by [`UploadGate::acquire`] as they return.
    debt: AtomicUsize,
    /// Consecutive commits since the last throttle.
    wins: AtomicUsize,
}

impl Default for UploadGate {
    fn default() -> Self {
        Self::new()
    }
}

impl UploadGate {
    #[must_use]
    pub fn new() -> Self {
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENCY));
        // Down to the starting limit. Nothing is in flight yet, so this always
        // takes effect immediately and incurs no debt.
        sem.forget_permits(MAX_CONCURRENCY - START_CONCURRENCY);
        Self {
            sem,
            target: AtomicUsize::new(START_CONCURRENCY),
            debt: AtomicUsize::new(0),
            wins: AtomicUsize::new(0),
        }
    }

    /// The limit in force right now — for logs and tests.
    #[must_use]
    pub fn limit(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    /// Wait for a slot. The permit is released when dropped, so the caller holds
    /// it across its WHOLE retry loop: a retry is the same piece of work having
    /// another go, and making it queue again behind newer work would let a
    /// struggling batch starve.
    pub async fn acquire(&self) -> OwnedSemaphorePermit {
        loop {
            let permit = self
                .sem
                .clone()
                .acquire_owned()
                .await
                .expect("upload gate semaphore is never closed");
            // A shrink that found every permit in flight left debt behind. Pay it
            // with this one — retire the permit instead of using it — and wait for
            // another. That is the shrink finally landing, one returning slot at a
            // time.
            if self
                .debt
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| d.checked_sub(1))
                .is_ok()
            {
                permit.forget();
                continue;
            }
            return permit;
        }
    }

    /// A batch committed. Grows the limit after a run of them.
    pub fn on_commit(&self) {
        let wins = self.wins.fetch_add(1, Ordering::Relaxed) + 1;
        if wins < WINS_TO_GROW {
            return;
        }
        self.wins.store(0, Ordering::Relaxed);
        // Only grow if the shrink we owe has actually been paid; otherwise the
        // two fight and the effective limit drifts.
        if self.debt.load(Ordering::Relaxed) > 0 {
            return;
        }
        let grown = self
            .target
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                (t < MAX_CONCURRENCY).then_some(t + 1)
            });
        if grown.is_ok() {
            self.sem.add_permits(1);
        }
    }

    /// The server pushed back (`429`, or a `503` from a summariser at capacity).
    /// Halves the limit at once.
    pub fn on_throttled(&self) {
        self.wins.store(0, Ordering::Relaxed);
        let before = self.target.load(Ordering::Relaxed);
        let after = (before / 2).max(MIN_CONCURRENCY);
        if after == before {
            return;
        }
        self.target.store(after, Ordering::Relaxed);
        let want = before - after;
        // Takes only permits sitting idle. The rest become debt, retired by
        // `acquire` as in-flight uploads finish — a shrink must never block the
        // thread that noticed the throttle.
        let removed = self.sem.forget_permits(want);
        if removed < want {
            self.debt.fetch_add(want - removed, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_starts_between_sequential_and_the_ceiling() {
        let g = UploadGate::new();
        assert_eq!(g.limit(), START_CONCURRENCY);
        assert_eq!(g.sem.available_permits(), START_CONCURRENCY);
    }

    #[tokio::test]
    async fn the_limit_is_the_number_of_slots_handed_out() {
        let g = UploadGate::new();
        let a = g.acquire().await;
        let b = g.acquire().await;
        // At the limit, the next caller waits rather than piling on.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), g.acquire())
                .await
                .is_err(),
            "a third upload must queue at a limit of two"
        );
        drop(a);
        drop(b);
        // A returned slot is reusable — this would hang if it were not.
        let _reused = g.acquire().await;
    }

    #[tokio::test]
    async fn a_throttle_halves_it_and_commits_earn_it_back() {
        let g = UploadGate::new();
        // Grow to the ceiling first: WINS_TO_GROW commits per step.
        for _ in 0..(WINS_TO_GROW * (MAX_CONCURRENCY - START_CONCURRENCY)) {
            g.on_commit();
        }
        assert_eq!(g.limit(), MAX_CONCURRENCY, "sustained success grows it");

        g.on_throttled();
        assert_eq!(
            g.limit(),
            MAX_CONCURRENCY / 2,
            "a throttle halves it at once"
        );
        g.on_throttled();
        assert_eq!(g.limit(), MAX_CONCURRENCY / 4);

        // And it recovers rather than staying shrunk forever.
        for _ in 0..WINS_TO_GROW {
            g.on_commit();
        }
        assert_eq!(
            g.limit(),
            MAX_CONCURRENCY / 4 + 1,
            "commits earn a slot back"
        );
    }

    #[tokio::test]
    async fn it_never_shrinks_below_the_sequential_floor() {
        let g = UploadGate::new();
        for _ in 0..20 {
            g.on_throttled();
        }
        assert_eq!(
            g.limit(),
            MIN_CONCURRENCY,
            "the floor is what shipped before"
        );
        // Still usable — a floor that deadlocked would be worse than slow.
        let p = g.acquire().await;
        drop(p);
    }

    /// The hard case: a throttle arrives while every slot is busy, so there is
    /// nothing to forget yet. The shrink must still land as slots return.
    #[tokio::test]
    async fn a_shrink_lands_even_when_every_slot_is_in_flight() {
        let g = UploadGate::new();
        for _ in 0..(WHOLE_RUN) {
            g.on_commit();
        }
        assert_eq!(g.limit(), MAX_CONCURRENCY);

        // Hold every permit, then throttle twice.
        let mut held = Vec::new();
        for _ in 0..MAX_CONCURRENCY {
            held.push(g.acquire().await);
        }
        g.on_throttled();
        assert_eq!(g.limit(), MAX_CONCURRENCY / 2);
        assert!(g.debt.load(Ordering::Relaxed) > 0, "the shrink is owed");

        // Give everything back; `acquire` retires the owed permits.
        held.clear();
        let mut now_held = Vec::new();
        for _ in 0..(MAX_CONCURRENCY / 2) {
            now_held.push(g.acquire().await);
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), g.acquire())
                .await
                .is_err(),
            "the halved limit is real once the slots came back"
        );
        assert_eq!(g.debt.load(Ordering::Relaxed), 0, "debt fully paid");
    }

    const WHOLE_RUN: usize = WINS_TO_GROW * (MAX_CONCURRENCY - START_CONCURRENCY);
}
