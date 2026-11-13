//! Slot management (port of `slot_manager.py`).
//!
//! - `GSlot = (backend_id, local_slot_id)`
//! - selection, per the `SLOT_ROUTING_POLICY` setting:
//!   - `lru` (default): free (never used) slots first, in insertion order;
//!     otherwise the least recently used slot (smallest `last_used`
//!     timestamp)
//!   - `key_affinity`: for a request that carries a KV restore key, an
//!     idle slot already running that key is preferred before the
//!     free/LRU tiers — the restore then becomes a zero-wire-call skip
//! - each slot is guarded by a `Semaphore(1)` — the Rust equivalent of the
//!   per-slot `asyncio.Lock`
//! - `acquire` waits up to `acquire_timeout` (default 300 s, same as the
//!   `asyncio.wait_for` wrapper in `app.py`) and restores before chat when a
//!   restore key is given. Every slot also tracks the last KV cache it is
//!   known to be running (a successful restore or save): a restore into a
//!   slot already running the requested key is skipped (no backend call).
//! - `save_after` saves the KV cache and updates the LRU timestamp; a
//!   failed save (backend 500 or network/other error) is retried up to
//!   `SAVE_RETRIES` times, waiting `SAVE_RETRY_DELAY_MS` milliseconds
//!   between attempts

use crate::config::{
    BackendConf, DEFAULT_SAVE_RETRIES, DEFAULT_SAVE_RETRY_DELAY_MS, RoutingPolicy,
};
use lpcache_plugins::{BackendError, LlamaBackend, RestoreOutcome};
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
    /// `Some(outcome)` when a restore was attempted before the chat — or
    /// when the picked slot was already running the requested KV cache and
    /// the restore was skipped (in which case the outcome is `Restored`:
    /// the slot effectively holds the cache, so the meta LRU touch and the
    /// request log's restored flag still apply).
    pub restored: Option<RestoreOutcome>,
    /// Holds the slot lock until the guard is dropped / released.
    #[allow(dead_code)]
    permit: OwnedSemaphorePermit,
}

impl SlotGuard {
    /// Explicit release; the permit is dropped here (also released on drop).
    pub fn release(self) {}
}

/// The tier that won in slot selection (for observability and tests): the
/// routing policy walks these tiers in order and stops at the first slot
/// that matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickReason {
    /// An idle slot already running the requested KV cache key (the
    /// `KeyAffinity` policy, with a restore key).
    KeyFree,
    /// An idle never-used slot in insertion order.
    Free,
    /// The least recently used slot (an idle one wins the tie-break).
    Lru,
}

