//! Fixtures for `vc-policy` tests (owned by the vc-policy task; add items here, never edit lib.rs).
//!
//! Files live in `testkit/fixtures/challenges/` (bot challenges and interstitials, each listed
//! in its `manifest.json`) and `testkit/fixtures/vc-policy/` (Google's robots.txt test cases,
//! normal pages that must not look like challenges).

use std::path::PathBuf;

/// The repository root.
pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("testkit lives under the repo root")
        .to_path_buf()
}

/// `testkit/fixtures/challenges/`.
pub fn challenges_dir() -> PathBuf {
    repo_root().join("testkit/fixtures/challenges")
}

/// `testkit/fixtures/vc-policy/`.
pub fn policy_fixtures_dir() -> PathBuf {
    repo_root().join("testkit/fixtures/vc-policy")
}

/// `policy/denylist.yaml`.
pub fn denylist_path() -> PathBuf {
    repo_root().join("policy/denylist.yaml")
}

/// `policy/subresource_allowlist.yaml`.
pub fn subresource_allowlist_path() -> PathBuf {
    repo_root().join("policy/subresource_allowlist.yaml")
}
