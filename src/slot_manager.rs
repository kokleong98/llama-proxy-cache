//! Slot management (port of `slot_manager.py`).
//!
//! - `GSlot = (backend_id, local_slot_id)`
//! - selection: free (never used) first, in insertion order; otherwise the
//!   least recently used slot (smallest `last_used` timestamp)
//! - each slot is guarded by a `Semaphore(1)` — the Rust equivalent of the
//!   per-slot `asyncio.Lock`
//! - `acquire` waits up to `acquire_timeout` (default 300 s, same as the
//!   `asyncio.wait_for` wrapper in `app.py`) and restores before chat when a
//!   restore key is given
//! - `save_after` saves the KV cache and updates the LRU timestamp

use crate::config::BackendConf;
use crate::llama_client::{BackendError, LlamaBackend, RestoreOutcome};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

/// Default acquire timeout (Python: `ACQUIRE_TIMEOUT = 300.0`).
pub const DEFAULT_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(300);

/// Default circuit-breaker cooldown for a backend with connection failures.
pub const DEFAULT_BACKEND_COOLDOWN: Duration = Duration::from_secs(5);

/// Cap for the escalated cooldown (5 s * 2^streak, never above this).
pub const MAX_BACKEND_COOLDOWN: Duration = Duration::from_secs(60);

/// A global slot: `(backend_id, local_slot_id)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GSlot {
    pub be: usize,
    pub slot: usize,
}

impl std::fmt::Display for GSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "({}, {})", self.be, self.slot)
    }
}

/// Returned when a slot could not be acquired within `acquire_timeout`.
#[derive(Debug)]
pub struct AcquireTimeout;

/// A held slot. Dropping it (or calling `release`) frees the slot lock,
/// i.e. it is the RAII equivalent of the Python lock returned by
/// `acquire_for_request`.
pub struct SlotGuard {
    pub slot: GSlot,
    /// `Some(outcome)` when a restore was attempted before the chat.
    pub restored: Option<RestoreOutcome>,
    /// Holds the slot lock until the guard is dropped / released.
    #[allow(dead_code)]
    permit: OwnedSemaphorePermit,
}

impl SlotGuard {
    /// Explicit release; the permit is dropped here (also released on drop).
    pub fn release(self) {}
}

pub struct SlotManager {
    clients: Vec<Arc<dyn LlamaBackend>>,
    all: Vec<GSlot>,
    last_used: Vec<AtomicU64>,
    sems: Vec<Arc<tokio::sync::Semaphore>>,
    acquire_timeout: Duration,
    /// One entry per backend: nanosecond deadline until which the backend is
    /// excluded from slot selection (circuit breaker). 0 = healthy.
    down_until: Vec<AtomicU64>,
    backend_cooldown: Duration,
    /// One entry per backend: consecutive connection-failure count, used to
    /// escalate the cooldown (5 s, 10 s, 20 s, 40 s, then capped at 60 s).
    /// Reset by any successful request or probe.
    failure_streak: Vec<AtomicU32>,
}

impl SlotManager {
    /// `clients[i]` must correspond to `backends[i]`.
    pub fn new(backends: &[BackendConf], clients: Vec<Arc<dyn LlamaBackend>>) -> Self {
        assert_eq!(
            backends.len(),
            clients.len(),
            "one client per backend required"
        );
        let all: Vec<GSlot> = backends
            .iter()
            .enumerate()
            .flat_map(|(be, c)| (0..c.n_slots).map(move |slot| GSlot { be, slot }))
            .collect();
        let last_used: Vec<AtomicU64> = all.iter().map(|_| AtomicU64::new(0)).collect();
        let sems: Vec<Arc<tokio::sync::Semaphore>> = all
            .iter()
            .map(|_| Arc::new(tokio::sync::Semaphore::new(1)))
            .collect();
        let down_until: Vec<AtomicU64> = (0..backends.len()).map(|_| AtomicU64::new(0)).collect();
        let failure_streak: Vec<AtomicU32> =
            (0..backends.len()).map(|_| AtomicU32::new(0)).collect();
        tracing::info!(
            "slot_manager n_backends={} total_slots={}",
            backends.len(),
            all.len()
        );
        Self {
            clients,
            all,
            last_used,
            sems,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            down_until,
            backend_cooldown: DEFAULT_BACKEND_COOLDOWN,
            failure_streak,
        }
    }

