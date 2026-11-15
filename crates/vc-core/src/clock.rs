//! Local wall-clock abstraction. Lease deadlines use database time ([`crate::DbTime`]); this
//! clock is for everything local (spool ages, cache TTLs, test time travel via
//! `testkit::FakeClock`).

use chrono::{DateTime, Utc};

/// A source of the current time.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// The current UTC time.
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}
