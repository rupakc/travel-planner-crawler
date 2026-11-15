//! `[kinds]`: kind registry files (owner: vc-kinds).

use serde::{Deserialize, Serialize};

/// Kind registry locations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KindsConfig {
    /// `kinds.toml` (policy per kind).
    pub kinds_toml: String,
    /// Directory of `<kind>.v<N>.json` schemas.
    pub schemas_dir: String,
    /// Directory of `<kind>.md` prompts.
    pub prompts_dir: String,
}

impl Default for KindsConfig {
    fn default() -> Self {
        KindsConfig {
            kinds_toml: "kinds.toml".into(),
            schemas_dir: "schemas".into(),
            prompts_dir: "prompts".into(),
        }
    }
}
