//! Warm session pool: pre-launched browsers, claim-once, destroy-after.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::{Notify, Semaphore, TryAcquireError};
use tokio::time::timeout;

/// Creates, destroys, and health-checks pool sessions.
///
/// The pool never reuses a session: every claim hands out a session that is
/// destroyed on release. Implementations must be cheap to clone-share via the
/// pool's own `Arc`.
pub trait SessionFactory: Send + Sync + 'static {
    /// One launched browser and everything needed to tear it down.
    type Session: Send + 'static;

    /// Launches a fresh session. Called by the replenisher and, on warm-stock
    /// miss, inline by a claimer.
    fn create(&self) -> impl std::future::Future<Output = Result<Self::Session, String>> + Send;

    /// Destroys a session completely (kill, reap, remove state). Must be
    /// idempotent-safe and never panic.
    fn destroy(&self, session: Self::Session) -> impl std::future::Future<Output = ()> + Send;

    /// Cheap liveness check for parked sessions.
    fn is_alive(&self, session: &mut Self::Session) -> bool;
}

/// Pool sizing knobs (mirrors the `pool.*` config block).
#[derive(Debug, Clone)]
pub struct PoolOptions {
    /// Warm sessions kept ready ahead of demand.
    pub min_ready: usize,
    /// Hard ceiling of concurrently claimed sessions.
    pub max_sessions: usize,
    /// Maximum claimers allowed to wait; more are rejected immediately.
    pub max_queue: usize,
    /// How long a queued claimer waits before rejection.
    pub queue_timeout: Duration,
    /// Warm sessions above `min_ready` idle longer than this are culled.
    pub warm_idle: Duration,
    /// Any warm session older than this is recycled.
    pub warm_max_age: Duration,
}

/// Why a claim failed.
#[derive(Debug, Error)]
pub enum ClaimError {
    /// The waiting line is full.
    #[error("session queue is full ({max_queue} waiting)")]
    QueueFull {
        /// The configured queue bound.
        max_queue: usize,
    },
    /// The claimer waited the full queue timeout without a slot opening.
    #[error("timed out waiting {waited_ms} ms for a session slot")]
    QueueTimeout {
        /// How long the claimer waited, in milliseconds.
        waited_ms: u64,
    },
    /// The pool is draining; no new sessions.
    #[error("pool is closed")]
    Closed,
    /// No warm session was available and the inline launch failed.
    #[error("session launch failed: {message}")]
    Launch {
        /// Factory-reported failure description.
        message: String,
    },
}

/// A claimed session. Destroy it through [`Pool::destroy`]; dropping it
/// without destroying spawns a background destroy as a safety net.
pub struct Claimed<F: SessionFactory> {
    session: Option<F::Session>,
    pool: Arc<Inner<F>>,
}

impl<F: SessionFactory> Claimed<F> {
    /// The claimed session.
    pub fn session(&self) -> &F::Session {
        self.session
            .as_ref()
            .unwrap_or_else(|| unreachable!("session taken only by destroy paths"))
    }

    /// Mutable access to the claimed session (for lending out transports).
    pub fn session_mut(&mut self) -> &mut F::Session {
        self.session
            .as_mut()
            .unwrap_or_else(|| unreachable!("session taken only by destroy paths"))
    }

    /// Takes the session out for bridging; the claim still owes a destroy.
    pub(crate) fn take_session(&mut self) -> Option<F::Session> {
        self.session.take()
    }
}

impl<F: SessionFactory> Drop for Claimed<F> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let pool = Arc::clone(&self.pool);
            tokio::spawn(async move {
                pool.destroy_session(session).await;
            });
        }
    }
}

struct Warm<S> {
    session: S,
    ready_at: Instant,
}

struct Inner<F: SessionFactory> {
    factory: F,
    options: PoolOptions,
    semaphore: Semaphore,
    warm: Mutex<VecDeque<Warm<F::Session>>>,
    waiting: AtomicUsize,
    launching: AtomicUsize,
    replenish: Notify,
}

impl<F: SessionFactory> Inner<F> {
    async fn destroy_session(&self, session: F::Session) {
        self.factory.destroy(session).await;
        self.semaphore.add_permits(1);
        self.replenish.notify_one();
    }

