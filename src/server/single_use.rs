//! One-session-per-process mode: the process serves a single browser session, then shuts down.

use std::sync::atomic::{AtomicBool, Ordering};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Tracks whether this process has spent its one session and signals shutdown
/// once that session is over.
#[derive(Debug, Default)]
pub struct SingleUse {
    spent: AtomicBool,
    finished: CancellationToken,
}

impl SingleUse {
    /// A fresh, unspent instance.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the one use. Returns a guard for the first caller only; every
    /// later or concurrent caller gets `None`. Dropping the guard, on any path,
    /// marks the session finished.
    #[must_use]
    pub fn try_spend(&self) -> Option<SpentGuard> {
        self.spent
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| SpentGuard {
                finished: self.finished.clone(),
            })
    }

    /// True once the one use has been taken.
    #[must_use]
    pub fn is_spent(&self) -> bool {
        self.spent.load(Ordering::SeqCst)
    }

    /// Resolves when the spent session has finished (its guard was dropped).
    pub fn finished(&self) -> WaitForCancellationFutureOwned {
        self.finished.clone().cancelled_owned()
    }
}

/// Held for the lifetime of the single session; dropping it starts shutdown.
#[derive(Debug)]
pub struct SpentGuard {
    finished: CancellationToken,
}

impl Drop for SpentGuard {
    fn drop(&mut self) {
        self.finished.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::SingleUse;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn only_the_first_spend_succeeds() {
        let single = SingleUse::new();
        assert!(!single.is_spent());
        let first = single.try_spend();
        assert!(first.is_some());
        assert!(single.is_spent());
        assert!(single.try_spend().is_none());
        drop(first);
        assert!(
            single.try_spend().is_none(),
            "a finished instance stays spent"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_spends_yield_exactly_one_guard() {
        for _ in 0..200 {
            let single = Arc::new(SingleUse::new());
            let attempts: Vec<_> = (0..16)
                .map(|_| {
                    let single = Arc::clone(&single);
                    tokio::spawn(async move { single.try_spend() })
                })
                .collect();
            let mut winners = 0;
            let mut guards = Vec::new();
            for attempt in attempts {
                if let Some(guard) = attempt.await.unwrap() {
                    winners += 1;
                    guards.push(guard);
                }
            }
            assert_eq!(winners, 1);
        }
    }

    #[tokio::test]
    async fn dropping_the_guard_signals_finished() {
        let single = SingleUse::new();
        let finished = single.finished();
        let guard = single.try_spend().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), single.finished())
                .await
                .is_err(),
            "not finished while the session holds the guard"
        );
        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), finished)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unspent_instance_never_finishes() {
        let single = SingleUse::new();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), single.finished())
                .await
                .is_err()
        );
    }
}