    /// Test hook: change the acquire timeout.
    pub fn with_acquire_timeout(mut self, d: Duration) -> Self {
        self.acquire_timeout = d;
        self
    }

    /// Test hook: change the backend failure cooldown.
    pub fn with_backend_cooldown(mut self, d: Duration) -> Self {
        self.backend_cooldown = d;
        self
    }

    /// Test hook: current cooldown deadline (nanos) for a backend.
    #[cfg(test)]
    pub fn down_until_for_test(&self, be: usize) -> u64 {
        self.down_until[be].load(Ordering::SeqCst)
    }

    /// Test hook: current failure streak for a backend.
    #[cfg(test)]
    pub fn failure_streak_for_test(&self, be: usize) -> u32 {
        self.failure_streak[be].load(Ordering::SeqCst)
    }

    /// Record a connection-level failure for `g`'s backend and exclude that
    /// backend from slot selection for the cooldown duration (circuit
    /// breaker).
    ///
    /// Deliberate deviation from the Python original: there, a dead backend
    /// keeps owning the "free/oldest" slot (LRU timestamps only advance on
    /// successful saves), so every subsequent request gets pinned to the
    /// dead slot. Here the failed backend is skipped while the remaining
    /// backends keep serving, and it re-enters the pool when the cooldown
    /// expires (a successful probe or request then keeps it in the pool).
    ///
    /// Escalating backoff: consecutive failures double the cooldown
    /// (5 s, 10 s, 20 s, 40 s, capped at 60 s) so a flapping backend stops
    /// chewing through client requests; any success resets the streak.
    pub fn report_failure(&self, g: GSlot) {
        let now = now_nanos();
        let streak = self.failure_streak[g.be].fetch_add(1, Ordering::SeqCst) + 1;
        let cooldown =
            (self.backend_cooldown * (1u32 << (streak - 1).min(4))).min(MAX_BACKEND_COOLDOWN);
        let until = now.saturating_add(cooldown.as_nanos() as u64);
        let prev = self.down_until[g.be].swap(until, Ordering::SeqCst);
        if prev <= now {
            tracing::warn!(
                "backend_down be={} cooldown_ms={} streak={streak}",
                g.be,
                cooldown.as_millis()
            );
        }
    }

    /// Clear a backend's failure state (cooldown + streak). Called by a
    /// successful probe or a successful request. Logs when a live cooldown
    /// is cut short.
    pub fn mark_backend_up(&self, be: usize) {
        let now = now_nanos();
        let prev = self.down_until[be].swap(0, Ordering::SeqCst);
        self.failure_streak[be].store(0, Ordering::SeqCst);
        if prev > now {
            tracing::info!("backend_up be={be} (cooldown cleared)");
        }
    }

    /// Record that a request reached `g`'s backend and got an HTTP answer
    /// (any status — reachability is what matters for the breaker).
    pub fn report_success(&self, g: GSlot) {
        self.mark_backend_up(g.be);
    }

    /// One background probe round (deviation from Python): for every
    /// backend still in failure cooldown, poll `GET /v1/models`; a
    /// successful answer clears the cooldown and streak immediately, so a
    /// recovered backend re-joins the slot pool without waiting out the
    /// cooldown. Returns the number of backends that recovered this round.
    pub async fn probe_down_backends(&self) -> usize {
        let now = now_nanos();
        let mut recovered = 0;
        for (be, until) in self.down_until.iter().enumerate() {
            let u = until.load(Ordering::SeqCst);
            if u == 0 || now >= u {
                continue;
            }
            if self.clients[be].get_model_id().await != "unknown" {
                self.mark_backend_up(be);
                recovered += 1;
            }
        }
        recovered
    }