    fn pop_warm(&self) -> Option<Warm<F::Session>> {
        self.warm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    fn warm_len(&self) -> usize {
        self.warm
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Live pool counters for `/pressure` and `/ready`.
#[derive(Debug, Clone, Copy)]
pub struct PoolStats {
    /// Sessions currently claimed by clients.
    pub running: usize,
    /// Claimers currently waiting for a slot.
    pub queued: usize,
    /// Warm sessions parked and ready.
    pub warm: usize,
    /// The configured session ceiling.
    pub max_sessions: usize,
    /// The configured queue bound.
    pub max_queue: usize,
    /// The pool accepts new claims.
    pub accepting: bool,
}

/// The warm pool. Cheap to clone; all clones share state.
pub struct Pool<F: SessionFactory> {
    inner: Arc<Inner<F>>,
}

impl<F: SessionFactory> Clone for Pool<F> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<F: SessionFactory> Pool<F> {
    /// Creates the pool and starts its replenisher task.
    pub fn new(factory: F, options: PoolOptions) -> Self {
        let inner = Arc::new(Inner {
            semaphore: Semaphore::new(options.max_sessions),
            warm: Mutex::new(VecDeque::new()),
            waiting: AtomicUsize::new(0),
            launching: AtomicUsize::new(0),
            replenish: Notify::new(),
            factory,
            options,
        });
        let pool = Self { inner };
        tokio::spawn(replenisher(Arc::clone(&pool.inner)));
        pool
    }

    /// Claims a session: warm hit is instant, otherwise an inline launch,
    /// otherwise a bounded, timed wait for capacity.
    ///
    /// # Errors
    ///
    /// [`ClaimError::QueueFull`], [`ClaimError::QueueTimeout`],
    /// [`ClaimError::Closed`], or [`ClaimError::Launch`].
    pub async fn claim(&self) -> Result<Claimed<F>, ClaimError> {
        let inner = &self.inner;
        let options = &inner.options;

        let permit = match inner.semaphore.try_acquire() {
            Ok(permit) => permit,
            Err(TryAcquireError::Closed) => return Err(ClaimError::Closed),
            Err(TryAcquireError::NoPermits) => {
                let queued = inner.waiting.fetch_add(1, Ordering::SeqCst) + 1;
                let _queue_guard = CounterGuard(&inner.waiting);
                if queued > options.max_queue {
                    return Err(ClaimError::QueueFull {
                        max_queue: options.max_queue,
                    });
                }
                let started = Instant::now();
                match timeout(options.queue_timeout, inner.semaphore.acquire()).await {
                    Ok(Ok(permit)) => permit,
                    Ok(Err(_closed)) => return Err(ClaimError::Closed),
                    Err(_elapsed) => {
                        return Err(ClaimError::QueueTimeout {
                            waited_ms: u64::try_from(started.elapsed().as_millis())
                                .unwrap_or(u64::MAX),
                        });
                    }
                }
            }
        };
        permit.forget();

        loop {
            let Some(warm) = inner.pop_warm() else { break };
            inner.replenish.notify_one();
            let mut warm = warm;
            if inner.factory.is_alive(&mut warm.session) {
                return Ok(Claimed {
                    session: Some(warm.session),
                    pool: Arc::clone(inner),
                });
            }
            let stale = warm.session;
            let cleanup = Arc::clone(inner);
            tokio::spawn(async move {
                cleanup.factory.destroy(stale).await;
            });
        }

        inner.replenish.notify_one();
        match inner.factory.create().await {
            Ok(session) => Ok(Claimed {
                session: Some(session),
                pool: Arc::clone(inner),
            }),
            Err(message) => {
                inner.semaphore.add_permits(1);
                Err(ClaimError::Launch { message })
            }
        }
    }

    /// Destroys a claimed session and frees its capacity afterwards.
    pub async fn destroy(&self, mut claimed: Claimed<F>) {
        if let Some(session) = claimed.take_session() {
            self.inner.destroy_session(session).await;
        }
    }

    /// Stops intake: queued and future claims fail with [`ClaimError::Closed`].
    /// Warm stock is destroyed in the background.
    pub fn close(&self) {
        self.inner.semaphore.close();
        let drained: Vec<F::Session> = {
            let mut warm = self
                .inner
                .warm
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            warm.drain(..).map(|w| w.session).collect()
        };
        for session in drained {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                inner.factory.destroy(session).await;
            });
        }
        self.inner.replenish.notify_one();
    }

    /// Current counters, derived from live state.
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        let inner = &self.inner;
        let available = inner.semaphore.available_permits();
        let accepting = !inner.semaphore.is_closed();
        PoolStats {
            running: if accepting {
                inner.options.max_sessions.saturating_sub(available)
            } else {
                0
            },
            queued: inner
                .waiting
                .load(Ordering::SeqCst)
                .min(inner.options.max_queue),
            warm: inner.warm_len(),
            max_sessions: inner.options.max_sessions,
            max_queue: inner.options.max_queue,
            accepting,
        }
    }
}

