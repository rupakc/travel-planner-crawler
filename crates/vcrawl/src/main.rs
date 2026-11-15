//! # vcrawl (Task 15, stub)
//!
//! The single binary: roles `scheduler`, `fetcher`, `processor`, `embedder`, `api`,
//! `maintenance`, `admin`, `migrate`. Only this crate depends on implementations.

use clap::{Parser, Subcommand};

/// The Voyager crawler.
#[derive(Debug, Parser)]
#[command(name = "vcrawl", version)]
struct Cli {
    #[command(subcommand)]
    role: Role,
}

#[derive(Debug, Subcommand)]
enum Role {
    /// Sources → seeds/sitemaps/feeds → enqueue; recrawl; robots refresh.
    Scheduler,
    /// The claim loop.
    Fetcher,
    /// Captures → records.
    Processor,
    /// Embeddings.
    Embedder,
    /// The `/v1` API.
    Api,
    /// Partitions, purge, drills, digest.
    Maintenance,
    /// Applies migrations.
    Migrate,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _cfg = vc_config::Config::load(None)?;
    anyhow::bail!("role {:?} is not wired yet (Task 15)", cli.role)
}