pub struct SlotManager {
    clients: Vec<Arc<dyn LlamaBackend>>,
    all: Vec<GSlot>,
    last_used: Vec<AtomicU64>,
    /// One entry per slot: the last KV cache key the slot is known to be
    /// running — set by a successful restore or a successful save, cleared
    /// after a failed restore (the slot's KV state is then unknown).
    /// `None` = unknown (fresh slot, or state invalidated).
    last_kv: Vec<std::sync::Mutex<Option<String>>>,
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
    /// One entry per backend: whether that backend supports KV cache
    /// save/restore (`BackendConf::kv_cache`). Restores are only ever
    /// attempted on slots of enabled backends.
    kv_enabled: Vec<bool>,
    /// `SAVE_RETRIES`: retries after a failed KV cache save (backend 500 or
    /// network/other error) before giving up (`0` = no retry).
    save_retries: usize,
    /// `SAVE_RETRY_DELAY_MS`: delay in milliseconds between save retry
    /// attempts.
    save_retry_delay: Duration,
    /// `SLOT_ROUTING_POLICY`: how slots are picked for a request (default
    /// `Lru`; `KeyAffinity` adds the key-free tier before free/LRU).
    policy: RoutingPolicy,
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
        let last_kv: Vec<std::sync::Mutex<Option<String>>> =
            all.iter().map(|_| std::sync::Mutex::new(None)).collect();
        let sems: Vec<Arc<tokio::sync::Semaphore>> = all
            .iter()
            .map(|_| Arc::new(tokio::sync::Semaphore::new(1)))
            .collect();
        let down_until: Vec<AtomicU64> = (0..backends.len()).map(|_| AtomicU64::new(0)).collect();
        let failure_streak: Vec<AtomicU32> =
            (0..backends.len()).map(|_| AtomicU32::new(0)).collect();
        let kv_enabled: Vec<bool> = backends.iter().map(|b| b.kv_cache).collect();
        tracing::info!(
            "slot_manager n_backends={} total_slots={} kv_cache={:?}",
            backends.len(),
            all.len(),
            kv_enabled
        );
        Self {
            clients,
            all,
            last_used,
            last_kv,
            sems,
            acquire_timeout: DEFAULT_ACQUIRE_TIMEOUT,
            down_until,
            backend_cooldown: DEFAULT_BACKEND_COOLDOWN,
            failure_streak,
            kv_enabled,
            save_retries: DEFAULT_SAVE_RETRIES,
            save_retry_delay: Duration::from_millis(DEFAULT_SAVE_RETRY_DELAY_MS),
            policy: RoutingPolicy::default(),
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

    /// Change the KV cache save retry policy (`SAVE_RETRIES` +
    /// `SAVE_RETRY_DELAY_MS`): a failed save is retried up to `retries`
    /// times, waiting `delay` between attempts.
    pub fn with_save_retry(mut self, retries: usize, delay: Duration) -> Self {
        self.save_retries = retries;
        self.save_retry_delay = delay;
        self
    }

    /// Set the slot routing policy (`SLOT_ROUTING_POLICY`; the default,
    /// `Lru`, keeps the original free-first/LRU pick).
    pub fn with_routing_policy(mut self, policy: RoutingPolicy) -> Self {
        self.policy = policy;
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

    /// Live snapshot of every slot (occupancy + LRU age) for
    /// observability (the dashboard plugin's slot grid).
    pub fn slot_snapshot(&self) -> Vec<lpcache_plugins::SlotStatus> {
        let now = now_nanos();
        self.all
            .iter()
            .map(|&g| {
                let t = self.last_used[self.idx(g)].load(Ordering::SeqCst);
                lpcache_plugins::SlotStatus {
                    be: g.be,
                    slot: g.slot,
                    busy: self.sems[self.idx(g)].available_permits() == 0,
                    last_used_secs: if t == 0 {
                        None
                    } else {
                        Some((now.saturating_sub(t)) as f64 / 1e9)
                    },
                }
            })
            .collect()
    }

    /// Circuit-breaker state of every backend (index = backend index),
    /// for observability (the dashboard plugin's backend table).
    pub fn breaker_state(&self) -> Vec<lpcache_plugins::BackendBreaker> {
        let now = now_nanos();
        self.down_until
            .iter()
            .enumerate()
            .map(|(be, d)| {
                let until = d.load(Ordering::SeqCst);
                lpcache_plugins::BackendBreaker {
                    down: until > now,
                    cooldown_secs: until.saturating_sub(now) as f64 / 1e9,
                    failure_streak: self.failure_streak[be].load(Ordering::SeqCst),
                }
            })
            .collect()
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

    /// The KV cache key `g` is currently known to be running, if any
    /// (see `last_kv`). The lock is only held for the copy — never across
    /// an `.await`.
    fn slot_kv_key(&self, g: GSlot) -> Option<String> {
        self.last_kv[self.idx(g)].lock().unwrap().clone()
    }

    /// Update the KV cache key `g` is considered to be running: `Some(key)`
    /// after a successful restore or save of `key`, `None` to forget.
    fn set_slot_kv_key(&self, g: GSlot, key: Option<&str>) {
        *self.last_kv[self.idx(g)].lock().unwrap() = key.map(str::to_string);
    }

    /// Test hook: the KV cache key `g` is currently considered to be
    /// running, if any.
    #[cfg(test)]
    pub(crate) fn last_kv_for_test(&self, g: GSlot) -> Option<String> {
        self.slot_kv_key(g)
    }

    /// Free (never used) slots first in insertion order; otherwise the
    /// least recently used one (port of `_get_free_or_oldest`). Panics only
    /// if the pool is empty (impossible: every backend has n_slots >= 1).
    fn pick(&self) -> GSlot {
        self.pick_filtered(|_| true, None)
            .map(|(g, _)| g)
            .expect("at least one slot is eligible (fail-open)")
    }

    /// Slot selection core (used by [`pick`] and [`SlotManager::acquire_excluding`]).
    ///
    /// Tiers, in order:
    /// - [`PickReason::KeyFree`] — only when `key_free` is `Some(key)` (the
    ///   `key_affinity` routing policy with a restore key): an idle slot
    ///   already running `key`, in insertion order. The post-pick restore is
    ///   then a zero-wire-call skip.
    /// - [`PickReason::Free`] — idle never-used slots in insertion order;
    /// - [`PickReason::Lru`] — otherwise the least recently used one (port
    ///   of `_get_free_or_oldest`).
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
    fn pick_filtered<F: Fn(&GSlot) -> bool>(
        &self,
        extra: F,
        key_free: Option<&str>,
    ) -> Option<(GSlot, PickReason)> {
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
        // Tier 1: an idle slot already running the requested key. (A slot
        // only knows a key after a successful restore/save, so it is by
        // definition a used slot — the "never used" condition of the free
        // tier would make this tier dead.)
        if let Some(key) = key_free
            && let Some(g) = self
                .all
                .iter()
                .find(|g| eligible(g) && idle(g) && self.slot_kv_key(**g).as_deref() == Some(key))
        {
            return Some((*g, PickReason::KeyFree));
        }
        // Tier 2: idle never-used slots in insertion order.
        if let Some(g) = self
            .all
            .iter()
            .find(|g| eligible(g) && idle(g) && self.last_used_of(**g) == 0)
        {
            return Some((*g, PickReason::Free));
        }
        // Tier 3: least recently used, idle slots first.
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
        best.map(|(g, _, _)| (g, PickReason::Lru))
    }

    /// Select the slot for an acquire that may carry a KV restore.
    ///
    /// When a restore key is given, slots of backends with `kv_cache`
    /// enabled are preferred — the KV file can only be loaded there. If no
    /// slot of an enabled backend is eligible, the restore key is dropped
    /// (warned) and a plain pick is made: the request runs a full prefill
    /// instead of failing.
    ///
    /// Under the `key_affinity` routing policy, the pick additionally
    /// prefers an idle slot already running the key (the key-free tier), so
    /// the restore becomes a zero-wire-call skip.
    fn pick_for_acquire<'a>(&self, restore_key: Option<&'a str>) -> (GSlot, Option<&'a str>) {
        let Some(key) = restore_key else {
            return (self.pick(), None);
        };
        let key_free = (self.policy == RoutingPolicy::KeyAffinity).then_some(key);
        match self.pick_filtered(|g| self.kv_enabled[g.be], key_free) {
            Some((g, _)) => (g, Some(key)),
            None => {
                tracing::warn!("restore_skipped key={key} reason=no_kv_enabled_backend");
                (self.pick(), None)
            }
        }
    }

    /// Acquire a slot (waiting up to `acquire_timeout`) and, when
    /// `restore_key` is given, restore that KV cache into the slot first
    /// (port of `acquire_for_request`).
    pub async fn acquire(&self, restore_key: Option<&str>) -> Result<SlotGuard, AcquireTimeout> {
        let (g, restore) = self.pick_for_acquire(restore_key);
        self.acquire_slot(g, restore).await
    }

    /// Like [`acquire`], but never picks a slot of backend `exclude_be`.
    /// Used to retry a connection-level failure on a *different* backend.
    /// Fails fast (no waiting) when no other backend has an eligible slot.
    pub async fn acquire_excluding(
        &self,
        exclude_be: usize,
        restore_key: Option<&str>,
    ) -> Result<SlotGuard, AcquireTimeout> {
        let (g, restore) = match restore_key {
            Some(key) => {
                let key_free = (self.policy == RoutingPolicy::KeyAffinity).then_some(key);
                match self.pick_filtered(
                    |c: &GSlot| c.be != exclude_be && self.kv_enabled[c.be],
                    key_free,
                ) {
                    Some((g, _)) => (g, Some(key)),
                    None => {
                        let Some(g) = self
                            .pick_filtered(|c: &GSlot| c.be != exclude_be, None)
                            .map(|(g, _)| g)
                        else {
                            tracing::warn!(
                                "acquire_excluding be={exclude_be} no other backend available"
                            );
                            return Err(AcquireTimeout);
                        };
                        tracing::warn!("restore_skipped key={key} reason=no_kv_enabled_backend");
                        (g, None)
                    }
                }
            }
            None => {
                let Some(g) = self
                    .pick_filtered(|c: &GSlot| c.be != exclude_be, None)
                    .map(|(g, _)| g)
                else {
                    tracing::warn!("acquire_excluding be={exclude_be} no other backend available");
                    return Err(AcquireTimeout);
                };
                (g, None)
            }
        };
        self.acquire_slot(g, restore).await
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
            // The slot is already running this exact KV cache (a previous
            // restore or save left it there): skip the backend call entirely
            // and count the acquire as restored, so the meta LRU touch and
            // the request log's restored flag still apply.
            if self.slot_kv_key(g).as_deref() == Some(key) {
                tracing::info!(
                    "restore_skipped g={g} key={} reason=already_running",
                    short(key)
                );
                guard.restored = Some(RestoreOutcome::Restored);
            } else {
                let outcome = self.clients[g.be].restore_slot(g.slot, key).await;
                tracing::info!(
                    "restore_before_chat g={g} key={} ok={}",
                    short(key),
                    matches!(outcome, RestoreOutcome::Restored)
                );
                match outcome {
                    RestoreOutcome::Restored => self.set_slot_kv_key(g, Some(key)),
                    // Rejected/Failed: the slot's KV state is unknown now —
                    // forget what we thought it was running.
                    _ => self.set_slot_kv_key(g, None),
                }
                guard.restored = Some(outcome);
            }
        }
        Ok(guard)
    }

    /// Save the slot's KV cache under `key` and mark the slot used
    /// (port of `save_after`, plus auto-retry).
    ///
    /// Auto-retry (deviation from the Python original): a failed save —
    /// backend 500 (`Ok(false)`) or any other error (`Err`) — is retried up
    /// to `save_retries` times, waiting `save_retry_delay` between attempts.
    /// The final outcome is returned unchanged: `Ok(false)` on a 500 after
    /// all attempts, `Err` when every attempt errored (the caller maps that
    /// to a 500, as in the Python app). The slot is marked used on any
    /// `Ok` outcome (including `Ok(false)`), as before.
    ///
    /// On a successful save the slot's KV cache is exactly `key`, so the
    /// per-slot tracking is updated to it (a later restore of the same key
    /// into this slot is then skipped); a failed save leaves the tracking
    /// untouched (the slot's KV is unchanged by a save attempt).
    pub async fn save_after(&self, g: GSlot, key: &str) -> Result<bool, BackendError> {
        let mut last = self.clients[g.be].save_slot(g.slot, key).await;
        let mut attempt = 0;
        while !matches!(&last, Ok(true)) && attempt < self.save_retries {
            attempt += 1;
            let cause = match &last {
                Ok(false) => "backend returned 500".to_string(),
                Ok(true) => unreachable!("the loop condition stops on Ok(true)"),
                Err(e) => e.to_string(),
            };
            tracing::warn!(
                "save_retry g={g} key={} attempt={attempt}/{} delay_ms={} cause={cause}",
                short(key),
                self.save_retries,
                self.save_retry_delay.as_millis()
            );
            tokio::time::sleep(self.save_retry_delay).await;
            last = self.clients[g.be].save_slot(g.slot, key).await;
        }
        match &last {
            Ok(true) => {
                self.last_used[self.idx(g)].store(now_nanos(), Ordering::SeqCst);
                self.set_slot_kv_key(g, Some(key));
            }
            Ok(false) => {
                // backend 500: the slot is marked used as before, but the KV
                // save did not land — the tracking stays as it was
                self.last_used[self.idx(g)].store(now_nanos(), Ordering::SeqCst);
            }
            Err(_) => {}
        }
        last
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
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream;
    use lpcache_plugins::{JsonChat, RestoreOutcome, StreamChat};
    use serde_json::Value;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// In-memory backend used to test selection/lock/save semantics.
    struct TestClient {
        saves: Arc<Mutex<Vec<(usize, String)>>>,
        restores: Arc<Mutex<Vec<(usize, String)>>>,
        /// 0 = Ok(true), 1 = Ok(false) (backend 500), 2 = Err
        save_mode: Arc<AtomicU8>,
        /// The first N save calls return a transient `Err` (0 = disabled);
        /// later calls fall through to `save_mode`.
        save_fail_first_n: Arc<AtomicUsize>,
        /// true = Restored, false = Failed
        restore_ok: Arc<AtomicBool>,
    }

    impl TestClient {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                saves: Arc::new(Mutex::new(Vec::new())),
                restores: Arc::new(Mutex::new(Vec::new())),
                save_mode: Arc::new(AtomicU8::new(0)),
                save_fail_first_n: Arc::new(AtomicUsize::new(0)),
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
            if self
                .save_fail_first_n
                .load(std::sync::atomic::Ordering::SeqCst)
                > 0
            {
                self.save_fail_first_n
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                return Err(BackendError::Other("mock transient save error".into()));
            }
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
        make_kv(n_backends, n_slots, vec![])
    }

    /// Like [`make`], but `no_kv[i]` disables KV save/restore on backend
    /// `i` (backends not listed keep the default `kv_cache: true`).
    fn make_kv(
        n_backends: usize,
        n_slots: usize,
        no_kv: Vec<usize>,
    ) -> (Arc<SlotManager>, Vec<Arc<TestClient>>) {
        let backends: Vec<BackendConf> = (0..n_backends)
            .map(|i| BackendConf {
                url: format!("http://b{i}"),
                n_slots,
                slot_save_path: None,
                kv_cache: !no_kv.contains(&i),
            })
            .collect();
        let raw: Vec<Arc<TestClient>> = (0..n_backends).map(|_| TestClient::new()).collect();
        let clients: Vec<Arc<dyn LlamaBackend>> = raw
            .iter()
            .map(|c| Arc::clone(c) as Arc<dyn LlamaBackend>)
            .collect();
        let sm = Arc::new(
            SlotManager::new(&backends, clients)
                // keep retry-driven tests fast (the retry count stays at the
                // production default, only the delay shrinks to 1 ms)
                .with_save_retry(DEFAULT_SAVE_RETRIES, Duration::from_millis(1)),
        );
        (sm, raw)
    }

    /// Like [`make`], but with the given slot routing policy.
    fn make_policy(
        n_backends: usize,
        n_slots: usize,
        policy: RoutingPolicy,
    ) -> (Arc<SlotManager>, Vec<Arc<TestClient>>) {
        let backends: Vec<BackendConf> = (0..n_backends)
            .map(|i| BackendConf {
                url: format!("http://b{i}"),
                n_slots,
                slot_save_path: None,
                kv_cache: true,
            })
            .collect();
        let raw: Vec<Arc<TestClient>> = (0..n_backends).map(|_| TestClient::new()).collect();
        let clients: Vec<Arc<dyn LlamaBackend>> = raw
            .iter()
            .map(|c| Arc::clone(c) as Arc<dyn LlamaBackend>)
            .collect();
        let sm = Arc::new(
            SlotManager::new(&backends, clients)
                .with_save_retry(DEFAULT_SAVE_RETRIES, Duration::from_millis(1))
                .with_routing_policy(policy),
        );
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

    /// Shared setup for the routing-policy tests: slot 0 runs "k" (older),
    /// slot 1 runs "other" (younger) — under `lru`, a repeat of "k" would
    /// be picked on slot 1 (the LRU slot) and need a real restore.
    async fn two_slots_with_keys(sm: &Arc<SlotManager>) {
        let g1 = sm.acquire(Some("k")).await.unwrap();
        assert_eq!(g1.slot, GSlot { be: 0, slot: 0 });
        let s1 = g1.slot;
        sm.release(g1);
        sm.mark_used(s1);
        let g2 = sm.acquire(Some("other")).await.unwrap();
        assert_eq!(g2.slot, GSlot { be: 0, slot: 1 });
        let s2 = g2.slot;
        sm.release(g2);
        sm.mark_used(s2);
        // make slot 1 the LRU pick (slot 0 is "newer")
        sm.set_last_used_for_test(GSlot { be: 0, slot: 0 }, 1000);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 1 }, 500);
    }

    #[tokio::test]
    async fn key_affinity_picks_idle_slot_running_key() {
        let (sm, raw) = make_policy(1, 2, RoutingPolicy::KeyAffinity);
        two_slots_with_keys(&sm).await;
        // key_affinity: the idle slot running "k" wins before the LRU tier
        let g = sm.acquire(Some("k")).await.unwrap();
        assert_eq!(
            g.slot,
            GSlot { be: 0, slot: 0 },
            "the key-affinity pick must prefer the slot running the key"
        );
        // the post-pick skip counts as restored...
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        // ...but issues no redundant restore wire call (2 = the two
        // restores of the setup)
        assert_eq!(raw[0].restores.lock().await.len(), 2);
        sm.release(g);
    }

    #[tokio::test]
    async fn lru_policy_ignores_key_on_pick() {
        let (sm, raw) = make_policy(1, 2, RoutingPolicy::Lru);
        two_slots_with_keys(&sm).await;
        // lru (the default): the key never influences the pick — the
        // oldest slot (slot 1, running "other") wins and the restore is
        // actually issued there
        let g = sm.acquire(Some("k")).await.unwrap();
        assert_eq!(
            g.slot,
            GSlot { be: 0, slot: 1 },
            "the lru pick must ignore the key"
        );
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        assert_eq!(raw[0].restores.lock().await.len(), 3);
        sm.release(g);
    }

    #[tokio::test]
    async fn key_affinity_without_key_matches_lru_pick() {
        // no restore key: the policy has no effect (free-first ordering)
        let (sm, raw) = make_policy(1, 2, RoutingPolicy::KeyAffinity);
        let g1 = sm.acquire(None).await.unwrap();
        let s1 = g1.slot;
        sm.release(g1);
        sm.mark_used(s1);
        let g2 = sm.acquire(None).await.unwrap();
        assert_eq!(g2.slot, GSlot { be: 0, slot: 1 }, "free slot first");
        assert!(raw[0].restores.lock().await.is_empty());
        sm.release(g2);
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
    async fn restore_skipped_when_slot_already_running_key() {
        let (sm, raw) = make(1, 2);
        // first acquire: slot 0 does not run k1 yet -> the restore is issued
        let g = sm.acquire(Some("k1")).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        // second acquire, same key: the slot is already running k1 (the
        // successful restore recorded it) -> no second backend call, but the
        // guard still reports the slot as restored
        let g = sm.acquire(Some("k1")).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        assert_eq!(
            *raw[0].restores.lock().await,
            vec![(0usize, "k1".to_string())]
        );
        // a different key is never skipped
        let g = sm.acquire(Some("k2")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        assert_eq!(
            *raw[0].restores.lock().await,
            vec![(0usize, "k1".to_string()), (0usize, "k2".to_string())]
        );
    }

    #[tokio::test]
    async fn save_updates_last_kv_tracking() {
        let (sm, raw) = make(1, 1);
        // a successful save leaves the slot running exactly `key`
        assert!(sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.unwrap());
        assert_eq!(
            sm.last_kv_for_test(GSlot { be: 0, slot: 0 }),
            Some("k1".to_string())
        );
        // a later restore of the same key into the same slot is skipped
        let g = sm.acquire(Some("k1")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        assert!(raw[0].restores.lock().await.is_empty());
        // ... but a different key is still restored
        let g = sm.acquire(Some("k2")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        assert_eq!(
            *raw[0].restores.lock().await,
            vec![(0usize, "k2".to_string())]
        );
    }

    #[tokio::test]
    async fn failed_restore_forgets_last_kv_tracking() {
        let (sm, raw) = make(1, 1);
        // a failed restore leaves the slot's KV state unknown -> the
        // tracking must be cleared so a later restore of the same key is
        // not wrongly skipped
        raw[0]
            .restore_ok
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let g = sm.acquire(Some("k1")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Failed));
        sm.release(g);
        assert_eq!(sm.last_kv_for_test(GSlot { be: 0, slot: 0 }), None);
        raw[0]
            .restore_ok
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let g = sm.acquire(Some("k1")).await.unwrap();
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        // both attempts reached the backend (the first failed, the second
        // was re-issued because the tracking had been cleared)
        assert_eq!(
            *raw[0].restores.lock().await,
            vec![(0usize, "k1".to_string()), (0usize, "k1".to_string())]
        );
        assert_eq!(
            sm.last_kv_for_test(GSlot { be: 0, slot: 0 }),
            Some("k1".to_string())
        );
    }

    #[tokio::test]
    async fn restore_prefers_kv_enabled_backend() {
        // backend 0 has kv_cache disabled -> the restore must land on
        // backend 1 even though backend 0's slots are "older"
        let (sm, raw) = make_kv(2, 1, vec![0]);
        sm.set_last_used_for_test(GSlot { be: 0, slot: 0 }, 1);
        sm.set_last_used_for_test(GSlot { be: 1, slot: 0 }, 2);
        let g = sm.acquire(Some("key-kv1234567890abcd")).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        assert!(raw[0].restores.lock().await.is_empty());
        assert_eq!(
            *raw[1].restores.lock().await,
            vec![(0usize, "key-kv1234567890abcd".to_string())]
        );
    }

    #[tokio::test]
    async fn restore_skipped_when_no_kv_enabled_backend() {
        let (sm, raw) = make_kv(2, 1, vec![0, 1]);
        let g = sm.acquire(Some("key-kv1234567890abcd")).await.unwrap();
        // a slot is still acquired (full prefill), but no restore is issued
        assert_eq!(g.restored, None);
        sm.release(g);
        assert!(raw[0].restores.lock().await.is_empty());
        assert!(raw[1].restores.lock().await.is_empty());
    }

    #[tokio::test]
    async fn acquire_excluding_respects_kv_flag() {
        // both backends kv-enabled: excluding backend 0 restores on 1
        let (sm, raw) = make_kv(2, 1, vec![]);
        let g = sm.acquire_excluding(0, Some("k1")).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        assert_eq!(g.restored, Some(RestoreOutcome::Restored));
        sm.release(g);
        assert!(raw[0].restores.lock().await.is_empty());
        assert_eq!(
            *raw[1].restores.lock().await,
            vec![(0usize, "k1".to_string())]
        );

        // backend 1 kv-disabled: the exclusion still works, the restore is
        // dropped (no other kv-enabled backend), and it is not an error
        let (sm, raw) = make_kv(2, 1, vec![1]);
        let g = sm.acquire_excluding(0, Some("k2")).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 1, slot: 0 });
        assert_eq!(g.restored, None);
        sm.release(g);
        assert!(raw[1].restores.lock().await.is_empty());
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
    async fn save_after_retries_until_success() {
        let (sm, raw) = make(1, 2);
        // first two saves fail transiently, the third succeeds
        raw[0]
            .save_fail_first_n
            .store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.unwrap());
        // 2 failed attempts + 1 successful attempt
        let saves = raw[0].saves.lock().await.clone();
        assert_eq!(
            saves,
            vec![
                (0usize, "k1".to_string()),
                (0usize, "k1".to_string()),
                (0usize, "k1".to_string())
            ]
        );
        // the successful save marked slot 0 used
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 1 });
        sm.release(g);
    }