    /// Mark the slot as used even though the request failed, so the
    /// free-slot-first selection does not keep pinning all traffic to a
    /// slot whose requests keep failing.
    ///
    /// Deliberate deviation from the Python original: `last_used` only
    /// advances on a successful save, so a slot (or a slot whose backend
    /// rejects every request) stayed "free" forever and intercepted all
    /// traffic.
    pub fn mark_used(&self, g: GSlot) {
        self.last_used[self.idx(g)].store(now_nanos(), Ordering::SeqCst);
    }

    pub fn clients(&self) -> &[Arc<dyn LlamaBackend>] {
        &self.clients
    }

    fn idx(&self, g: GSlot) -> usize {
        self.all.iter().position(|x| *x == g).expect("known slot")
    }

    fn last_used_of(&self, g: GSlot) -> u64 {
        self.last_used[self.idx(g)].load(Ordering::SeqCst)
    }

    /// Free (never used) slots first in insertion order; otherwise the
    /// least recently used one (port of `_get_free_or_oldest`). Panics only
    /// if the pool is empty (impossible: every backend has n_slots >= 1).
    fn pick(&self) -> GSlot {
        self.pick_filtered(|_| true)
            .expect("at least one slot is eligible (fail-open)")
    }

    /// Slot selection core (used by [`pick`] and [`SlotManager::acquire_excluding`]).
    ///
    /// Free (never used) slots first in insertion order; otherwise the
    /// least recently used one (port of `_get_free_or_oldest`).
    ///
    /// Circuit breaker: slots of backends still in cooldown are skipped —
    /// unless every backend is cooling down, in which case all slots are
    /// eligible again (fail-open: a single-backend deployment must keep
    /// retrying its own slots).
    ///
    /// Idle-aware (deviation from the Python original, which ignored slot
    /// occupancy): a slot that is currently held is never picked while a
    /// free/eligible idle slot exists — otherwise every concurrent request
    /// picks the first slot in order and serializes on it while the other
    /// slots sit idle.
    ///
    /// `extra` is an additional per-slot filter (e.g. "not the backend
    /// that just failed"); `None` when nothing passes it.
    fn pick_filtered<F: Fn(&GSlot) -> bool>(&self, extra: F) -> Option<GSlot> {
        let now = now_nanos();
        let any_up = self.down_until.iter().any(|d| {
            let u = d.load(Ordering::SeqCst);
            u == 0 || now >= u
        });
        let eligible = |g: &GSlot| {
            let u = self.down_until[g.be].load(Ordering::SeqCst);
            (u == 0 || now >= u || !any_up) && extra(g)
        };
        let idle = |g: &GSlot| self.sems[self.idx(*g)].available_permits() > 0;
        if let Some(g) = self
            .all
            .iter()
            .find(|g| eligible(g) && idle(g) && self.last_used_of(**g) == 0)
        {
            return Some(*g);
        }
        let mut best: Option<(GSlot, u64, bool)> = None;
        for &g in &self.all {
            if !eligible(&g) {
                continue;
            }
            let t = self.last_used_of(g);
            let g_idle = idle(&g);
            match best {
                None => best = Some((g, t, g_idle)),
                Some((_, bt, best_idle))
                    if (g_idle && !best_idle) || (t < bt && g_idle == best_idle) =>
                {
                    best = Some((g, t, g_idle));
                }
                _ => {}
            }
        }
        best.map(|(g, _, _)| g)
    }

    /// Acquire a slot (waiting up to `acquire_timeout`) and, when
    /// `restore_key` is given, restore that KV cache into the slot first
    /// (port of `acquire_for_request`).
    pub async fn acquire(&self, restore_key: Option<&str>) -> Result<SlotGuard, AcquireTimeout> {
        let g = self.pick();
        self.acquire_slot(g, restore_key).await
    }

