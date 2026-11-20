//! Place facts (2026-11-20): `wikidata_facts` (Wikidata EntityData and dump lines),
//! `wiki_hooks` (Wikipedia "Did you know" / "On this day" / unusual-articles wikitext) and
//! `factbook_json` (CIA World Factbook final edition via factbook.json), against small
//! fixtures (`testkit/fixtures/vc-sources/README.md`). Every item must fit `place_fact.v1`.

mod common;

use std::io::Write;

use common::{Harness, assert_raw_first, fixture, t0};
use serde_json::{Map, Value, json};
use vc_core::{CanonicalUrl, Error, SourceId};
use vc_sources::adapters;
use vc_sources::adapters::factbook_json::{self, FactbookJson};
use vc_sources::adapters::records::{AdapterItem, MAPPED, records};
use vc_sources::adapters::wiki_hooks::{self, HookPage, WikiHooks};
use vc_sources::adapters::wikidata_facts::{self, WikidataFacts};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const Q64: &str = "https://www.wikidata.org/wiki/Special:EntityData/Q64.json";
const FACTBOOK: &str =
    "https://raw.githubusercontent.com/factbook/factbook.json/master/europe/gm.json";

fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(common::repo_root().join("schemas/place_fact.v1.json")).unwrap(),
    )
    .unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

fn fit(items: &[AdapterItem]) {
    let v = validator();
    for i in items {
        assert_eq!(i.kind, "place_fact");
        assert!(i.id.is_some());
        assert!(
            v.is_valid(&i.data),
            "{}: {}",
            i.id.as_deref().unwrap_or(""),
            i.data
        );
        let n = i.data["text"].as_str().unwrap().chars().count();
        assert!((8..=280).contains(&n));
    }
}

fn by_field<'a>(items: &'a [AdapterItem], field: &str) -> Vec<&'a AdapterItem> {
    items
        .iter()
        .filter(|i| i.data["source_field"] == field)
        .collect()
}

fn text(i: &AdapterItem) -> &str {
    i.data["text"].as_str().unwrap()
}

/// The labels of the referenced fixture entities, as the adapter bundles them.
fn ref_labels() -> Map<String, Value> {
    let refs: Value = serde_json::from_slice(&fixture("wikidata_entity_refs.json")).unwrap();
    let mut out = Map::new();
    for (q, body) in refs.as_object().unwrap() {
        let e = &body["entities"][q];
        let mut labels = Map::new();
        for (lang, l) in e["labels"].as_object().unwrap() {
            labels.insert(lang.clone(), l["value"].clone());
        }
        let mut entry = json!({"labels": labels});
        if let Some(iso) = e["claims"]["P297"][0]["mainsnak"]["datavalue"]["value"].as_str() {
            entry["iso2"] = json!(iso);
        }
        out.insert(q.clone(), entry);
    }
    out
}

fn berlin() -> Value {
    let body = fixture("wikidata_entity_q64.json");
    wikidata_facts::entity_of(&body, "Q64")
        .unwrap()
        .expect("Berlin is a place")
}

#[test]
fn the_three_adapters_are_registered_and_mapped() {
    for id in ["wikidata_facts", "wiki_hooks", "factbook_json"] {
        assert!(adapters::get(id).is_some(), "{id}");
        assert!(MAPPED.contains(&id), "{id}");
        assert!(adapters::registry().iter().any(|a| a.id() == id), "{id}");
    }
}

// ---------------------------------------------------------------------------------------------
// wikidata_facts

#[test]
fn entity_data_urls_name_their_qid() {
    let q = |u: &str| wikidata_facts::entity_data_qid(&CanonicalUrl::parse(u).unwrap());
    assert_eq!(q(Q64).as_deref(), Some("Q64"));
    assert_eq!(
        q("https://www.wikidata.org/wiki/Special:EntityData/Q064.json"),
        None
    );
    assert_eq!(
        q("https://www.wikidata.org/wiki/Special:EntityData/P31.json"),
        None
    );
    assert_eq!(q("https://www.wikidata.org/wiki/Q64"), None);
}