    #[tokio::test]
    async fn save_after_retries_exhausted_propagates_error() {
        let (sm, raw) = make(1, 2);
        raw[0]
            .save_mode
            .store(2, std::sync::atomic::Ordering::SeqCst); // always Err
        assert!(sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.is_err());
        // 1 initial attempt + the configured retries
        assert_eq!(raw[0].saves.lock().await.len(), 1 + DEFAULT_SAVE_RETRIES);
        // last_used not updated -> slot 0 still "free"
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 0 });
        sm.release(g);
    }

    #[tokio::test]
    async fn save_after_retries_exhausted_500_returns_false() {
        let (sm, raw) = make(1, 2);
        raw[0]
            .save_mode
            .store(1, std::sync::atomic::Ordering::SeqCst); // always Ok(false)
        assert!(!sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.unwrap());
        assert_eq!(raw[0].saves.lock().await.len(), 1 + DEFAULT_SAVE_RETRIES);
        // a failed (500) save still marks the slot used
        let g = sm.acquire(None).await.unwrap();
        assert_eq!(g.slot, GSlot { be: 0, slot: 1 });
        sm.release(g);
    }

    #[tokio::test]
    async fn save_after_no_retry_when_retries_disabled() {
        let backends = vec![BackendConf {
            url: "http://b0".to_string(),
            n_slots: 1,
            slot_save_path: None,
            kv_cache: true,
        }];
        let client = TestClient::new();
        let clients: Vec<Arc<dyn LlamaBackend>> =
            vec![Arc::clone(&client) as Arc<dyn LlamaBackend>];
        let sm = Arc::new(
            SlotManager::new(&backends, clients).with_save_retry(0, Duration::from_millis(1)),
        );
        client
            .save_mode
            .store(2, std::sync::atomic::Ordering::SeqCst);
        assert!(sm.save_after(GSlot { be: 0, slot: 0 }, "k1").await.is_err());
        // only the initial attempt, no retries
        assert_eq!(client.saves.lock().await.len(), 1);
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
            kv_cache: true,
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
                kv_cache: true,
            },
            BackendConf {
                url: "http://b1".to_string(),
                n_slots: 1,
                slot_save_path: None,
                kv_cache: true,
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
