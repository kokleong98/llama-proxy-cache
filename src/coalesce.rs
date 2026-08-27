//! Concurrent-request coalescing ("singleflight").
//!
//! When enabled (`COALESCE_REQUESTS`), concurrent requests that compute the
//! same KV cache key — the hash of the backend model id plus the message
//! contents — are grouped, regardless of generation parameters: the first
//! request ("leader") runs the normal pipeline (slot acquire -> backend call
//! -> save/meta), and every other concurrent request ("follower") waits for
//! the leader and receives its result. Followers never touch slots,
//! backends, or meta files.
//!
//! Because grouping is based solely on the cache key, a follower may receive
//! a result generated under the leader's parameters: the leader's sampling
//! settings (`temperature`, `top_p`, `seed`, ...), `max_tokens`, and stream
//! mode apply to the whole group (a mixed stream/non-stream group is served
//! by converting the leader's result to the follower's response shape).

//! Concurrency: `std::sync::Mutex` guards short critical sections (no
//! async work inside); `tokio::sync::Notify` wakes waiters. A flight is
//! removed from the map as soon as the leader finishes, so a later request
//! with the same key starts a fresh group.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// The leader's terminal result, shared with all followers of the group.
#[derive(Debug, Clone, PartialEq)]
pub enum SharedOutcome {
    /// A JSON response (successful completion, or an error object) with its
    /// HTTP status.
    Json(u16, Value),
    /// A successful SSE stream; the full raw stream bytes.
    Sse(Vec<u8>),
    /// A non-JSON response body (e.g. a backend error page) with its status
    /// and content type.
    Raw(u16, Option<String>, Vec<u8>),
}

/// One in-flight group of concurrent same-key requests.
pub struct Flight {
    /// Raw SSE bytes produced so far by a streaming leader.
    sse: Mutex<Vec<u8>>,
    /// Set exactly once when the leader's pipeline reaches a terminal state.
    outcome: Mutex<Option<SharedOutcome>>,
    /// Set when a streaming leader starts producing chunks; lets stream
    /// followers commit to a 200 SSE response (or bail with the leader's
    /// error if it failed before that).
    stream_started: AtomicBool,
    notify: Notify,
}

impl Flight {
    pub fn new() -> Self {
        Self {
            sse: Mutex::new(Vec::new()),
            outcome: Mutex::new(None),
            stream_started: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    /// Append raw SSE bytes (streaming leader pump).
    pub fn append_sse(&self, chunk: &[u8]) {
        let mut buf = self.sse.lock().unwrap();
        buf.extend_from_slice(chunk);
        self.notify.notify_waiters();
    }

    /// Signal that a streaming leader has started producing chunks.
    pub fn mark_stream_started(&self) {
        self.stream_started.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn stream_started(&self) -> bool {
        self.stream_started.load(Ordering::SeqCst)
    }

    /// Register the leader's terminal result and wake all waiters.
    pub fn finish(&self, outcome: SharedOutcome) {
        *self.outcome.lock().unwrap() = Some(outcome);
        self.notify.notify_waiters();
    }

    /// The terminal result, if the leader has finished (cloned).
    pub fn outcome(&self) -> Option<SharedOutcome> {
        self.outcome.lock().unwrap().clone()
    }

    /// Copy the SSE bytes appended after `pos` and advance `pos`.
    pub fn drain_sse_from(&self, pos: &mut usize) -> Vec<u8> {
        let buf = self.sse.lock().unwrap();
        let out = buf[*pos..].to_vec();
        *pos = buf.len();
        out
    }

    /// Copy of all SSE bytes produced so far.
    pub fn sse_bytes(&self) -> Vec<u8> {
        self.sse.lock().unwrap().clone()
    }

    /// Register a wakeup interest **before** checking the condition (the
    /// check-then-wait pattern; see the `tokio::sync::Notify` docs).
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

impl Default for Flight {
    fn default() -> Self {
        Self::new()
    }
}

/// The set of in-flight groups, keyed by flight key.
pub struct SingleFlight {
    map: Mutex<HashMap<String, Arc<Flight>>>,
}

impl SingleFlight {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Join (or create) the group for `flight_key`.
    ///
    /// Returns the flight and whether this caller is its leader.
    pub fn enter(&self, flight_key: &str) -> (Arc<Flight>, bool) {
        let mut map = self.map.lock().unwrap();
        match map.get(flight_key) {
            Some(f) => (Arc::clone(f), false),
            None => {
                let f = Arc::new(Flight::new());
                map.insert(flight_key.to_string(), Arc::clone(&f));
                (f, true)
            }
        }
    }

    /// Remove a finished group so later same-key requests start fresh.
    pub fn forget(&self, flight_key: &str) {
        self.map.lock().unwrap().remove(flight_key);
    }
}

impl Default for SingleFlight {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enter_first_is_leader_second_is_follower() {
        let sf = SingleFlight::new();
        let (f1, leader1) = sf.enter("k");
        assert!(leader1);
        let (f2, leader2) = sf.enter("k");
        assert!(!leader2);
        assert!(Arc::ptr_eq(&f1, &f2));
        // A different key starts a new group.
        let (f3, leader3) = sf.enter("other");
        assert!(leader3);
        assert!(!Arc::ptr_eq(&f1, &f3));
    }

    #[test]
    fn forget_removes_the_group() {
        let sf = SingleFlight::new();
        let (_, leader) = sf.enter("k");
        assert!(leader);
        sf.forget("k");
        let (_, leader2) = sf.enter("k");
        assert!(leader2); // a fresh group, not the old flight
    }

    #[tokio::test]
    async fn finish_wakes_waiters_with_outcome() {
        let f = Arc::new(Flight::new());
        let f2 = Arc::clone(&f);
        let waiter = tokio::spawn(async move {
            loop {
                let n = f2.notified();
                tokio::pin!(n);
                if let Some(o) = f2.outcome() {
                    return o;
                }
                n.await;
            }
        });
        // Let the waiter register before finishing.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        f.finish(SharedOutcome::Json(200, json!({ "ok": true })));
        let o = waiter.await.unwrap();
        assert_eq!(o, SharedOutcome::Json(200, json!({ "ok": true })));
    }

    #[test]
    fn sse_append_and_drain_is_ordered() {
        let f = Flight::new();
        let mut pos = 0usize;
        assert!(f.drain_sse_from(&mut pos).is_empty());
        f.append_sse(b"a");
        f.append_sse(b"bc");
        assert_eq!(f.drain_sse_from(&mut pos), b"abc".to_vec());
        f.append_sse(b"d");
        assert_eq!(f.drain_sse_from(&mut pos), b"d".to_vec());
        assert_eq!(f.sse_bytes(), b"abcd".to_vec());
    }
}
