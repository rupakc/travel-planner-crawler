//! Role `maintenance` (Task 15). See the plan's Task 15 and spec §17.

use crate::Ctx;

/// Runs the `maintenance` role until shutdown.
pub async fn run(_ctx: Ctx) -> anyhow::Result<()> {
    anyhow::bail!("role maintenance is not wired yet (Task 15)")
}