struct CounterGuard<'a>(&'a AtomicUsize);

impl Drop for CounterGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn replenisher<F: SessionFactory>(inner: Arc<Inner<F>>) {
    const RETRY_BACKOFF_START: Duration = Duration::from_millis(200);
    let mut backoff = RETRY_BACKOFF_START;
    loop {
        if inner.semaphore.is_closed() {
            return;
        }

        cull_warm(&inner).await;

        let warm = inner.warm_len();
        let launching = inner.launching.load(Ordering::SeqCst);
        let active = inner
            .options
            .max_sessions
            .saturating_sub(inner.semaphore.available_permits());
        let headroom = inner
            .options
            .max_sessions
            .saturating_sub(active + warm + launching);
        let wanted = inner
            .options
            .min_ready
            .saturating_sub(warm + launching)
            .min(headroom);

        if wanted == 0 {
            let tick = inner.options.warm_idle.min(Duration::from_secs(10));
            let _ = timeout(tick, inner.replenish.notified()).await;
            continue;
        }

        inner.launching.fetch_add(1, Ordering::SeqCst);
        let result = inner.factory.create().await;
        inner.launching.fetch_sub(1, Ordering::SeqCst);
        match result {
            Ok(session) => {
                backoff = RETRY_BACKOFF_START;
                if inner.semaphore.is_closed() {
                    inner.factory.destroy(session).await;
                    return;
                }
                inner
                    .warm
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push_back(Warm {
                        session,
                        ready_at: Instant::now(),
                    });
            }
            Err(message) => {
                tracing::warn!(error = %message, "warm launch failed; backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

async fn cull_warm<F: SessionFactory>(inner: &Arc<Inner<F>>) {
    let now = Instant::now();
    let expired: Vec<F::Session> = {
        let mut warm = inner.warm.lock().unwrap_or_else(PoisonError::into_inner);
        let min_ready = inner.options.min_ready;
        let mut kept = VecDeque::with_capacity(warm.len());
        let mut removed = Vec::new();
        while let Some(entry) = warm.pop_front() {
            let age = now.duration_since(entry.ready_at);
            let mut entry = entry;
            let dead = !inner.factory.is_alive(&mut entry.session);
            let too_old = age >= inner.options.warm_max_age;
            let excess_idle = kept.len() >= min_ready && age >= inner.options.warm_idle;
            if dead || too_old || excess_idle {
                removed.push(entry.session);
            } else {
                kept.push_back(entry);
            }
        }
        *warm = kept;
        removed
    };
    for session in expired {
        inner.factory.destroy(session).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct FakeFactory {
        created: AtomicUsize,
        destroyed: AtomicUsize,
        fail_creates: AtomicBool,
        create_delay: Duration,
    }

    impl Default for FakeFactory {
        fn default() -> Self {
            Self {
                created: AtomicUsize::new(0),
                destroyed: AtomicUsize::new(0),
                fail_creates: AtomicBool::new(false),
                create_delay: Duration::ZERO,
            }
        }
    }

    #[derive(Debug)]
    struct FakeSession {
        alive: bool,
    }

    impl SessionFactory for Arc<FakeFactory> {
        type Session = FakeSession;

        async fn create(&self) -> Result<FakeSession, String> {
            if self.create_delay > Duration::ZERO {
                tokio::time::sleep(self.create_delay).await;
            }
            if self.fail_creates.load(Ordering::SeqCst) {
                return Err(String::from("boom"));
            }
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(FakeSession { alive: true })
        }

        async fn destroy(&self, _session: FakeSession) {
            self.destroyed.fetch_add(1, Ordering::SeqCst);
        }

        fn is_alive(&self, session: &mut FakeSession) -> bool {
            session.alive
        }
    }

    fn options(min_ready: usize, max_sessions: usize, max_queue: usize) -> PoolOptions {
        PoolOptions {
            min_ready,
            max_sessions,
            max_queue,
            queue_timeout: Duration::from_millis(200),
            warm_idle: Duration::from_mins(5),
            warm_max_age: Duration::from_hours(1),
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn claim_and_destroy_round_trip() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(0, 2, 2));
        let claimed = pool.claim().await.unwrap();
        assert_eq!(pool.stats().running, 1);
        pool.destroy(claimed).await;
        settle().await;
        assert_eq!(pool.stats().running, 0);
        assert_eq!(factory.destroyed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn warm_stock_is_replenished_and_used() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(2, 4, 2));
        settle().await;
        assert_eq!(pool.stats().warm, 2);
        let before = factory.created.load(Ordering::SeqCst);
        let claimed = pool.claim().await.unwrap();
        assert_eq!(
            factory.created.load(Ordering::SeqCst),
            before,
            "warm claim must not launch inline"
        );
        pool.destroy(claimed).await;
    }

    #[tokio::test]
    async fn ceiling_is_never_exceeded() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(0, 2, 5));
        let a = pool.claim().await.unwrap();
        let b = pool.claim().await.unwrap();
        assert!(matches!(
            pool.claim().await,
            Err(ClaimError::QueueTimeout { .. })
        ));
        assert_eq!(pool.stats().running, 2);
        pool.destroy(a).await;
        pool.destroy(b).await;
    }

    #[tokio::test]
    async fn queue_bound_rejects_immediately() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(0, 1, 0));
        let held = pool.claim().await.unwrap();
        let Err(err) = pool.claim().await else {
            panic!("claim must be rejected when the queue is full");
        };
        assert!(matches!(err, ClaimError::QueueFull { max_queue: 0 }));
        pool.destroy(held).await;
    }

    #[tokio::test]
    async fn waiter_gets_slot_when_one_frees() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(0, 1, 2));
        let held = pool.claim().await.unwrap();
        let waiter_pool = pool.clone();
        let waiter = tokio::spawn(async move { waiter_pool.claim().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        pool.destroy(held).await;
        let second = waiter.await.unwrap().unwrap();
        pool.destroy(second).await;
    }

    #[tokio::test]
    async fn dead_warm_sessions_are_skipped_at_claim() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(1, 2, 2));
        settle().await;
        {
            let mut warm = pool.inner.warm.lock().unwrap();
            if let Some(entry) = warm.front_mut() {
                entry.session.alive = false;
            }
        }
        let claimed = pool.claim().await.unwrap();
        assert!(claimed.session().alive);
        pool.destroy(claimed).await;
    }

