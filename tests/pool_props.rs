//! Property-based invariants for the warm pool: arbitrary interleavings of
//! claims, destroys, drops, and cancelled waiters must never violate capacity,
//! uniqueness, or accounting.

#![forbid(unsafe_code)]

use browserserve::pool::{Pool, PoolOptions, SessionFactory};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Default)]
struct Counters {
    created: AtomicUsize,
    destroyed: AtomicUsize,
    next_id: AtomicUsize,
}

#[derive(Clone, Default)]
struct PropFactory {
    counters: Arc<Counters>,
}

#[derive(Debug)]
struct PropSession {
    id: usize,
}

impl SessionFactory for PropFactory {
    type Session = PropSession;

    async fn create(&self) -> Result<PropSession, String> {
        self.counters.created.fetch_add(1, Ordering::SeqCst);
        Ok(PropSession {
            id: self.counters.next_id.fetch_add(1, Ordering::SeqCst),
        })
    }

    async fn destroy(&self, _session: PropSession) {
        self.counters.destroyed.fetch_add(1, Ordering::SeqCst);
    }

    fn is_alive(&self, _session: &mut PropSession) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
enum Op {
    Claim,
    DestroyOne,
    DropOne,
    CancelWaiter,
    Tick,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Claim),
        2 => Just(Op::DestroyOne),
        1 => Just(Op::DropOne),
        1 => Just(Op::CancelWaiter),
        1 => Just(Op::Tick),
    ]
}

async fn settle_until<F: Fn() -> bool>(condition: F) -> bool {
    for _ in 0..300 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

#[allow(clippy::too_many_lines)]
async fn run_case(
    min_ready: usize,
    max_sessions: usize,
    max_queue: usize,
    ops: Vec<Op>,
) -> Result<(), TestCaseError> {
    let factory = PropFactory::default();
    let counters = Arc::clone(&factory.counters);
    let pool = Pool::new(
        factory,
        PoolOptions {
            min_ready,
            max_sessions,
            max_queue,
            queue_timeout: Duration::from_millis(20),
            warm_idle: Duration::from_mins(1),
            warm_max_age: Duration::from_mins(1),
        },
    );

    let mut held = Vec::new();
    let mut seen_ids: HashSet<usize> = HashSet::new();

    for op in ops {
        match op {
            Op::Claim => {
                if let Ok(claimed) = pool.claim().await {
                    prop_assert!(
                        seen_ids.insert(claimed.session().id),
                        "a session id was claimed twice: sessions must never be reused"
                    );
                    held.push(claimed);
                }
            }
            Op::DestroyOne => {
                if !held.is_empty() {
                    pool.destroy(held.remove(0)).await;
                }
            }
            Op::DropOne => {
                if !held.is_empty() {
                    drop(held.remove(0));
                }
            }
            Op::CancelWaiter => {
                let waiter_pool = pool.clone();
                let handle = tokio::spawn(async move { waiter_pool.claim().await });
                tokio::time::sleep(Duration::from_millis(1)).await;
                handle.abort();
                if let Ok(Ok(claimed)) = handle.await {
                    prop_assert!(seen_ids.insert(claimed.session().id));
                    drop(claimed);
                }
            }
            Op::Tick => {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }

        let stats = pool.stats();
        prop_assert!(
            stats.running <= max_sessions,
            "running {} exceeded the ceiling {}",
            stats.running,
            max_sessions
        );
        prop_assert!(
            stats.warm <= min_ready.max(1),
            "warm stock {} exceeded the floor {}",
            stats.warm,
            min_ready
        );
        prop_assert!(
            held.len() <= max_sessions,
            "held {} sessions above the ceiling {}",
            held.len(),
            max_sessions
        );
    }

    for claimed in held.drain(..) {
        pool.destroy(claimed).await;
    }
    let drained = settle_until(|| pool.stats().running == 0 && pool.stats().queued == 0).await;
    prop_assert!(drained, "running/queued did not return to zero after drain");

    let mut restored = Vec::new();
    for _ in 0..max_sessions {
        match pool.claim().await {
            Ok(claimed) => {
                prop_assert!(seen_ids.insert(claimed.session().id));
                restored.push(claimed);
            }
            Err(e) => {
                return Err(TestCaseError::fail(format!(
                    "capacity was lost: claim {} of {max_sessions} failed after full drain: {e}",
                    restored.len() + 1
                )));
            }
        }
    }
    for claimed in restored.drain(..) {
        pool.destroy(claimed).await;
    }

    pool.close();
    let leak_free = settle_until(|| {
        counters.created.load(Ordering::SeqCst) == counters.destroyed.load(Ordering::SeqCst)
    })
    .await;
    prop_assert!(
        leak_free,
        "session leak: created {} != destroyed {}",
        counters.created.load(Ordering::SeqCst),
        counters.destroyed.load(Ordering::SeqCst)
    );
    prop_assert_eq!(pool.stats().queued, 0);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        max_shrink_iters: 200,
        .. ProptestConfig::default()
    })]

    #[test]
    fn pool_invariants_hold_under_arbitrary_interleavings(
        min_ready in 0usize..3,
        max_sessions in 1usize..4,
        max_queue in 0usize..3,
        ops in proptest::collection::vec(op_strategy(), 1..25),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| TestCaseError::fail(e.to_string()))?;
        runtime.block_on(run_case(min_ready, max_sessions, max_queue, ops))?;
    }
}
