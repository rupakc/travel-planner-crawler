//! `vcrawl admin …` (Task 15). One file per command group.

use clap::Subcommand;

use crate::Ctx;

/// Admin commands.
#[derive(Debug, Subcommand)]
pub enum AdminCmd {
    /// Frontier, queue, budget and source health at a glance.
    Status,
}

/// Runs one admin command.
pub async fn run(_ctx: Ctx, cmd: AdminCmd) -> anyhow::Result<()> {
    anyhow::bail!("admin {cmd:?} is not wired yet (Task 15)")
}
