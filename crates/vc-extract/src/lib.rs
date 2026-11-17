//! # vc-extract
//!
//! The extraction pipeline (spec §5.7, §17.8): structured data first (JSON-LD/microdata →
//! kinds via mapping tables, no LLM), then LLM extraction under the budget ledger (trimmed
//! input, untrusted-data system prompt, schema output), JSON Schema validation with one
//! repair attempt, grounding checks (verbatim fields, URL domain rule), the source-class gate
//! (high-stakes kinds from official sources only), place resolution and stable record keys.
//!
//! `FilePlaceResolver` reads the exported GeoNames subset in `data/` (see `data/NOTICE`).
//! Contains data from GeoNames (geonames.org), CC BY 4.0.

pub mod fold;
mod fold_table;
pub mod gate;
pub mod grounding;
pub mod pipeline;
pub mod places;
pub mod record_key;
pub mod structured;

pub use gate::{Gate, PeerRecord, source_gate};
pub use pipeline::{AdapterRecord, ExtractInput, ExtractOutput, ExtractReport, Extractor, Skip};
pub use places::{FilePlaceResolver, GEONAMES_ATTRIBUTION, LlmPlaceDisambiguator};
