//! `vcrawl` entry point: parses the CLI and dispatches to a role. See `lib.rs`.

use clap::{Parser, Subcommand};
use vcrawl::{Ctx, admin, health, migrate, roles};

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
    /// Operator commands.
    #[command(subcommand)]
    Admin(admin::AdminCmd),
    /// Applies migrations.
    Migrate {
        /// Apply contract (destructive) migrations.
        #[arg(long)]
        contract: bool,
    },
    /// Container health check.
    Health,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let ctx = Ctx::new(vcrawl::wiring::config::load()?);
    match cli.role {
        Role::Scheduler => roles::scheduler::run(ctx).await,
        Role::Fetcher => roles::fetcher::run(ctx).await,
        Role::Processor => roles::processor::run(ctx).await,
        Role::Embedder => roles::embedder::run(ctx).await,
        Role::Api => roles::api::run(ctx).await,
        Role::Maintenance => roles::maintenance::run(ctx).await,
        Role::Admin(cmd) => admin::run(ctx, cmd).await,
        Role::Migrate { contract } => migrate::run(ctx, contract).await,
        Role::Health => health::run(ctx).await,
    }
}
