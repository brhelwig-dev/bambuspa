//! Wall-clock time as unix milliseconds, swappable in tests.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Returns the current time in unix milliseconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    })
}

/// Seconds from a millisecond clock reading.
pub fn secs(ms: i64) -> i64 {
    ms.div_euclid(1000)
}

#[cfg(test)]
pub use fake::Fake;

#[cfg(test)]
mod fake {
    use super::Clock;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// A clock that only moves when told to.
    #[derive(Clone)]
    pub struct Fake(Arc<AtomicI64>);

    impl Fake {
        pub fn at(ms: i64) -> Fake {
            Fake(Arc::new(AtomicI64::new(ms)))
        }
        pub fn clock(&self) -> Clock {
            let t = self.0.clone();
            Arc::new(move || t.load(Ordering::SeqCst))
        }
        pub fn advance_ms(&self, ms: i64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
        pub fn advance(&self, secs: i64) {
            self.advance_ms(secs * 1000);
        }
    }
}
