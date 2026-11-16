//! # vc-sources (Task 10)
//!
//! - [`spec`]: loading and validating `sources/<id>.yaml` (the library half of
//!   `vcrawl admin source validate`).
//! - [`seed`]: seeding specs into the `sources` table and their seed URLs into the frontier.
//! - [`adapters`]: the built-in API/dump adapters ([`adapters::REGISTRY`]), each streaming
//!   through the permit client and writing raw records to the capture sink **before**
//!   deriving anything (raw first).
//!
//! No migration of its own yet: the `sources` table comes from `migrations/0002_sources.sql`;
//! block 17xx stays reserved.

pub mod adapters;
pub mod seed;
pub mod sha256;
pub mod spec;

pub use seed::{SeedReport, seed_frontier, seed_sources, seed_urls, spec_hash};
pub use spec::{
    ValidateOpts, Validation, deny_match, load_denylist, load_dir, load_file, parse_spec, read_dir,
    validate,
};
