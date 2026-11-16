//! # e2e-runner (plan Task 16)
//!
//! Drives the E2E compose stack (`e2e/compose.yaml`) through a fixed number of crawl cycles
//! and then checks, against what really happened:
//!
//! | assertion | how |
//! |---|---|
//! | `polite` | testkit [`check_polite`] over the fake web's request log × its policy snapshot (overlap, delay, 250 ms sub-resource gap, robots, denylist, parks, the one UA) |
//! | `raw_first` | testkit [`check_raw_first`] over Postgres × the fake GCS bucket |
//! | `version_bump` | the blog post changed in cycle 1 has ≥ 2 record versions with different data |
//! | `api_provenance` | `/v1/search` over a verified TLS handshake (our CA only) answers items with provenance on the fake web |
//! | `challenge_parked` | `hostile.test` is parked (`politeness_keys`) and got no page request after its first challenge |
//! | `spa_rendered` | a `render` capture of the SPA whose WARC holds the script-built DOM |
//! | `private_ip_blocked` | the trap server on `10.0.0.5` (reachable from the fetcher) saw nothing |
//! | `two_fetchers` | two fetcher instances heartbeat |
//!
//! Each assertion names the `vcrawl` roles it needs. While a role is not wired yet (Task 15
//! lands them one by one), `run.sh` reports it in `E2E_MISSING_ROLES` and the assertion is
//! **skipped with the role's name**, never passed: a crawl that did not happen proves
//! nothing. A role that crashed (`E2E_CRASHED_ROLES`) fails every assertion that needs it.

pub mod checks;
pub mod stack;

use std::collections::BTreeSet;
use std::fmt;

pub use testkit::oracle::{check_polite, check_raw_first};

/// The `vcrawl` roles the stack runs.
pub const ROLES: [&str; 7] = [
    "migrate",
    "scheduler",
    "fetcher",
    "processor",
    "embedder",
    "api",
    "maintenance",
];

/// One assertion: its name and the roles it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// Name (stable; CI output and the workflow summary use it).
    pub name: &'static str,
    /// Roles that must run for the assertion to mean anything.
    pub needs: &'static [&'static str],
}

/// Every assertion, in run order.
pub const ASSERTIONS: [Spec; 8] = [
    Spec {
        name: "polite",
        needs: &["scheduler", "fetcher"],
    },
    Spec {
        name: "raw_first",
        needs: &["scheduler", "fetcher", "processor"],
    },
    Spec {
        name: "version_bump",
        needs: &["scheduler", "fetcher", "processor"],
    },
    Spec {
        name: "api_provenance",
        needs: &["scheduler", "fetcher", "processor", "api"],
    },
    Spec {
        name: "challenge_parked",
        needs: &["scheduler", "fetcher"],
    },
    Spec {
        name: "spa_rendered",
        needs: &["scheduler", "fetcher"],
    },
    Spec {
        name: "private_ip_blocked",
        needs: &["scheduler", "fetcher"],
    },
    Spec {
        name: "two_fetchers",
        needs: &["fetcher"],
    },
];

/// Assertions that fail today because of known crawler bugs, with the bugs. An expected
/// failure does not fail the run, but an expected failure that **passes** does: whoever fixes
/// the bugs must remove the entry, so the assertion is blocking again.
pub const XFAIL: &[(&str, &str)] = &[(
    "polite",
    "known bugs: (1) Crawl-delay learned from robots.txt is not applied to the rest of the same \
     claim (crates/vcrawl/src/roles/fetcher.rs: run.crawl_delay only reaches the frontier at \
     completion; slow.test/ fetched 5 s after a Crawl-delay: 10 robots.txt); (2) render \
     sub-requests to a third-party key (cdn.test) are paced only within the page's permit, not \
     against that key's own claims (25-241 ms after cdn.test/robots.txt)",
)];

/// Applies [`XFAIL`] to an assertion's outcome.
pub fn apply_xfail(name: &str, o: Outcome) -> Outcome {
    let Some((_, why)) = XFAIL.iter().find(|(n, _)| *n == name) else {
        return o;
    };
    match o {
        Outcome::Fail(m) => Outcome::XFail(format!("{why}\n    observed: {m}")),
        Outcome::Pass(m) => Outcome::Fail(format!(
            "passed but is listed in XFAIL (e2e/runner/src/lib.rs): the bugs look fixed, remove \
             the entry so the assertion blocks again ({m})"
        )),
        other => other,
    }
}

