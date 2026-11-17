//! # vcrawl
//!
//! The single binary: `run` (several roles in one process, [`run`]), roles `scheduler`, `fetcher`, `processor`, `embedder`, `api`,
//! `maintenance`, plus `admin`, `migrate` and `health`. Only this crate depends on
//! implementations. Layout (so parallel work does not collide):
//! `roles/<role>.rs`, `admin/<group>.rs`, `agentic/*` (Task 17), `migrate.rs`, `health.rs`,
//! shared wiring in `ctx.rs`.

pub mod admin;
pub mod agentic;
pub mod ctx;
pub mod data;
pub mod health;
pub mod migrate;
pub mod obs_jobs;
pub mod roles;
pub mod run;
pub mod wiring;

pub use ctx::Ctx;
