//! # vc-kinds (Task 7, stub)
//!
//! `FileKindRegistry`: loads `schemas/<kind>.v<N>.json`, `prompts/<kind>.md` and `kinds.toml`.

use vc_config::KindsConfig;
use vc_core::KindId;
use vc_core::kinds::{KindDef, KindRegistry};

/// The file-backed kind registry.
#[derive(Debug, Clone, Default)]
pub struct FileKindRegistry {
    kinds: Vec<KindDef>,
}

impl FileKindRegistry {
    /// Loads every kind from the configured files.
    pub fn load(_config: &KindsConfig) -> vc_core::Result<Self> {
        todo!("Task 7")
    }
}

impl KindRegistry for FileKindRegistry {
    fn get(&self, id: &KindId) -> Option<&KindDef> {
        self.kinds.iter().find(|k| &k.id == id)
    }
    fn all(&self) -> Vec<&KindDef> {
        self.kinds.iter().collect()
    }
}
