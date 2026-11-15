//! A settable clock.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use vc_core::Clock;

/// A clock that only moves when told to. Clones share the same time.
#[derive(Debug, Clone)]
pub struct FakeClock(Arc<Mutex<DateTime<Utc>>>);

impl FakeClock {
    /// A clock at `t`.
    pub fn new(t: DateTime<Utc>) -> Self {
        FakeClock(Arc::new(Mutex::new(t)))
    }

    /// Moves time forward.
    pub fn advance(&self, d: Duration) {
        let mut t = self.0.lock().expect("clock lock");
        *t += chrono::Duration::from_std(d).expect("duration in range");
    }

    /// Sets the time.
    pub fn set(&self, t: DateTime<Utc>) {
        *self.0.lock().expect("clock lock") = t;
    }
}

impl Default for FakeClock {
    /// 2026-01-01T00:00:00Z.
    fn default() -> Self {
        FakeClock::new(
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .single()
                .expect("valid date"),
        )
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock lock")
    }
}
