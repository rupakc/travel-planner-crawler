//! `SourceSpec` and `Denylist` against the real files produced by Task 0B.
//!
//! Always checks the fixtures copied into `tests/fixtures/`; additionally checks every
//! `sources/*.yaml` and `policy/denylist.yaml` of the workspace when they exist (after 0B
//! merges), or of `$VC_SOURCES_DIR` / `$VC_DENYLIST` when set.

use std::path::{Path, PathBuf};

use vc_core::licence::{LicenceClass, SourceClass};
use vc_core::policy::Denylist;
use vc_core::sources::{Cadence, Method, SourceSpec};

fn load(path: &Path) -> SourceSpec {
    let text = std::fs::read_to_string(path).unwrap();
    let spec: SourceSpec =
        serde_norway::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let stem = path.file_stem().unwrap().to_str().unwrap();
    assert_eq!(spec.id.as_str(), stem, "id equals the file stem");
    if spec.licence.class == LicenceClass::Restricted {
        assert!(!spec.enabled, "{stem}: restricted sources must be disabled");
    }
    spec
}

fn yaml_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default();
    v.retain(|p| p.extension().is_some_and(|e| e == "yaml"));
    v.sort();
    v
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn fixture_sources_parse() {
    let files = yaml_files(&manifest_dir().join("tests/fixtures/sources"));
    assert!(files.len() >= 3);
    for f in &files {
        load(f);
    }
    let fcdo = load(&manifest_dir().join("tests/fixtures/sources/fcdo-content-api.yaml"));
    assert_eq!(fcdo.source_class, SourceClass::Official);
    assert_eq!(fcdo.licence.class, LicenceClass::Open);
    assert_eq!(fcdo.method, Method::Api);
    assert_eq!(fcdo.cadence, Cadence::OnDemand);
    assert_eq!(fcdo.adapter.as_deref(), Some("fcdo_content_api"));
    assert!(fcdo.politeness_key.is_none() && fcdo.sitemaps.is_empty());
}

#[test]
fn workspace_sources_parse_when_present() {
    let dir = std::env::var_os("VC_SOURCES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir().join("../../sources"));
    for f in yaml_files(&dir) {
        load(&f);
    }
}

#[test]
fn denylist_parses() {
    let mut files = vec![manifest_dir().join("tests/fixtures/denylist.yaml")];
    let ws = std::env::var_os("VC_DENYLIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir().join("../../policy/denylist.yaml"));
    if ws.exists() {
        files.push(ws);
    }
    for f in files {
        let d: Denylist = serde_norway::from_str(&std::fs::read_to_string(&f).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        assert_eq!(d.version, 1);
        assert!(d.entries.iter().any(|e| e.brand() == Some("expedia")));
        assert!(
            d.entries
                .iter()
                .any(|e| e.path_prefix.as_deref() == Some("/travel"))
        );
    }
}
