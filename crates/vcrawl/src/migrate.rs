//! `vcrawl migrate [--contract]` (Task 15).

use crate::Ctx;

/// Applies expand migrations, or contract migrations with `contract`.
pub async fn run(_ctx: Ctx, contract: bool) -> anyhow::Result<()> {
    anyhow::bail!("migrate (contract={contract}) is not wired yet (Task 15)")
}