/// What `run.sh` found out about the roles.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleStatus {
    /// Roles that exit with "not wired yet" (Task 15 has not landed them).
    pub missing: BTreeSet<String>,
    /// Roles that exited for any other reason (a real failure).
    pub crashed: BTreeSet<String>,
}

impl RoleStatus {
    /// Parses the comma-separated lists `run.sh` passes (`E2E_MISSING_ROLES`,
    /// `E2E_CRASHED_ROLES`). Unknown role names are an error: a typo must not turn into a skip.
    pub fn parse(missing: &str, crashed: &str) -> Result<RoleStatus, String> {
        let list = |s: &str| -> Result<BTreeSet<String>, String> {
            s.split(',')
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(|r| {
                    if ROLES.contains(&r) {
                        Ok(r.to_string())
                    } else {
                        Err(format!("unknown role {r:?} (known: {})", ROLES.join(", ")))
                    }
                })
                .collect()
        };
        Ok(RoleStatus {
            missing: list(missing)?,
            crashed: list(crashed)?,
        })
    }

    /// Reads `E2E_MISSING_ROLES` and `E2E_CRASHED_ROLES`.
    pub fn from_env() -> Result<RoleStatus, String> {
        let get = |k: &str| std::env::var(k).unwrap_or_default();
        RoleStatus::parse(&get("E2E_MISSING_ROLES"), &get("E2E_CRASHED_ROLES"))
    }

    /// The verdict for `spec` before it runs: `Some(Skip)` naming the missing roles,
    /// `Some(Fail)` naming the crashed ones, `None` = run it.
    pub fn gate(&self, spec: &Spec) -> Option<Outcome> {
        let crashed: Vec<&str> = spec
            .needs
            .iter()
            .copied()
            .filter(|r| self.crashed.contains(*r))
            .collect();
        if !crashed.is_empty() {
            return Some(Outcome::Fail(format!(
                "role {} crashed (see `docker compose logs`)",
                crashed.join(", ")
            )));
        }
        let missing: Vec<&str> = spec
            .needs
            .iter()
            .copied()
            .filter(|r| self.missing.contains(*r))
            .collect();
        if missing.is_empty() {
            None
        } else {
            Some(Outcome::Skip(format!(
                "vcrawl role {} not wired yet (Task 15)",
                missing.join(", ")
            )))
        }
    }
}

/// The result of one assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Held.
    Pass(String),
    /// Not run (why).
    Skip(String),
    /// Failed as expected (a known bug, listed in [`XFAIL`]): not a run failure.
    XFail(String),
    /// Broken (why).
    Fail(String),
}

impl Outcome {
    /// `Pass` when `problems` is empty, else `Fail` listing them.
    pub fn from_problems(ok: impl Into<String>, problems: Vec<String>) -> Outcome {
        if problems.is_empty() {
            Outcome::Pass(ok.into())
        } else {
            let mut s = format!("{} problem(s):", problems.len());
            for p in problems.iter().take(40) {
                s.push_str("\n    ");
                s.push_str(p);
            }
            if problems.len() > 40 {
                s.push_str(&format!("\n    … and {} more", problems.len() - 40));
            }
            Outcome::Fail(s)
        }
    }

    /// Whether it failed.
    pub fn is_fail(&self) -> bool {
        matches!(self, Outcome::Fail(_))
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Pass(m) => write!(f, "PASS  {m}"),
            Outcome::Skip(m) => write!(f, "SKIP  {m}"),
            Outcome::XFail(m) => write!(f, "XFAIL {m}"),
            Outcome::Fail(m) => write!(f, "FAIL  {m}"),
        }
    }
}

