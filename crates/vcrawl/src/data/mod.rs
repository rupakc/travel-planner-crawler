//! The data path (Task 15, slice "data path"): captures → records (`processor`), records →
//! embeddings (`embedder`) and the `/v1` read API (`api`). The role entry points in
//! `roles/{processor,embedder,api}.rs` only wire configuration into these pieces, so every
//! piece is testable against a migrated database and in-memory fakes.
//!
//! * [`processor`]: one `capture_ready` item → stored body → `vc-parse` → `vc-extract`
//!   (structured first; the LLM only when `VC_LLM_ENABLED` and under the extraction budget) →
//!   `RecordSink::upsert`; in-scope links → frontier; high-stakes `reference` records that
//!   await a second agreeing source are held in [`peers::PendingPeers`] (table `held_records`) and fed back as peers.
//! * [`stale`]: records a newer capture of their page no longer contains become `stale`
//!   (with provenance and a `changes` row) and are re-activated when they reappear.
//! * [`text_tier`]: per committed segment, `text/YYYY/MM/<segment>.jsonl.zst` with each
//!   capture's main text and structured data (create-if-absent, claimed per segment).
//! * [`embed`]: the embedder loop (`pending_embeddings` → reserve → embed → store) and the
//!   budgeted [`embed::BudgetQueryEmbedder`] used by search.
//! * [`api`]: the [`vc_api::ApiState`] over Postgres (`PgIndex`, `PgApiStore`).
//!
//! Logs carry ids and counts only, never page content.

pub mod api;
pub mod capture;
pub mod db;
pub mod embed;
pub mod peers;
pub mod processor;
pub mod shutdown;
pub mod sources;
pub mod stale;
pub mod text_tier;
