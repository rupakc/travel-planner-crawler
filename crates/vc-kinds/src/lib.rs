//! # vc-kinds (Task 7)
//!
//! [`FileKindRegistry`]: the data-driven kind registry (r5 Q15, spec §5.7, §17.8). It loads
//! `kinds.toml` (policy per kind), `schemas/<kind>.v<N>.json` (JSON Schema draft 2020-12, also
//! Vertex's `responseSchema`) and `prompts/<kind>.md` (the extraction system prompt) and turns
//! them into [`KindDef`]s.
//!
//! The loader refuses data that would weaken the grounding contract, whatever the files say:
//! - `visa_rule` and `advisory` are high-stakes and `official`-only; `emergency_numbers` is
//!   high-stakes, `official`, or `reference` with `min_reference_agreement >= 2`; a
//!   high-stakes kind never has an empty (= all) source-class list;
//! - every schema is draft 2020-12 with `additionalProperties: false` on each object,
//!   `maxLength` (or `enum`) on each string and `maxItems` on each array, and uses only the
//!   keywords Vertex `responseSchema` understands (no `$ref`);
//! - every `verbatim` pointer resolves to a scalar in the schema; `*` segments are expanded to
//!   concrete indices `0..maxItems`, so [`KindDef::verbatim_fields`] holds only plain JSON
//!   pointers (absent pointers are simply skipped by the grounding check);
//! - observation kinds carry a `series_key` template over their own fields (plus
//!   `{place_id}`, the first resolved GeoNames id); record kinds have none;
//! - every `<kind>.v<N>.json` in the schemas directory belongs to a `kinds.toml` entry.
//!
//! Provenance hooks: every v1 schema carries `source_url`, `as_of` and `evidence` (up to three
//! verbatim quotes), all of them verbatim-checked.

mod policy;
mod schema_lint;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use vc_config::KindsConfig;
use vc_core::KindId;
use vc_core::kinds::{KindDef, KindRegistry};

pub use policy::{HIGH_STAKES_KINDS, MIN_REFERENCE_AGREEMENT, OFFICIAL_ONLY_KINDS};
pub use schema_lint::DRAFT_2020_12;

/// The 20 entity kinds of v1 (spec §5.7, plus `cheapest_fare` from 2026-11-17), sorted.
pub const V1_KINDS: [&str; 20] = [
    "activity",
    "advisory",
    "airport",
    "article",
    "attraction",
    "cheapest_fare",
    "emergency_numbers",
    "event",
    "fx_rate",
    "hotel",
    "packing_hint",
    "place",
    "qa_thread",
    "route",
    "sim_offer",
    "tip",
    "transport_option",
    "visa_rule",
    "walking_tour",
    "weather_normal",
];

type SeriesKey = (String, Vec<policy::Piece>);

#[derive(Debug, Clone)]
struct Loaded {
    def: KindDef,
    validator: Arc<jsonschema::Validator>,
    series_key: Option<SeriesKey>,
}

/// The file-backed kind registry.
#[derive(Debug, Clone, Default)]
pub struct FileKindRegistry {
    kinds: BTreeMap<String, Loaded>,
}

fn invalid(msg: impl Into<String>) -> vc_core::Error {
    vc_core::Error::Invalid(format!("kinds: {}", msg.into()))
}

fn read(path: &Path) -> vc_core::Result<String> {
    std::fs::read_to_string(path).map_err(|e| invalid(format!("{}: {e}", path.display())))
}

impl FileKindRegistry {
    /// Loads every kind from the configured files and checks the rules in the crate docs.
    pub fn load(config: &KindsConfig) -> vc_core::Result<Self> {
        let toml_path = Path::new(&config.kinds_toml);
        let file: policy::KindsFile = toml::from_str(&read(toml_path)?)
            .map_err(|e| invalid(format!("{}: {e}", toml_path.display())))?;
        let schemas_dir = Path::new(&config.schemas_dir);
        let prompts_dir = Path::new(&config.prompts_dir);

        let mut kinds = BTreeMap::new();
        for (id, p) in &file.kinds {
            kinds.insert(id.clone(), load_kind(id, p, schemas_dir, prompts_dir)?);
        }
        check_orphans(schemas_dir, &kinds)?;
        Ok(FileKindRegistry { kinds })
    }