    /// Like [`acquire`], but never picks a slot of backend `exclude_be`.
    /// Used to retry a connection-level failure on a *different* backend.
    /// Fails fast (no waiting) when no other backend has an eligible slot.
    pub async fn acquire_excluding(
        &self,
        exclude_be: usize,
        restore_key: Option<&str>,
    ) -> Result<SlotGuard, AcquireTimeout> {
        let Some(g) = self.pick_filtered(|c: &GSlot| c.be != exclude_be) else {
            tracing::warn!("acquire_excluding be={exclude_be} no other backend available");
            return Err(AcquireTimeout);
        };
        self.acquire_slot(g, restore_key).await
    }

    /// Wait for the picked slot's permit and optionally restore into it.
    async fn acquire_slot(
        &self,
        g: GSlot,
        restore_key: Option<&str>,
    ) -> Result<SlotGuard, AcquireTimeout> {
        let permit = match tokio::time::timeout(
            self.acquire_timeout,
            self.sems[self.idx(g)].clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::error!("acquire_semaphore_error g={g}: {e}");
                return Err(AcquireTimeout);
            }
            Err(_) => {
                tracing::error!("acquire_timeout g={g}");
                return Err(AcquireTimeout);
            }
        };
        let mut guard = SlotGuard {
            slot: g,
            restored: None,
            permit,
        };
        if let Some(key) = restore_key {
            let outcome = self.clients[g.be].restore_slot(g.slot, key).await;
            tracing::info!(
                "restore_before_chat g={g} key={} ok={}",
                short(key),
                matches!(outcome, RestoreOutcome::Restored)
            );
            guard.restored = Some(outcome);
        }
        Ok(guard)
    }

    /// Save the slot's KV cache under `key` and mark the slot used
    /// (port of `save_after`). `Ok(false)` on backend 500; `Err` on other
    /// failures (the caller maps that to a 500, as in the Python app).
    pub async fn save_after(&self, g: GSlot, key: &str) -> Result<bool, BackendError> {
        let ok = self.clients[g.be].save_slot(g.slot, key).await?;
        self.last_used[self.idx(g)].store(now_nanos(), Ordering::SeqCst);
        Ok(ok)
    }

    /// Release a held slot (port of `release`).
    pub fn release(&self, guard: SlotGuard) {
        drop(guard);
    }

    #[cfg(test)]
    pub(crate) fn set_last_used_for_test(&self, g: GSlot, nanos: u64) {
        self.last_used[self.idx(g)].store(nanos, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) async fn hold_slot_for_test(&self, g: GSlot) -> OwnedSemaphorePermit {
        self.sems[self.idx(g)]
            .clone()
            .acquire_owned()
            .await
            .expect("test semaphore")
    }
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
}

