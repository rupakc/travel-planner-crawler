//! Process context shared by every role: configuration now; pools, stores, clients and
//! metrics are added here as roles are wired.

use std::sync::Arc;

/// Shared process context (cheap to clone).
#[derive(Clone)]
pub struct Ctx {
    /// Loaded configuration.
    pub cfg: Arc<vc_config::Config>,
}

impl Ctx {
    /// Builds the context from configuration.
    pub fn new(cfg: vc_config::Config) -> Self {
        Self { cfg: Arc::new(cfg) }
    }
}
