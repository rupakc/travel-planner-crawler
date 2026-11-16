//! The E2E stack's files agree with the fake web and with the crawler's own loaders: every
//! scripted site resolves to the fake web, the trap sits on the private IP the sites use, the
//! sources and policy files load and validate, and the config loads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use testkit::fake_web::sites;

fn e2e() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn compose() -> serde_norway::Value {
    let s = std::fs::read_to_string(e2e().join("compose.yaml")).expect("read compose.yaml");
    serde_norway::from_str(&s).expect("compose.yaml parses")
}

fn service<'a>(c: &'a serde_norway::Value, name: &str) -> &'a serde_norway::Value {
    &c["services"][name]
}

#[test]
fn every_site_and_the_denied_host_resolve_to_the_fake_web() {
    let c = compose();
    let aliases: BTreeSet<String> = service(&c, "fake-web")["networks"]["web"]["aliases"]
        .as_sequence()
        .expect("fake-web aliases")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    for h in sites::HOSTS.iter().chain([&sites::DENIED]) {
        assert!(aliases.contains(*h), "{h} is not an alias of fake-web");
    }
}

#[test]
fn the_trap_sits_on_the_sites_private_ip_and_fetchers_can_route_to_it() {
    let c = compose();
    assert_eq!(
        service(&c, "trap")["networks"]["trapnet"]["ipv4_address"].as_str(),
        Some(sites::PRIVATE_IP)
    );
    for f in ["fetcher-a", "fetcher-b"] {
        let nets: Vec<&str> = service(&c, f)["networks"]
            .as_sequence()
            .expect("networks list")
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for n in ["trapnet", "web", "render", "db", "store"] {
            assert!(nets.contains(&n), "{f} is not on {n}");
        }
    }
}

#[test]
fn every_network_is_internal_and_the_web_looks_public_to_the_ssrf_guard() {
    let c = compose();
    for (name, net) in c["networks"].as_mapping().expect("networks") {
        assert_eq!(
            net["internal"].as_bool(),
            Some(true),
            "network {name:?} is not internal"
        );
    }
    let subnet = c["networks"]["web"]["ipam"]["config"][0]["subnet"]
        .as_str()
        .expect("web subnet");
    let gw: std::net::IpAddr = subnet.split('/').next().unwrap().parse().unwrap();
    assert_eq!(
        vc_policy::ssrf::ip_rule(gw),
        None,
        "web subnet {subnet} is blocked by the SSRF guard"
    );
    let trap: std::net::IpAddr = sites::PRIVATE_IP.parse().unwrap();
    assert!(
        vc_policy::ssrf::ip_rule(trap).is_some(),
        "the trap IP must be one the guard blocks"
    );
}

#[test]
fn every_e2e_source_loads_validates_and_targets_the_fake_web() {
    let dir = e2e().join("sources");
    let specs = vc_sources::load_dir(&dir).expect("load e2e/sources");
    assert_eq!(specs.len(), 6, "one source per crawled fake site");
    let deny = vc_sources::load_denylist(&e2e().join("policy/denylist.yaml")).expect("denylist");
    let adapters = vc_sources::adapters::ids();
    for s in &specs {
        let v = vc_sources::validate(
            s,
            &vc_sources::ValidateOpts {
                file_stem: Some(s.id.0.as_str()),
                adapters: &adapters,
                denylist: Some(&deny),
            },
        );
        assert!(v.errors.is_empty(), "{}: {:?}", s.id, v.errors);
        let urls = s.seeds.iter().chain(&s.sitemaps).chain(&s.feeds);
        for u in urls {
            let host = u.host_str().unwrap().to_string();
            assert!(
                sites::HOSTS.contains(&host.as_str()),
                "{}: {u} is not a fake site",
                s.id
            );
        }
    }
}

fn canon(s: &str) -> vc_core::CanonicalUrl {
    vc_core::CanonicalUrl::parse(s).expect("url")
}

#[test]
fn the_denylist_holds_the_denied_host_and_the_allowlist_the_cdn() {
    let deny = vc_sources::load_denylist(&e2e().join("policy/denylist.yaml")).expect("denylist");
    assert!(
        vc_sources::deny_match(&deny, &canon(&format!("http://{}/deal", sites::DENIED))).is_some()
    );
    assert!(vc_sources::deny_match(&deny, &canon(&format!("http://{}/", sites::BLOG))).is_none());
    let allow = std::fs::read_to_string(e2e().join("policy/subresource_allowlist.yaml")).unwrap();
    let allow: serde_norway::Value = serde_norway::from_str(&allow).unwrap();
    assert_eq!(allow["hosts"][0]["host"].as_str(), Some(sites::CDN));
}

#[test]
fn the_e2e_config_loads_with_production_politeness() {
    let cfg: vc_config::Config = vc_config::Config::figment(Some(&e2e().join("vcrawl.toml")))
        .extract()
        .expect("e2e/vcrawl.toml loads");
    cfg.validate().expect("e2e/vcrawl.toml validates");
    let d = vc_config::Config::default();
    assert_eq!(
        cfg.frontier.min_delay_s, d.frontier.min_delay_s,
        "politeness floor relaxed"
    );
    assert_eq!(cfg.render.subresource_gap_ms, 250);
    assert_eq!(cfg.frontier.park_initial_h, d.frontier.park_initial_h);
    assert!(!cfg.llm.enabled, "nothing may reach Vertex from E2E");
    assert_eq!(cfg.store.gcs_endpoint, "http://fake-gcs:4443");
    assert!(cfg.render.enabled);
}

#[test]
fn every_vc_env_key_in_compose_is_a_real_setting() {
    let c = compose();
    let mut keys = Vec::new();
    for (_, svc) in c["services"].as_mapping().expect("services") {
        env_keys(&svc["environment"], &mut keys);
    }
    env_keys(&c["x-vcrawl"]["environment"], &mut keys);
    assert!(
        keys.iter().any(|k| k == "VC_VCRAWL_INSTANCE"),
        "role env not found: {keys:?}"
    );
    let bad = vc_config::unmapped_env_keys(keys.iter().map(String::as_str));
    assert!(
        bad.is_empty(),
        "compose.yaml sets VC_* names no setting reads: {bad:?}"
    );
}

/// Env keys of a compose `environment` mapping, following `<<` merges if the parser kept them.
fn env_keys(v: &serde_norway::Value, out: &mut Vec<String>) {
    if let Some(m) = v.as_mapping() {
        for (k, val) in m {
            match k.as_str() {
                Some("<<") => env_keys(val, out),
                Some(k) => out.push(k.to_string()),
                None => {}
            }
        }
    }
}
