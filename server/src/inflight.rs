//! inflight.rs — how many distinct clients have a slow request (chat / catalog / payment
//! gateway) in flight at the same instant, and the highest that number ever reached.
//!
//! The dispatch loop itself is serial, so "clients being served in parallel" is exactly
//! the set of spawned tasks: one `Guard` per spawned request, keyed by the request's
//! reply SURB tag (= one anonymous client). The peak is the number to watch in
//! scrai-admin to see whether the single Nym client / the provider slots are anywhere
//! near a bottleneck. Aggregate only — tags are never persisted.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

struct State<K> {
    by_client: HashMap<K, u32>,
    peak: usize,
}

/// Cheap shared counter; clone it into spawned tasks.
pub struct Inflight<K: Hash + Eq + Copy> {
    inner: Arc<Mutex<State<K>>>,
}

impl<K: Hash + Eq + Copy> Clone for Inflight<K> {
    fn clone(&self) -> Self {
        Inflight { inner: Arc::clone(&self.inner) }
    }
}

impl<K: Hash + Eq + Copy> Default for Inflight<K> {
    fn default() -> Self {
        Inflight { inner: Arc::new(Mutex::new(State { by_client: HashMap::new(), peak: 0 })) }
    }
}

impl<K: Hash + Eq + Copy> Inflight<K> {
    /// Register one in-flight request for `client`; the returned guard unregisters it
    /// on drop (hold it until the reply has been sent).
    pub fn enter(&self, client: K) -> Guard<K> {
        if let Ok(mut s) = self.inner.lock() {
            *s.by_client.entry(client).or_insert(0) += 1;
            let now = s.by_client.len();
            if now > s.peak {
                s.peak = now;
            }
        }
        Guard { inner: Arc::clone(&self.inner), client }
    }

    /// Distinct clients with a request in flight right now.
    pub fn current(&self) -> usize {
        self.inner.lock().map(|s| s.by_client.len()).unwrap_or(0)
    }

    /// Highest `current()` seen since start.
    pub fn peak(&self) -> usize {
        self.inner.lock().map(|s| s.peak).unwrap_or(0)
    }
}

pub struct Guard<K: Hash + Eq + Copy> {
    inner: Arc<Mutex<State<K>>>,
    client: K,
}

impl<K: Hash + Eq + Copy> Drop for Guard<K> {
    fn drop(&mut self) {
        if let Ok(mut s) = self.inner.lock() {
            if let Some(n) = s.by_client.get_mut(&self.client) {
                *n -= 1;
                if *n == 0 {
                    s.by_client.remove(&self.client);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_distinct_clients_and_remembers_the_peak() {
        let f: Inflight<u8> = Inflight::default();
        let a1 = f.enter(1);
        let a2 = f.enter(1); // same client twice = still ONE client
        assert_eq!(f.current(), 1);
        let b = f.enter(2);
        let c = f.enter(3);
        assert_eq!(f.current(), 3);
        assert_eq!(f.peak(), 3);
        drop(b);
        drop(c);
        assert_eq!(f.current(), 1);
        drop(a1);
        assert_eq!(f.current(), 1, "client 1 still has one request in flight");
        drop(a2);
        assert_eq!(f.current(), 0);
        assert_eq!(f.peak(), 3, "peak survives the drain");
    }
}
