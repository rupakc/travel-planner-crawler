//! `vcrawl` entry point: parses the CLI and dispatches to a role. See `lib.rs`.

use clap::{Parser, Subcommand};
use vcrawl::{Ctx, admin, health, migrate, roles, run};

/// The Voyager crawler.
#[derive(Debug, Parser)]
#[command(name = "vcrawl", version)]
struct Cli {
    #[command(subcommand)]
    role: Role,
}

#[derive(Debug, Subcommand)]
enum Role {
    /// Several roles as tasks of one process: one runtime, one DB pool, one /metrics +
    /// /healthz listener (the small-VM deployment).
    Run {
        /// Comma-separated roles (scheduler, fetcher, processor, embedder, api, maintenance).
        #[arg(long, value_parser = run::parse_roles,
              default_value = "scheduler,fetcher,processor,embedder,api,maintenance")]
        roles: std::vec::Vec<run::RoleName>,
    },
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
        /// Print `{"schema":N,"min_schema":N,"migrations":"1,2,…"}` of this image and exit (no
        /// database; the build reads it for the `io.voyager-crawler.{schema,min-schema,migrations}`
        /// labels).
        #[arg(long, conflicts_with = "contract")]
        print_schema: bool,
    },
    /// Container health check.
    Health,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    vcrawl::install_crypto_provider();
    let cli = Cli::parse();
    if let Role::Migrate {
        print_schema: true, ..
    } = cli.role
    {
        println!("{}", migrate::schema_labels());
        return Ok(());
    }
    let ctx = Ctx::new(vcrawl::wiring::config::load()?);
    match cli.role {
        Role::Run { roles } => run::run(ctx, roles).await,
        Role::Scheduler => roles::scheduler::run(ctx).await,
        Role::Fetcher => roles::fetcher::run(ctx).await,
        Role::Processor => roles::processor::run(ctx).await,
        Role::Embedder => roles::embedder::run(ctx).await,
        Role::Api => roles::api::run(ctx).await,
        Role::Maintenance => roles::maintenance::run(ctx).await,
        Role::Admin(cmd) => admin::run(ctx, cmd).await,
        Role::Migrate { contract, .. } => migrate::run(ctx, contract).await,
        Role::Health => health::run(ctx).await,
    }
}