fn short(s: &str) -> String {
    s.chars().take(16).collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::llama_client::{JsonChat, RestoreOutcome, StreamChat};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream;
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, AtomicU8};
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// In-memory backend used to test selection/lock/save semantics.
    struct TestClient {
        saves: Arc<Mutex<Vec<(usize, String)>>>,
        restores: Arc<Mutex<Vec<(usize, String)>>>,
        /// 0 = Ok(true), 1 = Ok(false) (backend 500), 2 = Err
        save_mode: Arc<AtomicU8>,
        /// true = Restored, false = Failed
        restore_ok: Arc<AtomicBool>,
    }

    impl TestClient {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                saves: Arc::new(Mutex::new(Vec::new())),
                restores: Arc::new(Mutex::new(Vec::new())),
                save_mode: Arc::new(AtomicU8::new(0)),
                restore_ok: Arc::new(AtomicBool::new(true)),
            })
        }
    }

    #[async_trait]
    impl LlamaBackend for TestClient {
        async fn save_slot(&self, slot_id: usize, basename: &str) -> Result<bool, BackendError> {
            self.saves
                .lock()
                .await
                .push((slot_id, basename.to_string()));
            match self.save_mode.load(std::sync::atomic::Ordering::SeqCst) {
                0 => Ok(true),
                1 => Ok(false),
                _ => Err(BackendError::Other("mock save error".into())),
            }
        }
        async fn restore_slot(&self, slot_id: usize, basename: &str) -> RestoreOutcome {
            self.restores
                .lock()
                .await
                .push((slot_id, basename.to_string()));
            if self.restore_ok.load(std::sync::atomic::Ordering::SeqCst) {
                RestoreOutcome::Restored
            } else {
                RestoreOutcome::Failed
            }
        }
        async fn get_model_id(&self) -> String {
            "test-model".into()
        }
        async fn chat_stream(
            &self,
            _b: &Value,
            _s: Option<usize>,
        ) -> Result<StreamChat, BackendError> {
            Ok(StreamChat {
                status: 200,
                stream: Box::pin(stream::iter(vec![Ok::<Bytes, String>(Bytes::new())])),
            })
        }
        async fn chat_json(&self, _b: &Value, _s: Option<usize>) -> Result<JsonChat, BackendError> {
            Ok(JsonChat::Object(Value::Object(Default::default())))
        }
    }

    fn make(n_backends: usize, n_slots: usize) -> (Arc<SlotManager>, Vec<Arc<TestClient>>) {
        let backends: Vec<BackendConf> = (0..n_backends)
            .map(|i| BackendConf {
                url: format!("http://b{i}"),
                n_slots,
                slot_save_path: None,
            })
            .collect();
        let raw: Vec<Arc<TestClient>> = (0..n_backends).map(|_| TestClient::new()).collect();
        let clients: Vec<Arc<dyn LlamaBackend>> = raw
            .iter()
            .map(|c| Arc::clone(c) as Arc<dyn LlamaBackend>)
            .collect();
        let sm = Arc::new(SlotManager::new(&backends, clients));
        (sm, raw)
    }

    #[tokio::test]
    async fn free_slot_picked_first_in_order() {
        let (sm, _) = make(1, 3);
        // never used -> always the first slot
        let g1 = sm.acquire(None).await.unwrap();
        assert_eq!(g1.slot, GSlot { be: 0, slot: 0 });
        sm.release(g1);
        let g2 = sm.acquire(None).await.unwrap();
        assert_eq!(g2.slot, GSlot { be: 0, slot: 0 });
        sm.release(g2);
    }

    #[tokio::test]
    async fn used_slots_are_skipped_then_oldest_wins() {
        let (sm, _raw) = make(1, 3);
        // mark slot 0 and slot 1 used, slot 0 older
        sm.set_last_used_for_test(GSlot { be: 0, slot: 0 }, 100);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 1 }, 200);
        let g = sm.acquire(None).await.unwrap();
        // slot 2 is still free -> picked before any LRU logic
        assert_eq!(g.slot, GSlot { be: 0, slot: 2 });
        sm.release(g);
        // now all used -> oldest (slot 0) is picked
        sm.set_last_used_for_test(GSlot { be: 0, slot: 2 }, 300);
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        sm.release(g);
    }

    #[tokio::test]
    async fn multi_backend_slot_ordering() {
        let (sm, _) = make(2, 2);
        // mark all of backend 0 used
        sm.set_last_used_for_test(GSlot { be: 0, slot: 0 }, 10);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 1 }, 20);
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        sm.release(g);
    }
    #[tokio::test]
    async fn restore_called_only_when_key_given() {
        let (sm, raw) = make(1, 2);
        let g = sm.acquire(Some("key-abcdef0123456789")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.restored, None);
        sm.release(g);

        let c = &raw[0];
        let restores = c.restores.lock().await.clone();
        assert_eq!(restores, vec![(0usize, "key-abcdef0123456789".to_string())]);

        // restore failure is reported, not swallowed
        raw[0]
            .restore_ok
            .store(false, std::sync::atomic::Ordering::SeqCst);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 0 }, 1);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 1 }, 2);
        let g = sm.acquire(Some("k2")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Failed));
        sm.release(g);
    }

    #[tokio::test]
    async fn save_after_saves_and_marks_used() {
        let (sm, raw) = make(1, 2);
        assert!(sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.unwrap());
        let saves = raw[0].saves.lock().await.clone();
        assert_eq!(saves, vec![(0usize, "k1".to_string())]);
        // slot 0 now used -> next free pick is slot 1
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 1 });
        sm.release(g);
    }

    #[tokio::test]
    async fn save_after_backend_500_returns_false_but_still_marks_used() {
        let (sm, raw) = make(1, 2);
        raw[0]
            .save_mode
            .store(1, std::sync::atomic::Ordering::SeqCst);
        assert!(!sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.unwrap());
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 1 });
        sm.release(g);
    }

    #[tokio::test]
    async fn save_after_other_error_propagates_and_keeps_slot_free() {
        let (sm, raw) = make(1, 2);
        raw[0]
            .save_mode
            .store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.is_err());
        // last_used not updated -> slot 0 still "free"
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        sm.release(g);
    }

    #[tokio::test]
    async fn second_acquire_waits_until_release() {
        let (sm, _) = make(1, 1);
        let g1 = sm.acquire(None).await.unwrap();
        let sm2 = Arc::clone(&sm);
        let t = tokio::spawn(async move { sm2.acquire(None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!t.is_finished(), "second acquire should be blocked");
        sm.release(g1);
        let g2 = t.await.unwrap().unwrap();
        assert_eq!(g2.slot, GSlot { be: 0, slot: 0 });
        sm.release(g2);
    }

    #[tokio::test]
    async fn acquire_times_out_when_slot_held() {
        let backends = vec![BackendConf {
            url: "http://b0".to_string(),
            n_slots: 1,
            slot_save_path: None,
        }];
        let client = TestClient::new();
        let clients: Vec<Arc<dyn LlamaBackend>> =
            vec![Arc::clone(&client) as Arc<dyn LlamaBackend>];
        let sm = Arc::new(
            SlotManager::new(&backends, clients).with_acquire_timeout(Duration::from_millis(100)),
        );
        let _permit = sm.hold_slot_for_test(GSlot { be: 0, slot: 0 }).await;
        let res = sm.acquire(None).await;
        assert!(matches!(res, Err(AcquireTimeout)));
    }

    #[tokio::test]
    async fn failed_backend_is_skipped_while_others_available() {
        let (sm, _) = make(2, 2);
        sm.report_failure(GSlot { be: 0, slot: 0 });
        // free-first path: backend 0's slots are ineligible
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        sm.release(g);
        // LRU path: all used -> oldest among eligible (still backend 1)
        sm.set_last_used_for_test(GSlot { be: 1, slot: 0 }, 100);
        sm.set_last_used_for_test(GSlot { be: 1, slot: 1 }, 200);
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        sm.release(g);
    }

    #[tokio::test]
    async fn all_backends_down_fails_open() {
        let (sm, _) = make(2, 1);
        sm.report_failure(GSlot { be: 0, slot: 0 });
        sm.report_failure(GSlot { be: 1, slot: 0 });
        // everything cooling -> still pick something (first slot in order)
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        sm.release(g);
    }

    #[tokio::test]
    async fn pick_skips_held_slot_in_favor_of_idle() {
        let (sm, _) = make(1, 3);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 0 }, 100); // oldest
        sm.set_last_used_for_test(GSlot { be: 0, slot: 1 }, 200);
        // hold the oldest slot; selection must not wait on it
        let _permit = sm.hold_slot_for_test(GSlot { be: 0, slot: 0 }).await;
        // free + idle -> slot 2 (the only never-used slot)
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 2 });
        sm.release(g);
        // all used; oldest slot 0 is busy, slots 1+2 idle -> LRU among idle
        sm.set_last_used_for_test(GSlot { be: 0, slot: 2 }, 300);
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 1 });
        sm.release(g);
    }

    #[tokio::test]
    async fn backend_cooldown_expires() {
        let backends = vec![
            BackendConf {
                url: "http://b0".to_string(),
                n_slots: 1,
                slot_save_path: None,
            },
            BackendConf {
                url: "http://b1".to_string(),
                n_slots: 1,
                slot_save_path: None,
            },
        ];
        let raw: Vec<Arc<TestClient>> = vec![TestClient::new(), TestClient::new()];
        let clients: Vec<Arc<dyn LlamaBackend>> = raw
            .iter()
            .map(|c| Arc::clone(c) as Arc<dyn LlamaBackend>)
            .collect();
        let sm = Arc::new(
            SlotManager::new(&backends, clients).with_backend_cooldown(Duration::from_millis(50)),
        );
        sm.report_failure(GSlot { be: 0, slot: 0 });
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        sm.release(g);
        // after the cooldown, backend 0 is eligible again (it is free)
        tokio::time::sleep(Duration::from_millis(80)).await;
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        sm.release(g);
    }

    fn cooldown_secs_left(sm: &SlotManager, be: usize) -> f64 {
        let until = sm.down_until_for_test(be);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            * 1e9;
        (until as f64 - now) / 1e9
    }

    #[tokio::test]
    async fn report_failure_escalates_backoff() {
        let (sm, _) = make(1, 1); // default 5 s base cooldown
        let g = GSlot { be: 0, slot: 0 };
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 5.0).abs() < 0.05);
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 10.0).abs() < 0.05);
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 20.0).abs() < 0.05);
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 40.0).abs() < 0.05);
        // capped at 60 s
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 60.0).abs() < 0.05);
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 60.0).abs() < 0.05);
        assert_eq!(sm.failure_streak_for_test(0), 6);
    }

    #[tokio::test]
    async fn report_success_resets_cooldown_and_streak() {
        let (sm, _) = make(1, 1);
        let g = GSlot { be: 0, slot: 0 };
        sm.report_failure(g);
        sm.report_failure(g); // streak 2 -> 10 s
        assert!((cooldown_secs_left(&sm, 0) - 10.0).abs() < 0.05);
        sm.report_success(g);
        assert_eq!(sm.down_until_for_test(0), 0);
        assert_eq!(sm.failure_streak_for_test(0), 0);
        // next failure starts the ladder again
        sm.report_failure(g);
        assert!((cooldown_secs_left(&sm, 0) - 5.0).abs() < 0.05);
    }

    #[tokio::test]
    async fn probe_down_backends_recovers_up_backend() {
        let (sm, _) = make(2, 1);
        sm.report_failure(GSlot { be: 1, slot: 0 });
        assert!(sm.down_until_for_test(1) > 0);
        // TestClient answers /v1/models, so one probe round recovers it
        let recovered = sm.probe_down_backends().await;
        assert_eq!(recovered, 1);
        assert_eq!(sm.down_until_for_test(1), 0);
        assert_eq!(sm.failure_streak_for_test(1), 0);
        // healthy backends are not counted
        assert_eq!(sm.probe_down_backends().await, 0);
    }

    #[tokio::test]
    async fn acquire_excluding_picks_other_backend() {
        let (sm, _) = make(2, 1);
        let g = sm.acquire_excluding(0, None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        sm.release(g);
        // single backend: no retry target -> fails fast, no 300 s wait
        let (single, _) = make(1, 1);
        let t0 = std::time::Instant::now();
        assert!(single.acquire_excluding(0, None).await.is_err());
        assert!(t0.elapsed() < Duration::from_secs(1));
    }
}
