//! Place facts (2026-11-20): `wikidata_facts` (Wikidata EntityData and dump lines) and
//! `factbook_json` (CIA World Factbook final edition via factbook.json), against small
//! fixtures (`testkit/fixtures/vc-sources/README.md`). Every item must fit `place_fact.v1`.

mod common;

use std::io::Write;

use common::{Harness, assert_raw_first, fixture, t0};
use serde_json::{Map, Value, json};
use vc_core::{CanonicalUrl, Error};
use vc_sources::adapters;
use vc_sources::adapters::factbook_json::{self, FactbookJson};
use vc_sources::adapters::records::{AdapterItem, MAPPED, records};
use vc_sources::adapters::wikidata_facts::{self, WikidataFacts};
use wiremock::matchers::{method, path};
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
fn the_place_fact_adapters_are_registered_and_mapped() {
    for id in ["wikidata_facts", "factbook_json"] {
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
fn slim_keeps_the_fact_properties_and_english_labels_only() {
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
    // English sentences: only English and language-independent (`mul`) labels are kept,
    // and no sitelinks.
    assert!(
        e["labels"]
            .as_object()
            .unwrap()
            .keys()
            .all(|k| k == "en" || k == "mul")
    );
    assert!(e.get("sitelinks").is_none());
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
    assert_eq!(
        nick.data["value_lang"], "de",
        "the quoted value's own language"
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
    let motto = it
        .iter()
        .find(|(_, d)| d["source_field"] == "wikidata:P1451")
        .unwrap();
    assert_eq!(motto.1["value_lang"], "la");
    // A deprecated statement is dropped when slimming.
    let raw = json!({"id": "Q2", "claims": {"P1566": [{"rank": "normal", "mainsnak": {"snaktype": "value", "datavalue": {"value": "9"}}}],
        "P571": [{"rank": "deprecated", "mainsnak": {"snaktype": "value", "datavalue": {"value": {"time": "+1900-00-00T00:00:00Z", "precision": 9}}}}]},
        "labels": {"en": {"value": "X Town"}}});
    let s = wikidata_facts::slim(&raw).unwrap();
    assert!(s["claims"].get("P571").is_none());
}

#[test]
fn english_sentences_never_carry_a_label_in_another_language() {
    // No English (or `mul`) label: no English sentence about the place at all.
    let e = json!({"id": "Q3", "labels": {"ar": "\u{0628}\u{0644}\u{062f}\u{0629}"},
    "claims": {
        "P1566": [{"rank": "normal", "value": "456", "q": {}, "refs": false}],
        "P571": [{"rank": "normal", "value": {"time": "+1900-00-00T00:00:00Z", "precision": 9}, "q": {}, "refs": false}]
    }});
    assert!(wikidata_facts::facts(&e, &Map::new()).is_empty());
    // A `mul` label is a name in every language: used, and recorded as such.
    let mut m = e.clone();
    m["labels"] = json!({"mul": "Qtown", "ar": "x"});
    let it = wikidata_facts::facts(&m, &Map::new());
    assert_eq!(it[0].1["text"], "Qtown was founded in 1900.");
    assert_eq!(it[0].1["place"]["name_lang"], "mul");
    // A referenced item (a twin town) with no English label is left out of the sentence.
    let mut t = m.clone();
    t["claims"]["P190"] = json!([
        {"rank": "normal", "value": {"id": "Q10"}, "q": {}, "refs": false},
        {"rank": "normal", "value": {"id": "Q11"}, "q": {}, "refs": false}
    ]);
    let mut labels = Map::new();
    labels.insert("Q10".into(), json!({"labels": {"ar": "y"}}));
    labels.insert("Q11".into(), json!({"labels": {"en": "Elsewhere"}}));
    let it = wikidata_facts::facts(&t, &labels);
    let twin = it
        .iter()
        .find(|(_, d)| d["source_field"] == "wikidata:P190")
        .unwrap();
    assert_eq!(
        twin.1["text"],
        "Qtown is twinned with Elsewhere and 1 other place."
    );
    // English values first among several nicknames.
    let mut n = m.clone();
    n["claims"]["P1449"] = json!([
        {"rank": "normal", "value": {"text": "Q-Stadt", "language": "de"}, "q": {}, "refs": false},
        {"rank": "normal", "value": {"text": "Q City", "language": "en"}, "q": {}, "refs": false}
    ]);
    let it = wikidata_facts::facts(&n, &Map::new());
    let nicks: Vec<&Value> = it
        .iter()
        .filter(|(_, d)| d["source_field"] == "wikidata:P1449")
        .map(|(_, d)| d)
        .collect();
    assert_eq!(nicks[0]["value_lang"], "en");
    assert_eq!(nicks[1]["value_lang"], "de");
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
    // The bundle carries the item's status: it is one response, not a dump batch, so the
    // processor reconciles the row's facts against it (a removed statement goes stale).
    assert_eq!(h.sink.records.last().unwrap().meta.status, Some(200));
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
async fn a_non_place_item_makes_an_empty_bundle_and_a_challenge_stops() {
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
    // An empty bundle with the item's status: the facts the row had (when it was a place)
    // go stale.
    let res = h.resources();
    assert_eq!(res.len(), 1);
    assert_eq!(h.sink.records.last().unwrap().meta.status, Some(200));
    let it = records(
        "wikidata_facts",
        &CanonicalUrl::parse("https://www.wikidata.org/wiki/Special:EntityData/Q8150.json")
            .unwrap(),
        &res[0].1,
    )
    .unwrap()
    .unwrap();
    assert!(it.is_empty());

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
        "Germany has eagle as its national symbol."
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

#[test]
fn factbook_symbols_become_our_own_sentence() {
    let s = |f: &str| factbook_json::symbols_sentence("Testland", f);
    assert_eq!(
        s("bald eagle, rose; national colors: red, white, blue").as_deref(),
        Some(
            "Testland has bald eagle and rose as its national symbols, and its national \
             colours are red, white and blue."
        )
    );
    assert_eq!(
        s("national colors: green, white").as_deref(),
        Some("The national colours of Testland are green and white.")
    );
    // Prose or parenthesised notes are not a list of names: no fact.
    assert_eq!(s("eagle (a symbol used since the 12th century)"), None);
    assert_eq!(s(""), None);
}
