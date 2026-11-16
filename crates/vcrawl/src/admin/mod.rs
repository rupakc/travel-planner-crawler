//! `vcrawl admin …` (Task 15). One file per command group:
//!
//! | command | file |
//! |---|---|
//! | `status` | [`status`] |
//! | `source validate\|list\|approve\|disable` | [`source`] |
//! | `purge --url\|--url-prefix\|--host\|--author\|--source\|--record` | [`purge`] |
//! | `rebuild-captures [--dry-run\|--verify]` | [`rebuild`] |
//! | `reextract --kind` | [`reextract`] |
//!
//! Every command prints one JSON document on stdout (ids and counts, never page content).

pub mod db;
pub mod purge;
pub mod rebuild;
pub mod reextract;
pub mod source;
pub mod status;

use clap::Subcommand;

use crate::Ctx;

/// Admin commands.
#[derive(Debug, Subcommand)]
pub enum AdminCmd {
    /// Frontier, queue, budget and source health at a glance.
    Status,
    /// Source specs and scout proposals.
    #[command(subcommand)]
    Source(source::SourceCmd),
    /// Purge (tombstone, raw rewrite, attribution scrub) by URL, host, author, ….
    Purge(purge::PurgeArgs),
    /// Rebuild `segments` and `captures` rows from the `.cdxj` objects (disaster recovery).
    RebuildCaptures(rebuild::RebuildArgs),
    /// Queue re-extraction of every capture that produced records of a kind.
    Reextract(reextract::ReextractArgs),
}

/// Runs one admin command.
pub async fn run(ctx: Ctx, cmd: AdminCmd) -> anyhow::Result<()> {
    let out = match cmd {
        AdminCmd::Status => {
            let pool = db::connect(&ctx.cfg).await?;
            serde_json::to_value(status::status(&pool).await?)?
        }
        AdminCmd::Source(c) => source::run(&ctx, c).await?,
        AdminCmd::Purge(a) => purge::run(&ctx, a).await?,
        AdminCmd::RebuildCaptures(a) => rebuild::run(&ctx, a).await?,
        AdminCmd::Reextract(a) => reextract::run(&ctx, a).await?,
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
