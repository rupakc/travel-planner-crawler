//! Fixtures for `vc-index` tests (owned by the vc-index task; add items here, never edit lib.rs).

use chrono::{DateTime, Utc};
use vc_core::ids::{CaptureId, CaptureRef, KindId, RecordKey, SourceId};
use vc_core::licence::{Licence, LicenceClass, SourceClass};
use vc_core::records::{ExtractedRecord, ExtractionMethod, ExtractorInfo, Provenance};
use vc_core::url::CanonicalUrl;
use vc_core::usepolicy::UsePolicy;

/// A structured-extraction record of `kind` with `key` and `data` (title from `data.name`).
pub fn record(kind: &str, key: &str, data: serde_json::Value) -> ExtractedRecord {
    let title = data
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    ExtractedRecord {
        kind: KindId::new(kind),
        schema_version: 1,
        record_key: RecordKey(key.to_string()),
        data,
        title,
        excerpt: None,
        lang: Some("en".into()),
        place_ids: vec![],
        quality: 0.8,
        conflict: false,
        use_policy: UsePolicy::ALLOW_ALL,
        extractor: ExtractorInfo {
            version: "test-1".into(),
            method: ExtractionMethod::Structured,
            model: None,
            prompt_hash: None,
            run_id: None,
        },
        observation: None,
    }
}

/// A licence of `class` (`spdx` optional) attributed to `attribution`.
pub fn licence(class: LicenceClass, spdx: Option<&str>, attribution: &str) -> Licence {
    Licence {
        class,
        spdx: spdx.map(str::to_string),
        attribution: attribution.to_string(),
        url: None,
    }
}

/// Provenance of a capture of `url` from `source` under `licence`, captured at `at`.
pub fn prov_at(source: &str, licence: Licence, url: &str, at: DateTime<Utc>) -> Provenance {
    let url = CanonicalUrl::parse(url).expect("fixture URL parses");
    Provenance {
        source_id: SourceId(source.to_string()),
        source_class: SourceClass::Reference,
        licence,
        url_key: url.key(),
        url,
        capture: CaptureRef {
            capture_id: CaptureId::at(at),
            captured_at: at,
        },
        use_policy: UsePolicy::ALLOW_ALL,
    }
}

/// [`prov_at`] captured now.
pub fn prov(source: &str, licence: Licence, url: &str) -> Provenance {
    prov_at(source, licence, url, Utc::now())
}

/// An open (CC0) licence.
pub fn open() -> Licence {
    licence(LicenceClass::Open, Some("CC0-1.0"), "Example Open Data")
}

/// A CC BY-SA 4.0 licence.
pub fn share_alike() -> Licence {
    licence(
        LicenceClass::ShareAlike,
        Some("CC-BY-SA-4.0"),
        "Wikivoyage contributors",
    )
}