#[test]
fn slim_keeps_the_fact_properties_labels_and_hook_sitelinks() {
    let e = berlin();
    assert_eq!(e["id"], "Q64");
    assert_eq!(e["labels"]["en"], "Berlin");
    let claims = e["claims"].as_object().unwrap();
    for p in [
        "P1566", "P571", "P793", "P1082", "P190", "P47", "P610", "P17",
    ] {
        assert!(claims.contains_key(p), "{p}");
    }
    assert!(
        !claims.contains_key("P31"),
        "only the fact and key properties"
    );
    let links: Vec<&str> = e["sitelinks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(links.contains(&"dewiki:Berlin") && links.contains(&"frwiki:Berlin"));
    assert!(
        !links.iter().any(|l| l.starts_with("enwiki:")),
        "enwiki has no hook pages listed"
    );
    // Not a place: no GeoNames id and no ISO code.
    let refs: Value = serde_json::from_slice(&fixture("wikidata_entity_refs.json")).unwrap();
    let olympics = serde_json::to_vec(&refs["Q8150"]).unwrap();
    assert!(
        wikidata_facts::entity_of(&olympics, "Q8150")
            .unwrap()
            .is_none()
    );
}

#[test]
fn referenced_items_put_the_current_country_first() {
    let r = wikidata_facts::referenced(&berlin());
    assert_eq!(r[0], "Q183", "the P17 without an end time");
    assert!(
        !r.contains(&"Q148499".to_string()),
        "a former country is not fetched"
    );
    for q in [
        "Q19259618",
        "Q65079568",
        "Q8150",
        "Q65",
        "Q90",
        "Q1208",
        "Q6115",
    ] {
        assert!(r.contains(&q.to_string()), "{q}");
    }
    assert!(r.len() <= wikidata_facts::MAX_REFERENCED);
}

#[test]
fn berlin_facts_are_templated_sentences_with_a_keyed_place() {
    let body = serde_json::to_vec(&wikidata_facts::bundle(
        berlin(),
        ref_labels().into_iter().collect(),
    ))
    .unwrap();
    let it = records("wikidata_facts", &CanonicalUrl::parse(Q64).unwrap(), &body)
        .unwrap()
        .unwrap();
    fit(&it);
    for i in &it {
        let p = &i.data["place"];
        assert_eq!(p["wikidata_qid"], "Q64");
        assert_eq!(p["geonames_id"], 2_950_159, "the first best P1566");
        assert_eq!(p["country"], "DE", "from the current country's P297");
        assert_eq!(p["admin_level"], "admin1", "Berlin has an ISO 3166-2 code");
        assert_eq!(i.data["licence"]["spdx"], "CC0-1.0");
        assert_eq!(i.data["confidence"], "structured");
        assert_eq!(i.data["text_origin"], "templated");
        assert_eq!(i.data["llm_ok"], true);
        assert!(
            i.data["source_url"]
                .as_str()
                .unwrap()
                .starts_with("https://www.wikidata.org/wiki/Q64#P")
        );
        assert_eq!(i.data["as_of"].as_str().unwrap().len(), 10);
    }
    let one = |f: &str| {
        let v = by_field(&it, f);
        assert_eq!(v.len(), 1, "{f}: {v:?}");
        v[0]
    };
    let founded = one("wikidata:P571");
    assert_eq!(text(founded), "Berlin was founded in 1244.");
    assert_eq!(founded.data["fact_type"], "historical");
    assert_eq!(founded.data["date_ref"], "1244");
    assert_eq!(
        founded.data["source"]["sourced"], "yes",
        "the statement has a reference"
    );

    let pop = one("wikidata:P1082");
    assert_eq!(pop.data["fact_type"], "popular");
    assert_eq!(text(pop), "Berlin had a population of 3,700,577 in 2025.");
    assert_eq!(
        pop.data["quantity"],
        json!({"value": 3_700_577.0, "unit": "people"})
    );

    let high = one("wikidata:P610");
    assert_eq!(high.data["fact_type"], "unbelievable");
    assert_eq!(
        text(high),
        "The highest point of Berlin is Arkenberge (121.9 m)."
    );
    assert_eq!(
        high.data["quantity"]["rank_scope"],
        "highest point of Berlin"
    );

    let area = one("wikidata:P2046");
    assert_eq!(
        text(area),
        "Berlin covers an area of 891.12 km\u{b2}.",
        "the preferred statement"
    );

    let twins = one("wikidata:P190");
    assert_eq!(twins.data["fact_type"], "little_known");
    assert_eq!(text(twins), "Berlin is twinned with Los Angeles and Paris.");
    let borders = one("wikidata:P47");
    assert_eq!(
        text(borders),
        "Berlin shares a border with Brandenburg and Barnim District."
    );

    let events = by_field(&it, "wikidata:P793");
    assert_eq!(events.len(), 2);
    assert!(events.iter().any(|e| text(e)
        == "A key event in the history of Berlin: Fall of Berlin, on 27 October 1806."
        && e.data["date_ref"] == "1806-10-27"));
    assert!(
        events
            .iter()
            .any(|e| text(e).contains("1936 Summer Olympics, in 1936"))
    );

    let nick = one("wikidata:P1449");
    assert_eq!(
        text(nick),
        "Berlin is nicknamed \u{201c}Spree-Athen\u{201d}."
    );
    // The official name equals the label: no fact.
    assert!(by_field(&it, "wikidata:P1448").is_empty());
    let elev = one("wikidata:P2044");
    assert_eq!(text(elev), "Berlin lies 34 m above sea level.");

    // Ids are unique (record keys).
    let mut ids: Vec<&str> = it.iter().map(|i| i.id.as_deref().unwrap()).collect();
    ids.sort_unstable();
    let n = ids.len();
    ids.dedup();
    assert_eq!(ids.len(), n);
}

#[test]
fn without_labels_only_label_free_facts_are_made() {
    // The dump route: a JSON array of slim entities, no labels of referenced items.
    let body = serde_json::to_vec(&json!([berlin()])).unwrap();
    let it = records(
        "wikidata_facts",
        &CanonicalUrl::parse(
            "https://dumps.wikimedia.org/wikidatawiki/entities/latest-all.json.gz",
        )
        .unwrap(),
        &body,
    )
    .unwrap()
    .unwrap();
    fit(&it);
    assert!(!by_field(&it, "wikidata:P571").is_empty());
    assert!(by_field(&it, "wikidata:P190").is_empty());
    assert!(by_field(&it, "wikidata:P610").is_empty());
    // No country label either: the place is still keyed by its QID and GeoNames id.
    assert!(it.iter().all(|i| i.data["place"].get("country").is_none()));
}

#[test]
fn a_raw_entity_data_body_is_not_a_bundle() {
    let err = records(
        "wikidata_facts",
        &CanonicalUrl::parse(Q64).unwrap(),
        &fixture("wikidata_entity_q64.json"),
    )
    .unwrap()
    .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)));
}