    #[tokio::test]
    async fn closed_pool_rejects_and_drains_warm() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(1, 2, 2));
        settle().await;
        pool.close();
        assert!(matches!(pool.claim().await, Err(ClaimError::Closed)));
        settle().await;
        assert_eq!(pool.stats().warm, 0);
        assert!(!pool.stats().accepting);
    }

    #[tokio::test]
    async fn launch_failure_restores_capacity() {
        let factory = Arc::new(FakeFactory::default());
        factory.fail_creates.store(true, Ordering::SeqCst);
        let pool = Pool::new(Arc::clone(&factory), options(0, 1, 2));
        assert!(matches!(pool.claim().await, Err(ClaimError::Launch { .. })));
        factory.fail_creates.store(false, Ordering::SeqCst);
        let claimed = pool.claim().await.unwrap();
        pool.destroy(claimed).await;
    }

    #[tokio::test]
    async fn dropped_claim_still_destroys_and_frees() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(0, 1, 2));
        let claimed = pool.claim().await.unwrap();
        drop(claimed);
        settle().await;
        assert_eq!(factory.destroyed.load(Ordering::SeqCst), 1);
        let again = pool.claim().await.unwrap();
        pool.destroy(again).await;
    }

    #[tokio::test]
    async fn cancelled_waiters_do_not_leak_queue_slots() {
        let factory = Arc::new(FakeFactory::default());
        let pool = Pool::new(Arc::clone(&factory), options(0, 1, 1));
        let held = pool.claim().await.unwrap();
        let cancelled_pool = pool.clone();
        let cancelled = tokio::spawn(async move { cancelled_pool.claim().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancelled.abort();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.stats().queued, 0);
        pool.destroy(held).await;
    }
}
