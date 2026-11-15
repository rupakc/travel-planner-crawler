//! # vc-sources (Task 10, stub)
//!
//! The `SourceSpec` loader/validator, seeding into Postgres, and the built-in API/dump adapters
//! (`adapters::REGISTRY`). Block 17xx.

use std::path::Path;

use vc_core::Result;
use vc_core::sources::SourceSpec;

pub mod adapters {
    //! Built-in adapters.

    use vc_core::sources::SourceAdapter;

    /// Every built-in adapter, by id.
    pub fn registry() -> Vec<Box<dyn SourceAdapter>> {
        Vec::new()
    }
}

/// Loads and validates every `<dir>/*.yaml`.
pub fn load_dir(_dir: &Path) -> Result<Vec<SourceSpec>> {
    todo!("Task 10")
}
