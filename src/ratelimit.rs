//! A sliding-window guard per key. A relay without one turns one eager
//! user into twenty concurrent `gray` processes; with one, message 21
//! inside the window gets a short "slow down" instead of a fork.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub struct RateLimiter {
    capacity: usize,
    window: std::time::Duration,
    events: HashMap<String, VecDeque<Instant>>,
    blocked_until: HashMap<String, Instant>,
}

impl RateLimiter {
    pub fn new(capacity: usize, window_secs: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            window: std::time::Duration::from_secs(window_secs.max(1)),
            events: HashMap::new(),
            blocked_until: HashMap::new(),
        }
    }

    /// True when the key may act now. A false consumes nothing — the caller
    /// is expected to say so and stop.
    pub fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        if let Some(until) = self.blocked_until.get(key) {
            if *until > now {
                return false;
            }
        }
        let q = self.events.entry(key.to_string()).or_default();
        while let Some(front) = q.front() {
            if now.duration_since(*front) >= self.window {
                q.pop_front();
            } else {
                break;
            }
        }
        if q.len() >= self.capacity {
            return false;
        }
        q.push_back(now);
        true
    }

    /// After this many seconds the key may act again.
    pub fn retry_after_secs(&mut self, key: &str) -> u64 {
        let now = Instant::now();
        let q = self.events.entry(key.to_string()).or_default();
        q.front()
            .map(|oldest| {
                self.window
                    .saturating_sub(now.duration_since(*oldest))
                    .as_secs()
                    + 1
            })
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_then_not() {
        let mut r = RateLimiter::new(2, 60);
        assert!(r.allow("a"));
        assert!(r.allow("a"));
        assert!(!r.allow("a"));
        assert!(r.allow("b"));
    }
}