    /// Validates `data` against the kind's schema; returns every error on failure. Messages
    /// name the instance path and the failed keyword only, never the offending value (logs
    /// never carry page content).
    pub fn validate(&self, id: &KindId, data: &Value) -> Result<(), Vec<String>> {
        let k = self
            .kinds
            .get(&*id.0)
            .ok_or_else(|| vec![format!("unknown kind {id}")])?;
        let errs: Vec<String> = k
            .validator
            .iter_errors(data)
            .map(|e| format!("{} ({})", e.instance_path(), e.schema_path()))
            .collect();
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// The observation series key template of a kind (`None` for record kinds).
    pub fn series_key_template(&self, id: &KindId) -> Option<&str> {
        self.kinds
            .get(&*id.0)?
            .series_key
            .as_ref()
            .map(|(t, _)| t.as_str())
    }

    /// Renders the series key of an observation from its data and the first resolved place
    /// id. `None` for record kinds, or when a placeholder has no value (never a guessed key).
    pub fn series_key(&self, id: &KindId, data: &Value, place_id: Option<i64>) -> Option<String> {
        let (_, pieces) = self.kinds.get(&*id.0)?.series_key.as_ref()?;
        policy::render(pieces, data, place_id)
    }
}

fn load_kind(
    id: &str,
    p: &policy::KindPolicy,
    schemas_dir: &Path,
    prompts_dir: &Path,
) -> vc_core::Result<Loaded> {
    policy::check(id, p).map_err(invalid)?;

    let schema_file = format!("{id}.v{}.json", p.version);
    let schema: Value = serde_json::from_str(&read(&schemas_dir.join(&schema_file))?)
        .map_err(|e| invalid(format!("{schema_file}: {e}")))?;
    jsonschema::meta::validate(&schema)
        .map_err(|e| invalid(format!("{schema_file}: not a valid JSON Schema: {e}")))?;
    schema_lint::lint(&schema).map_err(|e| invalid(format!("{schema_file}: {e}")))?;
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .map_err(|e| invalid(format!("{schema_file}: {e}")))?;

    let mut verbatim_fields = Vec::new();
    for pat in &p.verbatim {
        for ptr in
            schema_lint::expand_pointer(&schema, pat).map_err(|e| invalid(format!("{id}: {e}")))?
        {
            if !verbatim_fields.contains(&ptr) {
                verbatim_fields.push(ptr);
            }
        }
    }

    let series_key = match &p.series_key {
        None => None,
        Some(t) => {
            let pieces = policy::parse_template(t).map_err(|e| invalid(format!("{id}: {e}")))?;
            for piece in &pieces {
                if let policy::Piece::Field { name, .. } = piece
                    && name != policy::PLACE_ID
                    && !schema_lint::top_level_property(&schema, name)
                {
                    return Err(invalid(format!(
                        "{id}: series_key placeholder {{{name}}} is not a property"
                    )));
                }
            }
            Some((t.clone(), pieces))
        }
    };

    let prompt_file = format!("{id}.md");
    let prompt = read(&prompts_dir.join(&prompt_file))?;
    if prompt.trim().is_empty() {
        return Err(invalid(format!("{prompt_file} is empty")));
    }
    let prompt_hash = blake3::hash(prompt.as_bytes()).to_hex().to_string();

    Ok(Loaded {
        def: KindDef {
            id: KindId::new(id),
            version: p.version,
            schema,
            prompt,
            prompt_hash,
            verbatim_fields,
            allowed_source_classes: p.source_classes.clone(),
            storage: p.storage,
            high_stakes: p.high_stakes,
            min_reference_agreement: p.min_reference_agreement,
        },
        validator: Arc::new(validator),
        series_key,
    })
}

/// Every `<kind>.v<N>.json` in the schemas directory must belong to a registered kind.
fn check_orphans(dir: &Path, kinds: &BTreeMap<String, Loaded>) -> vc_core::Result<()> {
    let rd = std::fs::read_dir(dir).map_err(|e| invalid(format!("{}: {e}", dir.display())))?;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some((kind, ver)) = name
            .strip_suffix(".json")
            .and_then(|stem| stem.rsplit_once(".v"))
        else {
            continue;
        };
        if ver.is_empty() || !ver.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if !kinds.contains_key(kind) {
            return Err(invalid(format!(
                "schema {name} has no [kinds.{kind}] entry in kinds.toml"
            )));
        }
    }
    Ok(())
}

impl KindRegistry for FileKindRegistry {
    fn get(&self, id: &KindId) -> Option<&KindDef> {
        self.kinds.get(&*id.0).map(|k| &k.def)
    }
    fn all(&self) -> Vec<&KindDef> {
        self.kinds.values().map(|k| &k.def).collect()
    }
}
