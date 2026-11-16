//! Oracles for integration and E2E tests: the politeness oracle ([`assert_polite`]) over the
//! fake web's request log, and the raw-first oracle ([`assert_raw_first`]) over Postgres and the
//! blob store.

mod polite;
mod raw_first;

pub(crate) use polite::origin_of;
pub use polite::{
    Backoff, KeyRules, PolicySnapshot, RobotsServed, Violation, ViolationKind, assert_polite,
    check_polite, denylisted, entries_for,
};
pub use raw_first::{RawFirstViolation, assert_raw_first, check_raw_first};
