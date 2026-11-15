//! The data-driven kind registry (r5 Q15, spec §5.7, §17.8). Implemented by `vc-kinds`
//! (`FileKindRegistry` over `schemas/<kind>.v<N>.json`, `prompts/<kind>.md`, `kinds.toml`).

use serde::{Deserialize, Serialize};

use crate::ids::KindId;
use crate::licence::SourceClass;

/// Where records of a kind live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindStorage {
    /// Versioned records (`records` / `record_versions`).
    Record,
    /// Time series (`observations`), e.g. `fx_rate`, `weather_normal`.
    Observation,
}

/// One kind's definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindDef {
    /// The kind id.
    pub id: KindId,
    /// Schema version (the `v<N>` of the schema file); records carry `kind@version`.
    pub version: u32,
    /// The JSON Schema (draft 2020-12), also used as Gemini's `responseSchema`.
    pub schema: serde_json::Value,
    /// The extraction prompt (system prompt body).
    pub prompt: String,
    /// blake3 hex of the prompt (recorded in `record_versions.prompt_hash`).
    pub prompt_hash: String,
    /// JSON pointers (e.g. `/phone`, `/numbers/0/value`) whose values must appear verbatim
    /// (after normalisation) in the source text, or be dropped.
    pub verbatim_fields: Vec<String>,
    /// Source classes allowed to produce this kind (empty = all).
    pub allowed_source_classes: Vec<SourceClass>,
    /// Record or observation.
    pub storage: KindStorage,
    /// High-stakes kinds (`visa_rule`, `advisory`, `emergency_numbers`): conflicts are flagged.
    pub high_stakes: bool,
    /// When `reference` sources are allowed for a high-stakes kind, how many independent
    /// reference sources must agree (2 for `emergency_numbers`; 0 = not applicable).
    pub min_reference_agreement: u8,
}

impl KindDef {
    /// `kind@version`, as stored in `record_versions.kind_schema`.
    pub fn schema_tag(&self) -> String {
        format!("{}@{}", self.id, self.version)
    }

    /// Whether a source of class `c` may produce this kind.
    pub fn allows_source_class(&self, c: SourceClass) -> bool {
        self.allowed_source_classes.is_empty() || self.allowed_source_classes.contains(&c)
    }
}

/// Looks up kinds.
pub trait KindRegistry: Send + Sync {
    /// A kind by id.
    fn get(&self, id: &KindId) -> Option<&KindDef>;

    /// All kinds, sorted by id.
    fn all(&self) -> Vec<&KindDef>;
}
