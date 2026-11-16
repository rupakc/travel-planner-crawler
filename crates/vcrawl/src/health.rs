//! `vcrawl health`: the container health check (deploy contract, ledger TD).

use crate::Ctx;

/// Exits 0 when this process's dependencies are reachable.
pub async fn run(_ctx: Ctx) -> anyhow::Result<()> {
    anyhow::bail!("health is not wired yet (Task 15)")
}
