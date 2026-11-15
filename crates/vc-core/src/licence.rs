//! Licence and source classes (spec §5.9, §17.8).

use serde::{Deserialize, Serialize};
use url::Url;

/// How a source's data may be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LicenceClass {
    /// CC0, public domain, OGL, CC BY: merge into entities; attribution where required.
    Open,
    /// CC BY-SA, ODbL: separate records, never merged with other licences' attributes; served
    /// with licence + attribution; derived summaries carry the same licence.
    ShareAlike,
    /// Editorial content (blogs, news): only extracted facts + headline + ≤ 300-char excerpt +
    /// link; full text never served.
    FactsOnly,
    /// Usable per the provider's API terms; the source sets `max_retention_days` and
    /// `display_requirements` (e.g. Ticketmaster, Nager.Date).
    ApiTerms,
    /// Non-commercial or unknown: **not ingested in v1** (sources must be `enabled: false`).
    Restricted,
}

impl LicenceClass {
    /// Whether data of this class may be ingested at all.
    pub fn ingestible(&self) -> bool {
        !matches!(self, LicenceClass::Restricted)
    }

    /// The stable snake_case name stored in Postgres.
    pub fn as_str(&self) -> &'static str {
        match self {
            LicenceClass::Open => "open",
            LicenceClass::ShareAlike => "share_alike",
            LicenceClass::FactsOnly => "facts_only",
            LicenceClass::ApiTerms => "api_terms",
            LicenceClass::Restricted => "restricted",
        }
    }
}

/// What kind of publisher a source is; kind policies match on it (r5 Q12, §17.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceClass {
    /// Government or intergovernmental domains/APIs only.
    Official,
    /// Reference works (Wikipedia, Wikidata, Wikivoyage).
    Reference,
    /// Editorial publishers (guides, blogs, news).
    Editorial,
    /// Community content (forums, Q&A dumps).
    Community,
    /// Commercial APIs (e.g. Ticketmaster).
    CommercialApi,
}

impl SourceClass {
    /// The stable snake_case name stored in Postgres.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceClass::Official => "official",
            SourceClass::Reference => "reference",
            SourceClass::Editorial => "editorial",
            SourceClass::Community => "community",
            SourceClass::CommercialApi => "commercial_api",
        }
    }
}

/// A source's licence, carried by every API item as `licence {class, spdx, attribution, url}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Licence {
    /// The licence class.
    pub class: LicenceClass,
    /// The SPDX id, when there is one (e.g. `CC-BY-SA-4.0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spdx: Option<String>,
    /// The attribution string to display.
    pub attribution: String,
    /// The licence or terms URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<Url>,
}
