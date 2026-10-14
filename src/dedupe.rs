//! The spec requires duplicate suppression by `event_id`: homeservers retry,
//! and a retry must not ping a phone twice. This remembers which devices
//! each event reached, long enough to cover a homeserver's retry backoff.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::token::DeviceToken;

pub struct Dedupe {
    ttl: Duration,
    /// The most events remembered at once.
    cap: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Keyed by `Arc<str>` so a lookup by `&str` allocates nothing and the
    /// key is shared with `order`.
    events: HashMap<Arc<str>, Delivered>,
    /// Events in first-delivery order. Every entry has the same TTL, so the
    /// front is always the next to expire.
    order: VecDeque<(Arc<str>, Instant)>,
}

struct Delivered {
    at: Instant,
    /// A notification names a handful of devices, so a scan beats hashing.
    pushkeys: Vec<Box<str>>,
}

impl Dedupe {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            inner: Mutex::default(),
        }
    }

    pub fn contains(&self, event_id: &str, token: DeviceToken<'_>, now: Instant) -> bool {
        self.lock()
            .events
            .get(event_id)
            .filter(|d| now.duration_since(d.at) < self.ttl)
            .is_some_and(|d| d.pushkeys.iter().any(|p| **p == *token.as_str()))
    }

    pub fn insert(&self, event_id: &str, token: DeviceToken<'_>, now: Instant) {
        let mut inner = self.lock();
        let Inner { events, order } = &mut *inner;

        match events.get_mut(event_id) {
            Some(d) if now.duration_since(d.at) < self.ttl => {
                if !d.pushkeys.iter().any(|p| **p == *token.as_str()) {
                    d.pushkeys.push(token.as_str().into());
                }
            }
            _ => {
                let key: Arc<str> = event_id.into();
                events.insert(
                    key.clone(),
                    Delivered {
                        at: now,
                        pushkeys: vec![token.as_str().into()],
                    },
                );
                order.push_back((key, now));
            }
        }

        // Prune here rather than on a timer: no background task to own.
        while let Some((key, at)) = order.front() {
            if now.duration_since(*at) < self.ttl && order.len() <= self.cap {
                break;
            }
            // An event that expired and was delivered again has a newer
            // entry further back; only that entry may remove it.
            if events.get(key).is_some_and(|d| d.at == *at) {
                events.remove(key);
            }
            order.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.lock().events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A poisoned lock still holds a consistent map: each update above
        // completes before anything can panic.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(900);

    fn t(s: &str) -> DeviceToken<'_> {
        DeviceToken::try_from(s).unwrap()
    }

    #[test]
    fn remembers_a_delivery_for_the_ttl() {
        let d = Dedupe::new(TTL, 10);
        let t0 = Instant::now();
        assert!(!d.contains("$e", t("aa"), t0));
        d.insert("$e", t("aa"), t0);
        assert!(d.contains("$e", t("aa"), t0 + TTL - Duration::from_secs(1)));
        assert!(!d.contains("$e", t("aa"), t0 + TTL));
    }

    #[test]
    fn is_per_event_and_per_device() {
        let d = Dedupe::new(TTL, 10);
        let t0 = Instant::now();
        d.insert("$e", t("aa"), t0);
        d.insert("$e", t("bb"), t0);
        assert!(d.contains("$e", t("bb"), t0));
        assert!(!d.contains("$e", t("cc"), t0));
        assert!(!d.contains("$f", t("aa"), t0));
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn expired_events_are_pruned_on_insert() {
        let d = Dedupe::new(TTL, 10);
        let t0 = Instant::now();
        d.insert("$e", t("aa"), t0);
        d.insert("$f", t("aa"), t0 + TTL);
        assert_eq!(d.len(), 1);
        assert!(d.contains("$f", t("aa"), t0 + TTL));
    }

    #[test]
    fn the_cap_evicts_the_oldest_event() {
        let d = Dedupe::new(TTL, 2);
        let t0 = Instant::now();
        for e in ["$1", "$2", "$3"] {
            d.insert(e, t("aa"), t0);
        }
        assert_eq!(d.len(), 2);
        assert!(!d.contains("$1", t("aa"), t0));
        assert!(d.contains("$3", t("aa"), t0));
    }

    #[test]
    fn an_event_delivered_again_after_expiry_survives_its_old_entry() {
        let d = Dedupe::new(TTL, 10);
        let t0 = Instant::now();
        d.insert("$e", t("aa"), t0);
        let later = t0 + TTL + Duration::from_secs(1);
        d.insert("$e", t("bb"), later);
        assert!(d.contains("$e", t("bb"), later));
        assert!(!d.contains("$e", t("aa"), later));
        assert_eq!(d.len(), 1);
    }
}
