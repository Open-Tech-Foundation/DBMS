//! Injectable wall-clock time.

use std::sync::atomic::{AtomicI64, Ordering};

/// A source of wall-clock time, injectable so the lower layers can be driven
/// deterministically under simulation.
///
/// Time is expressed as epoch **microseconds, UTC** — the engine's `timestamp`
/// unit (`SPEC.md` §3).
pub trait Clock: Send + Sync {
    /// The current time as epoch microseconds, UTC.
    fn now_micros(&self) -> i64;
}

/// The real system clock.
///
/// # Examples
///
/// ```
/// use common::{Clock, SystemClock};
///
/// let clock = SystemClock;
/// assert!(clock.now_micros() > 0);
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_micros(&self) -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_micros()).unwrap_or(i64::MAX),
            Err(e) => i64::try_from(e.duration().as_micros())
                .map(|v| -v)
                .unwrap_or(i64::MIN),
        }
    }
}

/// A manually advanced clock for tests and deterministic simulation.
///
/// # Examples
///
/// ```
/// use common::{Clock, ManualClock};
///
/// let clock = ManualClock::new(1_000);
/// assert_eq!(clock.now_micros(), 1_000);
/// clock.advance(500);
/// assert_eq!(clock.now_micros(), 1_500);
/// ```
#[derive(Debug)]
pub struct ManualClock {
    micros: AtomicI64,
}

impl ManualClock {
    /// Create a clock fixed at `start_micros`.
    pub fn new(start_micros: i64) -> Self {
        Self {
            micros: AtomicI64::new(start_micros),
        }
    }

    /// Advance the clock by `delta_micros` (saturating).
    pub fn advance(&self, delta_micros: i64) {
        let mut cur = self.micros.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_add(delta_micros);
            match self
                .micros
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Set the clock to an absolute value.
    pub fn set(&self, micros: i64) {
        self.micros.store(micros, Ordering::Release);
    }
}

impl Clock for ManualClock {
    fn now_micros(&self) -> i64 {
        self.micros.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_advances_and_sets() {
        let clock = ManualClock::new(10);
        assert_eq!(clock.now_micros(), 10);
        clock.advance(5);
        assert_eq!(clock.now_micros(), 15);
        clock.set(100);
        assert_eq!(clock.now_micros(), 100);
    }

    #[test]
    fn system_clock_is_positive_and_monotonicish() {
        let clock = SystemClock;
        let a = clock.now_micros();
        let b = clock.now_micros();
        assert!(a > 0);
        assert!(b >= a);
    }
}
