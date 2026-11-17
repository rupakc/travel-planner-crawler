//! Include/exclude rules match the full URL or the path (`^/booking/` must exclude).

mod crawl_path_support;

use crawl_path_support::spec;
use vc_core::CanonicalUrl;
use vcrawl::data::sources::LoadedSource;

fn src(extra: &str) -> LoadedSource {
    let s = spec(&format!(
        "id: scope-test\nmethod: html\ncadence: weekly\nseeds: [\"https://tours.example/\"]\n{extra}"
    ));
    LoadedSource::new(s, true).expect("source")
}

fn u(s: &str) -> CanonicalUrl {
    CanonicalUrl::parse(s).expect("url")
}

#[test]
fn path_style_excludes_exclude() {
    let s = src("exclude: [\"^/booking/\", \"^/account/\"]\n");
    assert!(!s.in_scope(&u("https://tours.example/booking/123")));
    assert!(!s.in_scope(&u("https://tours.example/account/")));
    assert!(s.in_scope(&u("https://tours.example/city/rome")));
}

#[test]
fn full_url_excludes_still_exclude() {
    let s = src("exclude: [\"^https://tours\\\\.example/booking/\"]\n");
    assert!(!s.in_scope(&u("https://tours.example/booking/1")));
    assert!(s.in_scope(&u("https://tours.example/city/1")));
}

#[test]
fn path_style_includes_include_and_other_sites_stay_out() {
    let s = src("include: [\"^/blog/\"]\n");
    assert!(s.in_scope(&u("https://tours.example/blog/scams")));
    assert!(!s.in_scope(&u("https://tours.example/shop/")));
    assert!(!s.in_scope(&u("https://other.example/blog/x")));
}

#[test]
fn every_shipped_spec_exclude_rule_can_match_something_on_its_site() {
    // Guard against rules that look like paths but can never match (the old bug).
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sources");
    for e in std::fs::read_dir(dir).expect("sources dir") {
        let p = e.expect("entry").path();
        if p.extension().and_then(|x| x.to_str()) != Some("yaml") {
            continue;
        }
        let spec =
            vc_sources::parse_spec(&std::fs::read_to_string(&p).expect("read")).expect("spec");
        let Some(base) = spec
            .seeds
            .iter()
            .chain(spec.sitemaps.iter())
            .chain(spec.feeds.iter())
            .next()
            .cloned()
        else {
            continue;
        };
        let s = LoadedSource::new(spec.clone(), true).expect("source");
        for rule in &spec.exclude {
            if let Some(path) = rule.strip_prefix('^').filter(|r| r.starts_with('/')) {
                let lit: String = path
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || "/-_".contains(*c))
                    .collect();
                let probe = base.join(&format!("{lit}x")).expect("join");
                let probe = u(probe.as_str());
                assert!(
                    !s.in_scope(&probe),
                    "{}: exclude {rule} does not exclude {probe}",
                    spec.id
                );
            }
        }
    }
}
