//! # testkit (minimal, P0)
//!
//! Shared test fixtures for every crate (r5 Q14):
//!
//! * [`pg()`] — a fresh, fully migrated Postgres 17 + pgvector database per call, cloned from a
//!   per-process template; one `pgvector/pgvector:pg17` container per test binary (or the server
//!   in `VC_TEST_DATABASE_URL`, e.g. a CI service).
//! * [`FakeClock`] — settable time.
//! * In-memory fakes: [`MemFrontier`], [`MemBlobStore`], [`MemQueue`], [`FakeLlm`] (also an
//!   `Embedder`), [`MemLedger`].
//! * [`request_log`] — the **frozen** request-log schema the politeness oracle reads.
//!
//! P1 agents may add `src/<crate>_fixtures.rs`; the P1 testkit task adds fake-web, fake-gemini,
//! fake-gcs and the oracles.

pub mod blob;
pub mod clock;
pub mod frontier;
pub mod llm;
pub mod pg;
pub mod queue;
pub mod request_log;

// One fixtures module per P1 crate (each owned by that crate's task).
pub mod vc_api_fixtures;
pub mod vc_extract_fixtures;
pub mod vc_fetch_fixtures;
pub mod vc_frontier_fixtures;
pub mod vc_index_fixtures;
pub mod vc_kinds_fixtures;
pub mod vc_llm_fixtures;
pub mod vc_obs_fixtures;
pub mod vc_parse_fixtures;
pub mod vc_policy_fixtures;
pub mod vc_queue_fixtures;
pub mod vc_render_fixtures;
pub mod vc_secrets_fixtures;
pub mod vc_sources_fixtures;
pub mod vc_store_fixtures;

pub use blob::MemBlobStore;
pub use clock::FakeClock;
pub use frontier::{MemFrontier, UrlState};
pub use llm::{FakeLlm, MemLedger};
pub use pg::{MIGRATOR, pg, pg_with_url};
pub use queue::MemQueue;
pub use request_log::{RequestLog, RequestLogEntry};
