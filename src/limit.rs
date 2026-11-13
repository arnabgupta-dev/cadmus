//! Slows password guessing: a bounded number of login attempts per account and per client
//! address in a time window.
//!
//! The counters live in this process's memory. With several instances behind a load balancer
//! each one counts on its own, and a restart clears them.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One address may try this many times as many logins as one account, so that people behind
/// a shared address are not locked out by each other's typos.
const ADDRESS_FACTOR: u32 = 4;

/// Beyond this many tracked keys, expired ones are dropped; if that is not enough, all are.
/// It bounds memory when someone sprays made-up usernames.
const MAX_TRACKED: usize = 100_000;

#[derive(PartialEq, Eq, Hash)]
enum Key {
    Account(String),
    Address(IpAddr),
}

struct Window {
    attempts: u32,
    started: Instant,
}

/// Counts login attempts; see the module documentation.
pub(crate) struct LoginLimiter {
    /// Attempts allowed per account in one window; `0` turns the limiter off.
    max_attempts: u32,
    window: Duration,
    windows: Mutex<HashMap<Key, Window>>,
}

impl LoginLimiter {
    pub(crate) fn new(max_attempts: u32, window_secs: u64) -> Self {
        Self {
            max_attempts,
            window: Duration::from_secs(window_secs),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Registers a login attempt.
    ///
    /// The attempt is counted here, before the password is checked, so that many requests
    /// sent at once cannot all slip under the limit.
    ///
    /// # Errors
    /// The number of seconds to wait, if the account or the address has used up its attempts.
    pub(crate) fn attempt(&self, username: &str, address: Option<IpAddr>) -> Result<(), u64> {
        self.attempt_at(username, address, Instant::now())
    }

    fn attempt_at(&self, username: &str, address: Option<IpAddr>, now: Instant) -> Result<(), u64> {
        if self.max_attempts == 0 {
            return Ok(());
        }
        let mut windows = self.lock();
        if windows.len() >= MAX_TRACKED {
            windows.retain(|_, w| now.duration_since(w.started) < self.window);
            if windows.len() >= MAX_TRACKED {
                windows.clear();
            }
        }
        let keys = [
            Some((Key::Account(username.to_owned()), self.max_attempts)),
            address.map(|a| {
                (
                    Key::Address(a),
                    self.max_attempts.saturating_mul(ADDRESS_FACTOR),
                )
            }),
        ];
        // Refuse before counting, so that a locked-out caller does not extend the lock.
        for (key, limit) in keys.iter().flatten() {
            if let Some(window) = windows.get(key) {
                let age = now.duration_since(window.started);
                if age < self.window && window.attempts >= *limit {
                    return Err((self.window - age).as_secs().max(1));
                }
            }
        }
        for (key, _) in keys.into_iter().flatten() {
            let window = windows.entry(key).or_insert(Window {
                attempts: 0,
                started: now,
            });
            if now.duration_since(window.started) >= self.window {
                *window = Window {
                    attempts: 0,
                    started: now,
                };
            }
            window.attempts += 1;
        }
        Ok(())
    }

    /// Forgets the attempts of an account that has just logged in, and takes this attempt off
    /// its address's count.
    pub(crate) fn succeeded(&self, username: &str, address: Option<IpAddr>) {
        let mut windows = self.lock();
        windows.remove(&Key::Account(username.to_owned()));
        if let Some(window) = address.and_then(|a| windows.get_mut(&Key::Address(a))) {
            window.attempts = window.attempts.saturating_sub(1);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Window>> {
        // The map is always left consistent, so a panic elsewhere while it was held is no
        // reason to stop limiting.
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([10, 0, 0, last]))
    }

    #[test]
    fn an_account_is_locked_after_too_many_attempts_and_released_when_the_window_ends() {
        let limiter = LoginLimiter::new(3, 60);
        let start = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.attempt_at("ann", None, start), Ok(()));
        }
        assert_eq!(limiter.attempt_at("ann", None, start), Err(60));
        // Trying while locked does not push the release further away.
        let later = start + Duration::from_secs(45);
        assert_eq!(limiter.attempt_at("ann", None, later), Err(15));
        // Other accounts are unaffected.
        assert_eq!(limiter.attempt_at("bob", None, later), Ok(()));
        let after = start + Duration::from_secs(60);
        assert_eq!(limiter.attempt_at("ann", None, after), Ok(()));
    }

    #[test]
    fn logging_in_clears_the_count() {
        let limiter = LoginLimiter::new(3, 60);
        let now = Instant::now();
        for _ in 0..2 {
            limiter.attempt_at("ann", address(1), now).unwrap();
        }
        limiter.attempt_at("ann", address(1), now).unwrap();
        limiter.succeeded("ann", address(1));
        for _ in 0..3 {
            assert_eq!(limiter.attempt_at("ann", address(1), now), Ok(()));
        }
        assert!(limiter.attempt_at("ann", address(1), now).is_err());
    }

    #[test]
    fn an_address_is_limited_across_accounts() {
        let limiter = LoginLimiter::new(2, 60);
        let now = Instant::now();
        // Eight attempts (2 x 4) on eight different names from one address.
        for i in 0..8 {
            assert_eq!(
                limiter.attempt_at(&format!("user{i}"), address(1), now),
                Ok(())
            );
        }
        assert!(limiter.attempt_at("fresh", address(1), now).is_err());
        // The refused attempt did not count against the name, and other addresses are fine.
        assert_eq!(limiter.attempt_at("fresh", address(2), now), Ok(()));
    }

    #[test]
    fn zero_attempts_turns_it_off() {
        let limiter = LoginLimiter::new(0, 60);
        for _ in 0..1000 {
            assert_eq!(limiter.attempt("ann", address(1)), Ok(()));
        }
    }
}
