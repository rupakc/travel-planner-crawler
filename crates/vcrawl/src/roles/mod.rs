//! Long-running roles. One file per role; each exposes `pub async fn run(ctx: Ctx)`.

pub mod api;
pub mod embedder;
pub mod fetcher;
pub mod maintenance;
pub mod processor;
pub mod scheduler;