/// The run's summary table (Markdown, for `$GITHUB_STEP_SUMMARY`) and whether it failed.
pub fn summary(results: &[(&'static str, Outcome)]) -> (String, bool) {
    let mut md = String::from("| assertion | result | detail |\n|---|---|---|\n");
    let mut failed = false;
    for (name, o) in results {
        let (tag, msg) = match o {
            Outcome::Pass(m) => ("pass", m),
            Outcome::Skip(m) => ("SKIP", m),
            Outcome::XFail(m) => ("xfail", m),
            Outcome::Fail(m) => {
                failed = true;
                ("**FAIL**", m)
            }
        };
        let first = msg.lines().next().unwrap_or_default().replace('|', "\\|");
        md.push_str(&format!("| `{name}` | {tag} | {first} |\n"));
    }
    (md, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> Spec {
        *ASSERTIONS.iter().find(|s| s.name == name).expect("known")
    }

    #[test]
    fn missing_role_skips_and_names_it() {
        let rs = RoleStatus::parse("processor, api", "").unwrap();
        assert_eq!(rs.gate(&spec("polite")), None);
        match rs.gate(&spec("api_provenance")) {
            Some(Outcome::Skip(m)) => {
                assert!(m.contains("processor, api"), "{m}");
                assert!(m.contains("Task 15"), "{m}");
            }
            other => panic!("expected a skip, got {other:?}"),
        }
    }

    #[test]
    fn crashed_role_fails_rather_than_skips() {
        let rs = RoleStatus::parse("api", "fetcher").unwrap();
        assert!(
            matches!(rs.gate(&spec("api_provenance")), Some(Outcome::Fail(m)) if m.contains("fetcher"))
        );
        assert!(matches!(
            rs.gate(&spec("two_fetchers")),
            Some(Outcome::Fail(_))
        ));
    }

    #[test]
    fn unknown_role_name_is_an_error_not_a_skip() {
        assert!(RoleStatus::parse("fetchr", "").is_err());
        assert!(RoleStatus::parse("", "proccessor").is_err());
        assert_eq!(RoleStatus::parse(" , ", "").unwrap(), RoleStatus::default());
    }

    #[test]
    fn every_assertion_needs_only_known_roles() {
        let mut names = BTreeSet::new();
        for s in ASSERTIONS {
            assert!(names.insert(s.name), "duplicate {}", s.name);
            assert!(
                !s.needs.is_empty(),
                "{} needs no role: it would always run",
                s.name
            );
            for r in s.needs {
                assert!(ROLES.contains(r), "{}: unknown role {r}", s.name);
            }
        }
    }

    #[test]
    fn xfail_turns_a_fail_into_xfail_and_a_pass_into_a_fail() {
        let name = XFAIL[0].0;
        let o = apply_xfail(name, Outcome::Fail("ShortGap".into()));
        assert!(
            matches!(&o, Outcome::XFail(m) if m.contains("ShortGap") && m.contains("known bugs"))
        );
        assert!(!o.is_fail());
        let o = apply_xfail(name, Outcome::Pass("clean".into()));
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("remove")),
            "{o}"
        );
        assert_eq!(
            apply_xfail(name, Outcome::Skip("s".into())),
            Outcome::Skip("s".into())
        );
        // Others are untouched.
        assert_eq!(
            apply_xfail("raw_first", Outcome::Fail("x".into())),
            Outcome::Fail("x".into())
        );
        let (_, failed) = summary(&[(name, Outcome::XFail("k".into()))]);
        assert!(!failed);
    }

    #[test]
    fn only_polite_is_expected_to_fail() {
        let names: Vec<&str> = XFAIL.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["polite"]);
        for (n, _) in XFAIL {
            assert!(ASSERTIONS.iter().any(|s| s.name == *n), "unknown {n}");
        }
    }

    #[test]
    fn summary_fails_only_on_fail() {
        let (md, failed) = summary(&[
            ("polite", Outcome::Pass("ok".into())),
            (
                "raw_first",
                Outcome::Skip("role processor not wired".into()),
            ),
        ]);
        assert!(!failed);
        assert!(
            md.contains("| `raw_first` | SKIP | role processor not wired |"),
            "{md}"
        );
        let (_, failed) = summary(&[("x", Outcome::Fail("a|b\nmore".into()))]);
        assert!(failed);
    }

    #[test]
    fn from_problems_lists_them() {
        assert_eq!(
            Outcome::from_problems("fine", vec![]),
            Outcome::Pass("fine".into())
        );
        let o = Outcome::from_problems("fine", vec!["a".into(), "b".into()]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.starts_with("2 problem(s)") && m.contains("\n    b"))
        );
    }
}
