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
//!
//! Full testkit (Task 14):
//!
//! * [`fake_web`] — the scripted "mini internet" of spec §12 (seven sites, cycles, request log,
//!   admin endpoints); binary `fake_web`, image `testkit-fake-web` (`testkit/Dockerfile.fake-web`).
//! * [`fake_gemini`] — Vertex AI online + batch wire format, scripted; binary `fake_gemini`.
//! * [`fake_gcs`] — the `fsouza/fake-gcs-server` container helper and a `BlobStore` over it.
//! * [`oracle`] — [`assert_polite`] (request log × [`PolicySnapshot`]) and [`assert_raw_first`].
//!
//! P1 agents may add `src/<crate>_fixtures.rs`.

pub mod blob;
pub mod clock;
pub mod fake_gcs;
pub mod fake_gemini;
pub mod fake_web;
pub mod frontier;
pub mod llm;
pub mod oracle;
pub mod pg;
pub mod queue;
pub mod request_log;
pub mod server;

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
pub use fake_gcs::{FakeGcsStore, fake_gcs};
pub use fake_gemini::FakeGemini;
pub use fake_web::{FakeWeb, FakeWebServer};
pub use frontier::{MemFrontier, UrlState};
pub use llm::{FakeLlm, MemLedger};
pub use oracle::{PolicySnapshot, assert_polite, assert_raw_first};
pub use pg::{MIGRATOR, pg, pg_with_url};
pub use queue::MemQueue;
pub use request_log::{RequestLog, RequestLogEntry};
pub use server::FakeServer;