#[test]
fn bce_and_deprecated_statements() {
    let e = json!({"id": "Q1", "labels": {"en": "Old Town"}, "sitelinks": [],
    "claims": {
        "P1566": [{"rank": "normal", "value": "123", "q": {}, "refs": false}],
        "P571": [{"rank": "normal", "value": {"time": "-0500-00-00T00:00:00Z", "precision": 9}, "q": {}, "refs": false}],
        "P1451": [{"rank": "normal", "value": {"text": "Semper fidelis", "language": "la"}, "q": {}, "refs": false}]
    }});
    let it: Vec<(String, Value)> = wikidata_facts::facts(&e, &Map::new());
    let f = it
        .iter()
        .find(|(_, d)| d["source_field"] == "wikidata:P571")
        .unwrap();
    assert_eq!(f.1["text"], "Old Town was founded in 500 BCE.");
    assert_eq!(f.1["date_ref"], "-0500");
    assert_eq!(f.1["source"]["sourced"], "no");
    assert!(
        it.iter()
            .any(|(_, d)| d["text"] == "The motto of Old Town is \u{201c}Semper fidelis\u{201d}.")
    );
    // A deprecated statement is dropped when slimming.
    let raw = json!({"id": "Q2", "claims": {"P1566": [{"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "9"}}}],
        "P571": [{"rank": "deprecated", "mainsnak": {"snaktype": "value", "datavalue": {"value": {"time": "+1900-00-00T00:00:00Z", "precision": 9}}}}]},
        "labels": {"en": {"value": "X Town"}}});
    let s = wikidata_facts::slim(&raw).unwrap();
    assert!(s["claims"].get("P571").is_none());
}

fn entity_data_server_paths() -> Vec<(String, Vec<u8>)> {
    let refs: Value = serde_json::from_slice(&fixture("wikidata_entity_refs.json")).unwrap();
    let mut v = vec![(
        "/wiki/Special:EntityData/Q64.json".to_string(),
        fixture("wikidata_entity_q64.json"),
    )];
    for (q, body) in refs.as_object().unwrap() {
        v.push((
            format!("/wiki/Special:EntityData/{q}.json"),
            serde_json::to_vec(body).unwrap(),
        ));
    }
    v
}

#[tokio::test]
async fn entity_data_row_spools_raw_subresources_then_one_bundle() {
    let s = MockServer::start().await;
    for (p, body) in entity_data_server_paths() {
        Mock::given(method("GET"))
            .and(path(p.as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_bytes(body),
            )
            .expect(1)
            .mount(&s)
            .await;
    }
    let mut h = Harness::new(
        common::spec("wikidata-dump"),
        "wikidata_facts",
        &format!("{}/wiki/Special:EntityData/Q64.json", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&WikidataFacts::DEFAULT).await.unwrap();
    assert!(rep.complete);
    assert_eq!(rep.requests, 9, "the item and its 8 referenced items");
    assert_raw_first(&h.events.all());
    let roles: Vec<vc_core::store::CaptureRole> =
        h.sink.records.iter().map(|r| r.meta.role).collect();
    assert_eq!(
        roles
            .iter()
            .filter(|r| **r == vc_core::store::CaptureRole::Subresource)
            .count(),
        9
    );
    assert_eq!(roles.last(), Some(&vc_core::store::CaptureRole::Primary));
    let res = h.resources();
    assert_eq!(res.len(), 1);
    let it = records(
        "wikidata_facts",
        &CanonicalUrl::parse(Q64).unwrap(),
        &res[0].1,
    )
    .unwrap()
    .unwrap();
    fit(&it);
    assert!(
        it.iter()
            .any(|i| text(i) == "Berlin is twinned with Los Angeles and Paris.")
    );
}

#[tokio::test]
async fn a_non_place_item_makes_no_bundle_and_a_challenge_stops() {
    let s = MockServer::start().await;
    let refs: Value = serde_json::from_slice(&fixture("wikidata_entity_refs.json")).unwrap();
    Mock::given(method("GET"))
        .and(path("/wiki/Special:EntityData/Q8150.json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(serde_json::to_vec(&refs["Q8150"]).unwrap()),
        )
        .mount(&s)
        .await;
    let mut h = Harness::new(
        common::spec("wikidata-dump"),
        "wikidata_facts",
        &format!("{}/wiki/Special:EntityData/Q8150.json", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&WikidataFacts::DEFAULT).await.unwrap();
    assert_eq!(rep.requests, 1);
    assert!(h.resources().is_empty());

    let s2 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&s2)
        .await;
    let mut h2 = Harness::new(
        common::spec("wikidata-dump"),
        "wikidata_facts",
        &format!("{}/wiki/Special:EntityData/Q64.json", s2.uri()),
        t0(),
    )
    .await;
    let err = h2.run(&WikidataFacts::DEFAULT).await.unwrap_err();
    assert!(matches!(err, Error::Fetch(_)), "{err:?}");
    assert_eq!(
        s2.received_requests().await.unwrap().len(),
        1,
        "never retried"
    );
}

#[tokio::test]
async fn the_dump_route_slims_place_lines_only() {
    let refs: Value = serde_json::from_slice(&fixture("wikidata_entity_refs.json")).unwrap();
    let q64: Value = serde_json::from_slice(&fixture("wikidata_entity_q64.json")).unwrap();
    let lines = format!(
        "[\n{},\n{}\n]\n",
        serde_json::to_string(&q64["entities"]["Q64"]).unwrap(),
        serde_json::to_string(&refs["Q8150"]["entities"]["Q8150"]).unwrap()
    );
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(lines.as_bytes()).unwrap();
    let body = gz.finish().unwrap();
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wikidatawiki/entities/latest-all.json.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        common::spec("wikidata-dump"),
        "wikidata_facts",
        &format!("{}/wikidatawiki/entities/latest-all.json.gz", s.uri()),
        t0(),
    )
    .await;
    h.run(&WikidataFacts::DEFAULT).await.unwrap();
    let res = h.resources();
    assert_eq!(res.len(), 1);
    let batch: Vec<Value> = serde_json::from_slice(&res[0].1).unwrap();
    assert_eq!(batch.len(), 1, "the Olympics are not a place");
    assert_eq!(batch[0]["id"], "Q64");
    assert_eq!(
        h.manifests()[0]["filter_version"],
        wikidata_facts::FILTER_VERSION
    );
}

#[tokio::test]
async fn an_unknown_row_makes_no_request() {
    let s = MockServer::start().await;
    let mut h = Harness::new(
        common::spec("wikidata-dump"),
        "wikidata_facts",
        &format!("{}/wiki/Q64", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(
        h.run(&WikidataFacts::DEFAULT).await,
        Err(Error::Invalid(_))
    ));
    assert!(s.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// wiki_hooks

#[test]
fn every_listed_wiki_and_page_is_unique() {
    let mut sites: Vec<&str> = wiki_hooks::SITES.iter().map(|s| s.site).collect();
    assert_eq!(sites.len(), 37);
    sites.sort_unstable();
    sites.dedup();
    assert_eq!(sites.len(), 37);
    let pages: usize = wiki_hooks::SITES.iter().map(|s| s.pages.len()).sum();
    assert_eq!(pages, 93);
    for s in wiki_hooks::SITES {
        assert!(s.host.ends_with(".wikipedia.org"), "{}", s.host);
        assert_eq!(wiki_hooks::site_by_host(s.host).unwrap().site, s.site);
        for (p, k) in s.pages {
            assert_eq!(wiki_hooks::page_kind(s, p), Some(*k), "{p}");
            assert!(!p.contains(['?', '#']) || p.ends_with('?'), "{p}");
        }
    }
    let de = wiki_hooks::site_by_host("de.wikipedia.org").unwrap();
    assert_eq!(
        wiki_hooks::page_kind(de, "Wikipedia:Hauptseite/Jahrestage/November/20"),
        Some(HookPage::Otd),
        "an archive subpage"
    );
    assert_eq!(wiki_hooks::page_kind(de, "Berlin"), None);
    assert_eq!(
        wiki_hooks::page_kind(de, "Wikipedia:Kuriosit%C3%A4tenkabinett"),
        Some(HookPage::Unusual),
        "percent-encoded titles"
    );
}

fn hook_items(url: &str, file: &str) -> Vec<AdapterItem> {
    records(
        "wiki_hooks",
        &CanonicalUrl::parse(url).unwrap(),
        &fixture(file),
    )
    .unwrap()
    .unwrap()
}

#[test]
fn dyk_hooks_keep_short_share_alike_text_and_their_link_candidates() {
    let it = hook_items(
        "https://simple.wikipedia.org/wiki/Wikipedia:Did_you_know?action=raw",
        "wiki_hooks_simple_dyk.txt",
    );
    fit(&it);
    let texts: Vec<&str> = it.iter().map(text).collect();
    assert_eq!(
        texts,
        vec![
            "... that Berlin has more bridges than Venice?",
            "... that the Arkenberge, the highest point of the German capital, is a former rubble hill?",
            "... that the village of Hallstatt was copied in China?",
            "... that Lake Bled has an island with a church?",
        ],
        "refs, edge templates and files removed; inner templates, unlinked and short hooks and duplicates dropped"
    );
    for i in &it {
        assert_eq!(i.data["text_origin"], "source_hook");
        assert_eq!(
            i.data["llm_ok"], false,
            "share-alike text never goes to a model"
        );
        assert_eq!(i.data["licence"]["class"], "share_alike");
        assert_eq!(i.data["licence"]["spdx"], "CC-BY-SA-4.0");
        assert_eq!(i.data["fact_type"], "fun");
        assert_eq!(i.data["lang"], "en", "Simple English");
        assert_eq!(i.data["confidence"], "extracted");
        assert_eq!(
            i.data["source_url"],
            "https://simple.wikipedia.org/wiki/Wikipedia:Did_you_know"
        );
        assert!(
            i.data["place"].get("wikidata_qid").is_none(),
            "resolved by the read API"
        );
    }
    assert_eq!(
        it[1].data["place"]["link_candidates"],
        json!(["simplewiki:Arkenberge", "simplewiki:Berlin"]),
        "bold links first, then the others by their target"
    );
    assert_eq!(
        it[2].data["place"]["link_candidates"],
        json!(["simplewiki:Hallstatt (village)", "simplewiki:China"])
    );
}

#[test]
fn otd_lines_give_the_year_and_unusual_tables_their_rows() {
    let otd = hook_items(
        "https://de.wikipedia.org/wiki/Wikipedia:Hauptseite/Jahrestage?action=raw",
        "wiki_hooks_de_otd.txt",
    );
    fit(&otd);
    assert_eq!(otd.len(), 2);
    assert_eq!(otd[0].data["date_ref"], "1244");
    assert_eq!(otd[0].data["fact_type"], "historical");
    assert_eq!(
        text(&otd[0]),
        "1244 – Erste urkundliche Erwähnung der Stadt Berlin in einer Urkunde."
    );
    assert_eq!(
        otd[0].data["place"]["link_candidates"],
        json!(["dewiki:Berlin"]),
        "year articles are not places"
    );
    assert_eq!(otd[1].data["date_ref"], "1806");
    assert_eq!(otd[0].data["lang"], "de");

    let unusual = hook_items(
        "https://fr.wikipedia.org/wiki/Wikip%C3%A9dia:Articles_insolites?action=raw",
        "wiki_hooks_fr_unusual.txt",
    );
    fit(&unusual);
    assert_eq!(unusual.len(), 2);
    assert_eq!(unusual[0].data["fact_type"], "unbelievable");
    assert!(text(&unusual[0]).starts_with("Baarle-Hertog \u{2014} Une commune"));
    assert_eq!(
        unusual[0].data["place"]["link_candidates"],
        json!(["frwiki:Baarle-Hertog"])
    );
}

#[test]
fn a_page_that_is_not_listed_is_refused() {
    let r = records(
        "wiki_hooks",
        &CanonicalUrl::parse("https://de.wikipedia.org/wiki/Berlin?action=raw").unwrap(),
        b"* [[Berlin]] is a city with a long enough line.",
    )
    .unwrap();
    assert!(matches!(r, Err(Error::Invalid(_))));
}

#[tokio::test]
async fn a_hook_row_fetches_the_raw_wikitext_once() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/wiki/Wikipedia:Did_you_know"))
        .and(query_param("action", "raw"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/x-wiki; charset=UTF-8")
                .set_body_bytes(fixture("wiki_hooks_simple_dyk.txt")),
        )
        .expect(1)
        .mount(&s)
        .await;
    let mut spec = common::spec("wikidata-dump");
    spec.id = SourceId("wiki-facts-test".into());
    let mut h = Harness::new(
        spec,
        "wiki_hooks",
        &format!("{}/wiki/Wikipedia:Did_you_know", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&WikiHooks).await.unwrap();
    assert_eq!(rep.requests, 1);
    assert!(rep.complete);
    assert_raw_first(&h.events.all());
    assert_eq!(h.responses().len(), 1);
    assert!(h.responses()[0].url.as_str().ends_with("?action=raw"));

    let mut h2 = Harness::new(
        common::spec("wikidata-dump"),
        "wiki_hooks",
        &format!("{}/wiki/Berlin", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(h2.run(&WikiHooks).await, Err(Error::Invalid(_))));
}

// ---------------------------------------------------------------------------------------------
// factbook_json

#[test]
fn factbook_country_file_facts() {
    let it = records(
        "factbook_json",
        &CanonicalUrl::parse(FACTBOOK).unwrap(),
        &fixture("factbook_gm.json"),
    )
    .unwrap()
    .unwrap();
    fit(&it);
    for i in &it {
        assert_eq!(i.data["place"]["country"], "DE");
        assert_eq!(i.data["place"]["admin_level"], "country");
        assert_eq!(i.data["place"]["native_id"], "factbook:gm");
        assert_eq!(i.data["as_of"], factbook_json::EDITION);
        assert_eq!(i.data["licence"]["class"], "open");
        assert_eq!(i.data["confidence"], "structured");
    }
    let one = |f: &str| {
        let v = by_field(&it, f);
        assert_eq!(v.len(), 1, "{f}");
        v[0]
    };
    let high = one("factbook:Geography/Elevation/highest point");
    assert_eq!(
        text(high),
        "The highest point of Germany is Zugspitze (2,963 m)."
    );
    assert_eq!(high.data["quantity"]["value"], 2963.0);
    assert_eq!(high.data["fact_type"], "unbelievable");
    assert_eq!(
        text(one("factbook:Geography/Elevation/lowest point")),
        "The lowest point of Germany is Neuendorf bei Wilster (-3.5 m)."
    );
    assert_eq!(
        text(one("factbook:Geography/Area/total")),
        "Germany covers a total area of 357,022 km\u{b2}."
    );
    assert_eq!(
        text(one(
            "factbook:Government/National heritage/total World Heritage Sites"
        )),
        "Germany has 56 UNESCO World Heritage Sites."
    );
    assert_eq!(
        text(one("factbook:Government/National symbol(s)")),
        "National symbols of Germany: eagle."
    );
    assert_eq!(
        text(one("factbook:Government/Country name/former")),
        "Former names of Germany: German Reich."
    );
    let ind = one("factbook:Government/Independence");
    assert_eq!(
        text(ind),
        "Germany dates its independence to 18 January 1871."
    );
    assert_eq!(ind.data["date_ref"], "1871-01-18");
}

#[test]
fn a_factbook_file_without_a_resolvable_country_yields_nothing() {
    let body = json!({"Government": {"Country name": {"conventional short form": {"text": "none"}}},
        "Geography": {"Elevation": {"highest point": {"text": "Mount X 100 m"}}}});
    let it = records(
        "factbook_json",
        &CanonicalUrl::parse(
            "https://raw.githubusercontent.com/factbook/factbook.json/master/oceans/xo.json",
        )
        .unwrap(),
        &serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
    .unwrap();
    assert!(it.is_empty());
    assert!(
        factbook_json::file_code(&CanonicalUrl::parse("https://example.org/gm.json").unwrap())
            .is_none()
    );
}

#[tokio::test]
async fn a_factbook_row_fetches_its_file() {
    let s = MockServer::start().await;
    let p = "/factbook/factbook.json/master/europe/gm.json";
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(fixture("factbook_gm.json")))
        .expect(1)
        .mount(&s)
        .await;
    let mut h = Harness::new(
        common::spec("wikidata-dump"),
        "factbook_json",
        &format!("{}{p}", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&FactbookJson).await.unwrap();
    assert_eq!(rep.requests, 1);
    assert_raw_first(&h.events.all());
    let mut h2 = Harness::new(
        common::spec("wikidata-dump"),
        "factbook_json",
        &format!("{}/README.md", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(
        h2.run(&FactbookJson).await,
        Err(Error::Invalid(_))
    ));
}
