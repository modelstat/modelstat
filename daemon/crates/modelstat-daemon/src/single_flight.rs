//! Coalescing single-flight runner.
//!
//! Wraps an async task so **at most one invocation runs at a time** and
//! overlapping requests collapse into a single follow-up run rather than
//! stacking up. This is the load-bearing bound on the daemon's memory: the scan
//! is triggered from three places (boot, the file-watcher, the 5-min backstop),
//! and without coalescing a long backfill would admit fresh concurrent
//! `scan_all()` runs faster than the one serialized summariser queue could drain
//! them — each parked run pinning its event buffers until the process OOMs.
//!
//! Guarantees (identical to the TS):
//!   - `task` never runs concurrently with itself.
//!   - A [`trigger`](CoalescingRunner::trigger) during a run causes exactly one
//!     more run afterwards; multiple triggers coalesce into that single
//!     follow-up and the most recent argument wins.
//!   - `task` errors are the task's own concern (it returns `()`); a failure
//!     never drops the queued follow-up.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

type BoxedTask<A> = dyn Fn(A) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

struct Inner<A> {
    running: bool,
    /// The single coalesced follow-up (most-recent-wins), if any.
    next: Option<A>,
}

/// A coalescing runner over an async `task(arg)`. Clone-cheap (all state is
/// shared behind `Arc`), so every trigger site can hold its own handle.
#[derive(Clone)]
pub struct CoalescingRunner<A> {
    inner: Arc<Mutex<Inner<A>>>,
    idle: Arc<Notify>,
    task: Arc<BoxedTask<A>>,
}

impl<A: Send + 'static> CoalescingRunner<A> {
    /// Build a runner over `task`. `task` owns its error reporting — it returns
    /// `()`, so a scan failure is logged inside and never escapes here.
    pub fn new<F, Fut>(task: F) -> Self
    where
        F: Fn(A) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                running: false,
                next: None,
            })),
            idle: Arc::new(Notify::new()),
            task: Arc::new(move |a| Box::pin(task(a))),
        }
    }

    /// Request a run. Starts immediately if idle; otherwise records that one more
    /// run is needed once the active run finishes (coalescing — the most recent
    /// `arg` wins). Requires a tokio runtime (the active run is spawned).
    pub fn trigger(&self, arg: A) {
        let mut inner = self.inner.lock().unwrap();
        if inner.running {
            inner.next = Some(arg);
            return;
        }
        inner.running = true;
        drop(inner);

        let inner = self.inner.clone();
        let idle = self.idle.clone();
        let task = self.task.clone();
        tokio::spawn(async move {
            let mut cur = arg;
            loop {
                // The task swallows its own errors (returns ()), so a failure
                // can't drop the coalesced follow-up below.
                (task)(cur).await;
                // No await between taking `next` and clearing `running`, so a
                // trigger() can't race into the gap: it either arrived during the
                // task (picked up here) or sees running==false and starts fresh.
                let mut g = inner.lock().unwrap();
                match g.next.take() {
                    Some(n) => cur = n,
                    None => {
                        g.running = false;
                        drop(g);
                        idle.notify_waiters();
                        break;
                    }
                }
            }
        });
    }

    /// True while a run is in progress.
    pub fn is_running(&self) -> bool {
        self.inner.lock().unwrap().running
    }

    /// True while a follow-up run is queued behind the active one.
    pub fn is_pending(&self) -> bool {
        self.inner.lock().unwrap().next.is_some()
    }

    /// Resolve once the runner is idle — the active run plus any follow-up it
    /// coalesces has fully drained. Resolves immediately when already idle. Used
    /// at shutdown to let an in-flight scan settle before tearing down the engine.
    pub async fn idle(&self) {
        loop {
            // Register the waiter BEFORE the running check so a notify that fires
            // in the gap isn't lost (the canonical tokio Notify condition-wait).
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.inner.lock().unwrap().running {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify as TNotify;

    #[tokio::test]
    async fn runs_the_task_and_goes_idle() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let runner = CoalescingRunner::new(move |_: ()| {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
            }
        });
        runner.trigger(());
        runner.idle().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(!runner.is_running());
        assert!(!runner.is_pending());
    }

    #[tokio::test]
    async fn overlapping_triggers_coalesce_into_one_followup() {
        let runs = Arc::new(AtomicUsize::new(0));
        // A gate the first run blocks on, so we can pile up triggers mid-run.
        let gate = Arc::new(TNotify::new());
        let r = runs.clone();
        let g = gate.clone();
        let runner = CoalescingRunner::new(move |_arg: u32| {
            let r = r.clone();
            let g = g.clone();
            async move {
                if r.fetch_add(1, Ordering::SeqCst) == 0 {
                    // First run: block until released so triggers below coalesce.
                    g.notified().await;
                }
            }
        });
        runner.trigger(1);
        // Wait until the first run is actually executing + blocked on the gate.
        while !runner.is_running() || runs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // Three triggers during the run collapse into ONE follow-up (last wins).
        runner.trigger(2);
        runner.trigger(3);
        runner.trigger(4);
        assert!(runner.is_pending());
        gate.notify_waiters(); // release the first run
        runner.idle().await;
        // Exactly two runs total: the original + one coalesced follow-up.
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_task_panic_free_error_never_wedges_the_runner() {
        // The task returns () (owns its errors); a run that does "nothing useful"
        // still lets the next trigger start a fresh chain.
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let runner = CoalescingRunner::new(move |_: ()| {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
            }
        });
        runner.trigger(());
        runner.idle().await;
        runner.trigger(());
        runner.idle().await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn idle_resolves_immediately_when_never_triggered() {
        let runner: CoalescingRunner<()> = CoalescingRunner::new(|_| async {});
        // Should not hang.
        tokio::time::timeout(Duration::from_secs(1), runner.idle())
            .await
            .expect("idle resolves immediately when idle");
    }
}
