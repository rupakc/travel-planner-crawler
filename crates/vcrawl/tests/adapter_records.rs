//! Go-live regression (2026-11-17): the ramp-1 sources (`sources/ramp-1.txt`) through the real
//! processor into the index. Adapter captures are committed exactly as the fetcher stores them
//! (an API `response`, or a filtered dump `resource` batch with no status; media type fixed at
//! fetch time) with the **recorded** adapter fixtures of `testkit/fixtures/vc-sources/`, and the
//! sources are the repository's own `sources/<id>.yaml`. Before the adapter record mappings,
//! every one of these produced zero records (ECB: `no_handler`; the JSON APIs: parsed, but no
//! schema.org types to map, and the LLM is off).

#[path = "processor_support.rs"]
mod support;

use std::io::Read as _;

use support::*;
use vc_core::media::{FetchMode, MediaType};
use vcrawl::data::processor::Outcome;

fn recorded(name: &str) -> Vec<u8> {
    let p = root().join("testkit/fixtures/vc-sources").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
}

fn text(name: &str) -> String {
    String::from_utf8(recorded(name)).expect("utf-8 fixture")
}

/// Seeds the repository's real source specs.
async fn seed_real(pool: &sqlx::PgPool, ids: &[&str]) {
    let specs: Vec<_> = ids
        .iter()
        .map(|id| {
            let p = root().join("sources").join(format!("{id}.yaml"));
            let y = std::fs::read_to_string(&p).expect("source spec");
            vc_sources::spec::parse_spec(&y).expect("spec")
        })
        .collect();
    vc_sources::seed::seed_sources(pool, &specs, false)
        .await
        .expect("seed sources");
}

/// The ramp sources, as `sources/ramp-1.txt` lists them.
fn ramp() -> Vec<String> {
    std::fs::read_to_string(root().join("sources/ramp-1.txt"))
        .expect("ramp-1.txt")
        .lines()
        .filter_map(|l| l.split('#').next())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

async fn rig_real(frontier: bool) -> Rig {
    let r = rig(
        &[],
        RigOpts {
            frontier,
            real_places: true,
            ..RigOpts::default()
        },
    )
    .await;
    let ids = ramp();
    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
    seed_real(&r.pool, &ids).await;
    r
}

/// An API adapter capture: a 200 `response`.
fn api<'a>(
    url: &'a str,
    source: &'a str,
    adapter: &str,
    media: MediaType,
    body: &'a str,
) -> Cap<'a> {
    Cap {
        media,
        fetch_mode: FetchMode::Adapter(adapter.into()),
        response: true,
        ..Cap::html(url, source, body, at(0))
    }
}

/// A dump batch: a status-less `resource`.
fn dump<'a>(
    url: &'a str,
    source: &'a str,
    adapter: &str,
    media: MediaType,
    body: &'a str,
) -> Cap<'a> {
    Cap {
        media,
        status: None,
        fetch_mode: FetchMode::Adapter(adapter.into()),
        ..Cap::html(url, source, body, at(0))
    }
}

async fn run(r: &Rig, c: Cap<'_>) -> Outcome {
    let cref = commit(&r.pool, &r.blobs, c).await;
    r.proc.process(&cref).await.expect("process")
}

fn assert_processed(o: &Outcome) {
    assert!(
        matches!(o, Outcome::Processed { .. }),
        "expected processed, got {o:?}"
    );
}

async fn records_of(r: &Rig, kind: &str, source: &str) -> Vec<serde_json::Value> {
    sqlx::query_scalar(
        "SELECT v.data FROM records r JOIN record_versions v
           ON v.record_id = r.record_id AND v.version = r.current_version
         WHERE r.kind = $1 AND r.source_id = $2 ORDER BY r.record_key",
    )
    .bind(kind)
    .bind(source)
    .fetch_all(&r.pool)
    .await
    .expect("records")
}

#[test]
fn ramp_lists_the_eight_sources() {
    let ids = ramp();
    assert_eq!(ids.len(), 8, "{ids:?}");
}

#[tokio::test]
async fn ecb_fx_xml_yields_fx_rate_observations() {
    let r = rig_real(false).await;
    let body = text("ecb_eurofxref_daily.xml");
    let o = run(
        &r,
        api(
            "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml",
            "ecb-eurofxref",
            "ecb_fx",
            MediaType::Xml,
            &body,
        ),
    )
    .await;
    assert_processed(&o);
    let n = count(
        &r.pool,
        "SELECT count(*) FROM observations WHERE series_key LIKE 'fx:EUR:%'",
    )
    .await;
    assert!(n >= 20, "one observation per ECB currency, got {n}");
    let usd: serde_json::Value =
        sqlx::query_scalar("SELECT value FROM observations WHERE series_key = 'fx:EUR:USD'")
            .fetch_one(&r.pool)
            .await
            .expect("EUR/USD observation");
    assert_eq!(usd["base_currency"], "EUR");
    assert_eq!(usd["quote_currency"], "USD");
    assert!(usd["rate"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn travelpayouts_grouped_prices_yield_fare_observations() {
    let r = rig_real(false).await;
    seed_real(&r.pool, &["travelpayouts"]).await;
    let body = text("travelpayouts_grouped_prices_month.json");
    let o = run(
        &r,
        api(
            "https://api.travelpayouts.com/aviasales/v3/grouped_prices?currency=usd&destination=LON&group_by=month&origin=NYC",
            "travelpayouts",
            "travelpayouts_fares",
            MediaType::Json,
            &body,
        ),
    )
    .await;
    assert_processed(&o);
    let n = count(
        &r.pool,
        "SELECT count(*) FROM observations WHERE series_key LIKE 'fare:NYC:LON:%'",
    )
    .await;
    assert_eq!(n, 2, "one observation per departure month");
    let jan: serde_json::Value = sqlx::query_scalar(
        "SELECT value FROM observations WHERE series_key = 'fare:NYC:LON:2027-01'",
    )
    .fetch_one(&r.pool)
    .await
    .expect("NYC-LON January observation");
    assert_eq!(jan["price"].as_f64(), Some(389.0));
    assert_eq!(jan["currency"], "USD");
    assert_eq!(jan["trip_type"], "round_trip");
}

#[tokio::test]
async fn canada_advisories_json_yields_advisories() {
    let r = rig_real(false).await;
    let body = text("canada_index.json");
    let o = run(
        &r,
        api(
            "https://data.international.gc.ca/travel-voyage/index-alpha-eng.json",
            "canada-travel-json",
            "canada_advisories",
            MediaType::Json,
            &body,
        ),
    )
    .await;
    assert_processed(&o);
    let recs = records_of(&r, "advisory", "canada-travel-json").await;
    assert_eq!(recs.len(), 3, "one per country in the fixture: {recs:?}");
    let fr = recs
        .iter()
        .find(|d| d["country_code"] == "FR")
        .expect("France");
    assert_eq!(fr["country"], "France");
    assert_eq!(fr["issuer"], "Global Affairs Canada");
    assert_eq!(fr["level_label"], "Exercise a high degree of caution");
    assert_eq!(fr["level"], 1);
}

#[tokio::test]
async fn state_dept_api_yields_advisories() {
    let r = rig_real(false).await;
    let body = text("state_dept_advisories.json");
    let o = run(
        &r,
        api(
            "https://cadataapi.state.gov/api/TravelAdvisories",
            "state-dept-advisories-api",
            "state_dept_advisories",
            MediaType::Json,
            &body,
        ),
    )
    .await;
    assert_processed(&o);
    let recs = records_of(&r, "advisory", "state-dept-advisories-api").await;
    assert_eq!(recs.len(), 2, "{recs:?}");
    let ir = recs
        .iter()
        .find(|d| d["country_code"] == "IR")
        .expect("Iran");
    assert_eq!(ir["level"], 4);
    assert_eq!(ir["level_label"], "Do Not Travel");
    assert_eq!(ir["country"], "Iran");
    assert_eq!(ir["updated_date"], "2026-10-22");
}

#[tokio::test]
async fn fcdo_country_page_yields_an_advisory_and_the_index_none() {
    let r = rig_real(false).await;
    let page = text("fcdo_france.json");
    let o = run(
        &r,
        api(
            "https://www.gov.uk/api/content/foreign-travel-advice/france",
            "fcdo-content-api",
            "fcdo_content_api",
            MediaType::Json,
            &page,
        ),
    )
    .await;
    assert_processed(&o);
    let index = text("fcdo_index.json");
    let o = run(
        &r,
        Cap {
            at: at(1),
            ..api(
                "https://www.gov.uk/api/content/foreign-travel-advice",
                "fcdo-content-api",
                "fcdo_content_api",
                MediaType::Json,
                &index,
            )
        },
    )
    .await;
    assert_processed(&o);
    let recs = records_of(&r, "advisory", "fcdo-content-api").await;
    assert_eq!(recs.len(), 1, "{recs:?}");
    assert_eq!(recs[0]["country"], "France");
    assert_eq!(recs[0]["issuer_country_code"], "GB");
}

#[tokio::test]
async fn ourairports_dump_batch_yields_airports() {
    let r = rig_real(false).await;
    // The recorded first 8 KiB hold only small US fields (left out: no IATA code, no scheduled
    // service); two large airports in the file's own format are appended.
    let body = format!(
        "{}{}",
        text("ourairports_airports.csv"),
        "3622,\"KJFK\",\"large_airport\",\"John F Kennedy International Airport\",40.639447,-73.779317,13,\"NA\",\"US\",\"US-NY\",\"New York\",\"yes\",\"KJFK\",\"JFK\",\"KJFK\",\"JFK\",\"https://www.jfkairport.com/\",\"https://en.wikipedia.org/wiki/John_F._Kennedy_International_Airport\",\"Manhattan, New York City, NYC, Idlewild\"\n\
         2434,\"EGLL\",\"large_airport\",\"London Heathrow Airport\",51.4706,-0.461941,83,\"EU\",\"GB\",\"GB-ENG\",\"London\",\"yes\",\"EGLL\",\"LHR\",\"EGLL\",,\"https://www.heathrow.com/\",\"https://en.wikipedia.org/wiki/Heathrow_Airport\",\"LON, Londres\"\n"
    );
    let o = run(
        &r,
        dump(
            "https://davidmegginson.github.io/ourairports-data/airports.csv",
            "ourairports",
            "ourairports_csv",
            MediaType::Csv,
            &body,
        ),
    )
    .await;
    assert_processed(&o);
    let recs = records_of(&r, "airport", "ourairports").await;
    assert_eq!(recs.len(), 2, "large airports only: {recs:?}");
    for a in &recs {
        assert!(a["name"].is_string() && a["city"].is_string(), "{a}");
        assert!(
            a["country"].is_string(),
            "country filled from the code: {a}"
        );
    }
    let jfk = recs.iter().find(|a| a["iata"] == "JFK").expect("JFK");
    assert_eq!(jfk["city"], "New York");
    assert_eq!(jfk["country_code"], "US");
    assert_eq!(jfk["icao"], "KJFK");
    let lhr = recs.iter().find(|a| a["iata"] == "LHR").expect("LHR");
    assert_eq!(lhr["country_code"], "GB");
    assert!(lhr["country"].is_string());
}

/// A `gtfs_static` batch (as the adapter stores it) becomes `transit_item` records under the
/// feed's own licence (CC BY 4.0, Auckland Transport).
#[tokio::test]
async fn gtfs_static_batch_yields_transit_items_with_the_feed_licence() {
    let r = rig_real(false).await;
    seed_real(&r.pool, &["auckland-gtfs"]).await;
    let body = r#"[
      {"t":"operator","id":"AT","name":"Auckland Transport","url":"https://at.govt.nz/","fare_url":"https://at.govt.nz/fares/","timezone":"Pacific/Auckland","lang":"en","modes":["bus","ferry","rail"],"route_count":3,
       "fares":[{"label":"adult_1zone","amount":2.4,"currency":"NZD","unlimited_transfers":true,"transfer_duration_s":3600}],
       "valid_from":"2026-11-10","valid_to":"2027-02-01","publisher":"Auckland Transport"},
      {"t":"line","id":"EAST","short_name":"EAST","long_name":"Eastern Line","route_type":2,"mode":"rail","operator":"Auckland Transport","timezone":"Pacific/Auckland"},
      {"t":"station","id":"BRIT","name":"Britomart Train Station","lat":-36.8443,"lng":174.7676,"wheelchair":true,"timezone":"Pacific/Auckland"}
    ]"#;
    let o = run(
        &r,
        dump(
            "https://gtfs.at.govt.nz/gtfs.zip",
            "auckland-gtfs",
            "gtfs_static",
            MediaType::Json,
            body,
        ),
    )
    .await;
    assert_processed(&o);
    let recs = records_of(&r, "transit_item", "auckland-gtfs").await;
    assert_eq!(recs.len(), 3, "{recs:?}");
    let op = recs
        .iter()
        .find(|x| x["item_type"] == "operator")
        .expect("operator");
    assert_eq!(op["country_code"], "NZ");
    assert!(op["country"].is_string(), "country filled from the code");
    assert_eq!(op["fares"][0]["price"]["currency"], "NZD");
    assert_eq!(op["source_url"], "https://gtfs.at.govt.nz/gtfs.zip");
    let classes: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT licence_class FROM records WHERE source_id = 'auckland-gtfs'",
    )
    .fetch_all(&r.pool)
    .await
    .unwrap();
    assert_eq!(classes, ["open"]);
}

#[tokio::test]
async fn wikivoyage_dump_batch_yields_places() {
    let r = rig_real(false).await;
    // The batch exactly as the dump filter writes it: a JSON array of filtered pages.
    let mut xml = Vec::new();
    bzip2::read::MultiBzDecoder::new(&recorded("wikivoyage_pages_articles.xml.bz2")[..])
        .read_to_end(&mut xml)
        .expect("bunzip2");
    let xml = String::from_utf8(xml).expect("utf-8");
    let mut pages = Vec::new();
    for chunk in xml.split("<page>").skip(1) {
        let page = format!(
            "<page>{}",
            chunk.split("</page>").next().unwrap_or_default()
        );
        if let Some(v) =
            vc_sources::adapters::wikivoyage_dump::filter_page(format!("{page}</page>").as_bytes())
                .expect("filter")
        {
            pages.push(v);
        }
    }
    assert!(pages.len() >= 3, "articles + redirect kept");
    let body = serde_json::to_string(&pages).unwrap();
    let o = run(
        &r,
        dump(
            "https://dumps.wikimedia.org/enwikivoyage/latest/enwikivoyage-latest-pages-articles.xml.bz2",
            "wikivoyage-dump",
            "wikivoyage_dump",
            MediaType::Json,
            &body,
        ),
    )
    .await;
    assert_processed(&o);
    let recs = records_of(&r, "place", "wikivoyage-dump").await;
    let abakan = recs
        .iter()
        .find(|d| d["name"] == "Abakan")
        .unwrap_or_else(|| panic!("Abakan place: {recs:?}"));
    assert_eq!(abakan["country_code"], "RU");
    assert_eq!(abakan["place_type"], "city");
    assert!(
        abakan["summary"]
            .as_str()
            .is_some_and(|s| s.contains("Abakan"))
    );
}

#[tokio::test]
async fn feeds_and_pages_enqueue_their_links() {
    let r = rig_real(true).await;
    // FCDO Atom change feed (recorded).
    let atom = text("fcdo_changes.atom");
    let o = run(
        &r,
        Cap {
            media: MediaType::Atom,
            ..Cap::html(
                "https://www.gov.uk/foreign-travel-advice.atom",
                "fcdo-atom",
                &atom,
                at(0),
            )
        },
    )
    .await;
    let Outcome::Processed { links, .. } = o else {
        panic!("atom: {o:?}")
    };
    assert!(links > 0, "atom entries enqueued");

    // HTML index pages (LLM off: links only, no records).
    for (url, src) in [(
        "https://wwwnc.cdc.gov/travel/destinations/list",
        "cdc-travel",
    )] {
        let host = url.split('/').take(3).collect::<Vec<_>>().join("/");
        let html = format!(
            "<html><head><title>Index</title></head><body><main><h1>Destinations</h1>\
             <p>Pick a destination to read the official page for it before you travel.</p>\
             <a href=\"{host}/content/travel/en/international-travel/International-Travel-Country-Information-Pages/France.html\">France</a>\
             <a href=\"{host}/travel/destinations/traveler/none/france\">France</a>\
             </main></body></html>"
        );
        let o = run(&r, Cap::html(url, src, &html, at(2))).await;
        assert_processed(&o);
    }
    assert!(
        count(&r.pool, "SELECT count(*) FROM url_state WHERE depth >= 1").await >= 2,
        "feed items and page links reach the frontier"
    );
}

#[tokio::test]
async fn admin_reprocess_hands_done_captures_back() {
    use vc_core::queue::{WorkKind, WorkQueue};
    use vcrawl::admin::reprocess::{ReprocessArgs, reprocess};
    let r = rig_real(false).await;
    let body = text("ecb_eurofxref_daily.xml");
    commit(
        &r.pool,
        &r.blobs,
        api(
            "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml",
            "ecb-eurofxref",
            "ecb_fx",
            MediaType::Xml,
            &body,
        ),
    )
    .await;
    // Production before the fix: the item was taken and acked with nothing extracted.
    let q = vc_queue::PgWorkQueue {
        pool: r.pool.clone(),
        config: vc_config::QueueConfig::default(),
    };
    let items = q
        .take(
            WorkKind::CaptureReady,
            10,
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    q.ack(items[0].id).await.unwrap();
    assert_eq!(count(&r.pool, "SELECT count(*) FROM observations").await, 0);

    let args = |dry_run| ReprocessArgs {
        sources: vec!["ecb-eurofxref".into(), "ourairports".into()],
        dry_run,
    };
    let dry = reprocess(&r.pool, &args(true)).await.unwrap();
    assert_eq!((dry.eligible, dry.queued), (1, 0));
    let rep = reprocess(&r.pool, &args(false)).await.unwrap();
    assert_eq!((rep.eligible, rep.queued), (1, 1));
    // Queued once while it waits.
    assert_eq!(reprocess(&r.pool, &args(false)).await.unwrap().queued, 0);
    assert_eq!(r.proc.tick(8).await.unwrap(), 1);
    assert!(count(&r.pool, "SELECT count(*) FROM observations").await >= 20);
}

// ---------------------------------------------------------------------------------------------
// Embassy, medical and visa-reference sources (2026-11-18)
// ---------------------------------------------------------------------------------------------

/// `(adapter, URL, fixture)` of every recorded capture of the 2026-11-18 adapters.
const EMBASSY_CAPTURES: &[(&str, &str, &str)] = &[
    (
        "govuk_world_orgs",
        "https://www.gov.uk/api/content/world/organisations/british-embassy-tirana",
        "govuk_org_tirana.json",
    ),
    (
        "govuk_world_orgs",
        "https://www.gov.uk/api/content/world/organisations/british-embassy-madrid",
        "govuk_org_madrid.json",
    ),
    (
        "canada_offices",
        "https://data.international.gc.ca/travel-voyage/opendata-offices-list-alpha-eng.json",
        "canada_offices.json",
    ),
    (
        "govuk_medical_lists",
        "https://www.gov.uk/api/content/government/publications/albania-list-of-medical-facilities",
        "govuk_medical_albania.json",
    ),
    (
        "govuk_medical_lists",
        "https://www.gov.uk/api/content/government/publications/albania-list-of-medical-facilities/list-of-english-speaking-medical-facilities-for-british-nationals-in-albania--2",
        "govuk_medical_albania_list.json",
    ),
    (
        "enwiki_visa",
        "https://dumps.wikimedia.org/enwiki/latest/enwiki-latest-pages-articles-multistream.xml.bz2?length=3569&offset=1000",
        "enwiki_visa_stream.xml.bz2",
    ),
];

#[test]
fn embassy_medical_and_visa_records_fit_their_schemas() {
    let r = root();
    let reg = vc_kinds::FileKindRegistry::load(&vc_config::KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    })
    .expect("kind registry");
    let mut total = 0;
    for (adapter, url, file) in EMBASSY_CAPTURES {
        let u = vc_core::CanonicalUrl::parse(url).unwrap();
        let items = vc_sources::adapters::records::records(adapter, &u, &recorded(file))
            .expect("mapped")
            .expect("fixture maps");
        assert!(!items.is_empty(), "{file}: no records");
        for it in items {
            if let Err(e) = reg.validate(&vc_core::KindId::new(it.kind), &it.data) {
                panic!(
                    "{file}: {} does not fit {}: {e:?}\n{:#}",
                    it.kind, it.kind, it.data
                );
            }
            total += 1;
        }
    }
    assert!(total > 20, "{total}");
}

#[tokio::test]
async fn official_embassy_and_medical_lists_reach_the_index() {
    let r = rig_real(false).await;
    seed_real(
        &r.pool,
        &[
            "govuk-world-orgs",
            "canada-offices-json",
            "govuk-medical-lists",
        ],
    )
    .await;
    let caps = [
        ("govuk-world-orgs", 0),
        ("canada-offices-json", 2),
        ("govuk-medical-lists", 3),
        ("govuk-medical-lists", 4),
    ];
    for (source, i) in caps {
        let (adapter, url, file) = EMBASSY_CAPTURES[i];
        let body = text(file);
        let o = run(&r, api(url, source, adapter, MediaType::Json, &body)).await;
        assert_processed(&o);
    }
    let uk = records_of(&r, "embassy", "govuk-world-orgs").await;
    assert_eq!(uk.len(), 1, "{uk:?}");
    assert_eq!(uk[0]["name"], "British Embassy Tirana");
    assert_eq!(uk[0]["phones"][0]["e164"], "+35542247697");
    let ca = records_of(&r, "embassy", "canada-offices-json").await;
    assert!(ca.len() >= 6, "{}", ca.len());
    assert!(
        ca.iter()
            .any(|d| d["of_country_code"] == "GB" && d["country_code"] == "GM"),
        "partner offices are kept with their own sending country"
    );
    let med = records_of(&r, "medical_facility", "govuk-medical-lists").await;
    assert_eq!(med.len(), 6);
    assert!(med.iter().all(|d| d["list_updated"] == "2026-03-17"));
    let lists = records_of(&r, "article", "govuk-medical-lists").await;
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0]["updated_date"], "2026-03-17");
}

/// US country information and the food-hygiene registers (2026-11-18): (source, adapter, URL,
/// recorded fixture).
const COUNTRY_INFO_AND_FOOD: &[(&str, &str, &str, &str)] = &[
    (
        "state-dept-country-info",
        "state_dept_country_info",
        "https://cadataapi.state.gov/api/CountryTravelInformation",
        "state_dept_country_info.json",
    ),
    (
        "fsa-food-hygiene",
        "fsa_fhrs",
        "https://api.ratings.food.gov.uk/Establishments?businessTypeId=1&localAuthorityId=120&pageNumber=1&pageSize=1000",
        "fsa_establishments_p1.json",
    ),
    (
        "nyc-restaurant-inspections",
        "nyc_restaurant_inspections",
        "https://data.cityofnewyork.us/resource/43nn-pn8j.json?%24limit=2000&%24offset=0",
        "nyc_restaurant_inspections.json",
    ),
    (
        "toronto-dinesafe",
        "toronto_dinesafe",
        "https://ckan0.cf.opendata.inter.prod-toronto.ca/api/3/action/datastore_search?limit=2000&offset=0&resource_id=75dbb3ff-8281-4f03-9de2-bd1214761302",
        "toronto_dinesafe_page.json",
    ),
];

#[test]
fn country_info_and_restaurant_records_fit_their_schemas() {
    let r = root();
    let reg = vc_kinds::FileKindRegistry::load(&vc_config::KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    })
    .expect("kind registry");
    for (_, adapter, url, file) in COUNTRY_INFO_AND_FOOD {
        let u = vc_core::CanonicalUrl::parse(url).unwrap();
        let items = vc_sources::adapters::records::records(adapter, &u, &recorded(file))
            .expect("mapped")
            .expect("fixture maps");
        assert!(!items.is_empty(), "{file}: no records");
        for it in items {
            if let Err(e) = reg.validate(&vc_core::KindId::new(it.kind), &it.data) {
                panic!("{file}: {} does not fit: {e:?}\n{:#}", it.kind, it.data);
            }
        }
    }
}

async fn attributions(r: &Rig, kind: &str, source: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT DISTINCT v.licence_class, v.attribution FROM records r JOIN record_versions v
           ON v.record_id = r.record_id AND v.version = r.current_version
         WHERE r.kind = $1 AND r.source_id = $2",
    )
    .bind(kind)
    .bind(source)
    .fetch_all(&r.pool)
    .await
    .expect("attributions")
}

#[tokio::test]
async fn country_info_and_food_registers_reach_the_index_with_their_licence() {
    let r = rig_real(false).await;
    let ids: Vec<&str> = COUNTRY_INFO_AND_FOOD.iter().map(|c| c.0).collect();
    seed_real(&r.pool, &ids).await;
    for (source, adapter, url, file) in COUNTRY_INFO_AND_FOOD {
        let body = text(file);
        let o = run(&r, api(url, source, adapter, MediaType::Json, &body)).await;
        assert_processed(&o);
    }
    // Official US visa rules pass the high-stakes gate (official source only).
    let visa = records_of(&r, "visa_rule", "state-dept-country-info").await;
    assert_eq!(visa.len(), 4, "{visa:?}");
    assert!(
        visa.iter()
            .any(|d| d["destination_country_code"] == "JP" && d["visa_type"] == "visa_free")
    );
    let emb = records_of(&r, "embassy", "state-dept-country-info").await;
    assert_eq!(emb.len(), 4, "{emb:?}");
    assert!(emb.iter().all(|d| d["of_country_code"] == "US"));
    assert!(
        !records_of(&r, "tip", "state-dept-country-info")
            .await
            .is_empty()
    );
    // Restaurants: every one carries its rating date and the source's licence attribution.
    for (source, want) in [
        ("fsa-food-hygiene", 3),
        ("nyc-restaurant-inspections", 3),
        ("toronto-dinesafe", 2),
    ] {
        let rs = records_of(&r, "restaurant", source).await;
        assert_eq!(rs.len(), want, "{source}");
        assert!(
            rs.iter().all(|d| d["hygiene"]["rating_date"].is_string()),
            "{source}"
        );
        let a = attributions(&r, "restaurant", source).await;
        assert_eq!(a.len(), 1, "{source}");
        assert_eq!(a[0].0, "open", "{source}");
        assert!(!a[0].1.is_empty(), "{source}");
    }
    assert_eq!(
        attributions(&r, "restaurant", "fsa-food-hygiene").await[0].1,
        "Contains public sector information licensed under the Open Government Licence v3.0 — Food Standards Agency"
    );
}

/// The generic open-data readers (2026-11-18): (source, adapter, URL, recorded fixture, dump?).
const OPEN_DATA_READERS: &[(&str, &str, &str, &str, bool)] = &[
    (
        "od-chicago-food-inspections",
        "socrata",
        "https://data.cityofchicago.org/resource/4ijn-s7e5.json?%24limit=2000&%24offset=0",
        "socrata_chicago_food_inspections.json",
        false,
    ),
    (
        "od-austin-food-inspections",
        "socrata",
        "https://data.austintexas.gov/resource/ecmv-9xxi.json?%24limit=2000&%24offset=0",
        "socrata_austin_food_inspections.json",
        false,
    ),
    (
        "od-catalonia-tourism-register",
        "socrata",
        "https://analisi.transparenciacatalunya.cat/resource/t2h3-cgys.json?%24limit=2000&%24offset=0",
        "socrata_catalonia_tourism_register.json",
        false,
    ),
    (
        "od-nyc-public-restrooms",
        "socrata",
        "https://data.cityofnewyork.us/resource/i7jb-7jku.json?%24limit=2000&%24offset=0",
        "socrata_nyc_public_restrooms.json",
        false,
    ),
    (
        "od-fr-atout-france-classes",
        "udata_resource",
        "https://data.classement.atout-france.fr/static/exportHebergementsClasses/hebergements_classes.csv",
        "udata_atout_france_classes.csv",
        true,
    ),
    (
        "od-wien-ogd",
        "wfs_geojson",
        "https://data.wien.gv.at/daten/geo?service=WFS&request=GetFeature&version=1.1.0&typeName=ogdwien:WCANLAGEOGD&srsName=EPSG:4326&outputFormat=json",
        "wfs_wien_toilets.json",
        false,
    ),
    (
        "od-wien-ogd",
        "wfs_geojson",
        "https://data.wien.gv.at/daten/geo?service=WFS&request=GetFeature&version=1.1.0&typeName=ogdwien:SEHENSWUERDIGOGD&srsName=EPSG:4326&outputFormat=json",
        "wfs_wien_sights.json",
        false,
    ),
    (
        "od-zuerich-ogd",
        "wfs_geojson",
        "https://www.ogd.stadt-zuerich.ch/wfs/geoportal/Zueri_WC?service=WFS&version=1.1.0&request=GetFeature&typename=poi_zueriwc_rs_view&outputFormat=GeoJSON&srsName=EPSG:4326",
        "wfs_zuerich_wc_rs.json",
        false,
    ),
];

#[test]
fn open_data_reader_records_fit_their_schemas() {
    let r = root();
    let reg = vc_kinds::FileKindRegistry::load(&vc_config::KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    })
    .expect("kind registry");
    for (_, adapter, url, file, _) in OPEN_DATA_READERS {
        let u = vc_core::CanonicalUrl::parse(url).unwrap();
        let items = vc_sources::adapters::records::records(adapter, &u, &recorded(file))
            .expect("mapped")
            .expect("fixture maps");
        assert!(!items.is_empty(), "{file}: no records");
        for it in items {
            if let Err(e) = reg.validate(&vc_core::KindId::new(it.kind), &it.data) {
                panic!("{file}: {} does not fit: {e:?}\n{:#}", it.kind, it.data);
            }
        }
    }
}

#[tokio::test]
async fn open_data_readers_reach_the_index_with_their_licence() {
    let r = rig_real(false).await;
    let mut ids: Vec<&str> = OPEN_DATA_READERS.iter().map(|c| c.0).collect();
    ids.dedup();
    seed_real(&r.pool, &ids).await;
    for (source, adapter, url, file, is_dump) in OPEN_DATA_READERS {
        let body = text(file);
        let cap = if *is_dump {
            dump(url, source, adapter, MediaType::Csv, &body)
        } else {
            api(url, source, adapter, MediaType::Json, &body)
        };
        let o = run(&r, cap).await;
        assert_processed(&o);
    }
    for (kind, source, want, licence) in [
        ("restaurant", "od-chicago-food-inspections", 2, "open"),
        ("restaurant", "od-austin-food-inspections", 5, "open"),
        ("hotel", "od-catalonia-tourism-register", 7, "facts_only"),
        ("amenity", "od-nyc-public-restrooms", 5, "open"),
        ("hotel", "od-fr-atout-france-classes", 7, "open"),
        ("amenity", "od-wien-ogd", 2, "open"),
        ("attraction", "od-wien-ogd", 3, "open"),
        ("amenity", "od-zuerich-ogd", 1, "open"),
    ] {
        let rs = records_of(&r, kind, source).await;
        assert_eq!(rs.len(), want, "{kind} {source}: {rs:?}");
        let a = attributions(&r, kind, source).await;
        assert_eq!(a.len(), 1, "{source}");
        assert_eq!(a[0].0, licence, "{source}");
        assert!(!a[0].1.is_empty(), "{source}");
    }
    assert_eq!(
        attributions(&r, "amenity", "od-wien-ogd").await[0].1,
        "Datenquelle: Stadt Wien – data.wien.gv.at, CC BY 4.0"
    );
}

#[test]
fn factor_table_records_fit_the_kind_registry() {
    let r = root();
    let reg = vc_kinds::FileKindRegistry::load(&vc_config::KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    })
    .expect("kind registry");
    for (url, file, kind) in [
        (
            "https://assets.publishing.service.gov.uk/media/x/ghg-conversion-factors-2026-flat-format-revised.xlsx",
            "factor_desnz_flat_2026.xlsx",
            "emission_factor",
        ),
        (
            "https://ourworldindata.org/grapher/ghg-per-kg-poore.csv",
            "factor_owid_food_poore.csv",
            "emission_factor",
        ),
        (
            "https://ourworldindata.org/grapher/electricity-mix.csv",
            "factor_owid_grid_ci.csv",
            "grid_carbon_intensity",
        ),
        // Second batch (2026-11-19).
        (
            "https://www.epa.gov/system/files/documents/2025-06/summary_tables_rev2.xlsx",
            "factor_egrid_summary_2023.xlsx",
            "emission_factor",
        ),
        (
            "https://www.epa.gov/system/files/other-files/2025-01/ghg-emission-factors-hub-2025.xlsx",
            "factor_epa_ef_hub_2025.xlsx",
            "emission_factor",
        ),
        (
            "https://ourworldindata.org/grapher/carbon-footprint-travel-mode.csv",
            "factor_owid_travel_mode.csv",
            "emission_factor",
        ),
        (
            "https://raw.githubusercontent.com/owid/co2-data/master/owid-co2-data.csv",
            "factor_owid_co2_data.csv",
            "emission_factor",
        ),
        (
            "https://dataportal.orr.gov.uk/media/1527/table-6100-estimates-of-normalised-passenger-carbon-dioxide-equivalent-emissions.ods",
            "factor_orr_6100.ods",
            "emission_factor",
        ),
        (
            "https://www.fueleconomy.gov/feg/epadata/vehicles.csv.zip",
            "factor_fueleconomy_vehicles.csv.zip",
            "emission_factor",
        ),
        (
            "https://concito.dk/files/media/document/Version%201.2_DK_ENG_rev_18122025_0.xlsx",
            "factor_concito_v1_2.xlsx",
            "emission_factor",
        ),
        (
            "https://www.rivm.nl/sites/default/files/2026-03/Database-milieubelasting-voedingsmiddelen-database-versie-23-september-2024-met-tabblad-per-voedingsmiddelengroep.xlsx",
            "factor_rivm_2024.xlsx",
            "emission_factor",
        ),
        (
            "https://ec.europa.eu/eurostat/api/dissemination/statistics/1.0/data/sdg_13_31",
            "eurostat_sdg_13_31.json",
            "emission_factor",
        ),
        // Third batch (2026-11-19): mobitool, AIB residual mixes, EPA supply chain, OWID.
        (
            "https://assets.ctfassets.net/4y40wxcxzkmz/a/b/mobitool-Faktoren-v3.1-20250408.xlsx",
            "factor_mobitool_v3_1.xlsx",
            "emission_factor",
        ),
        (
            "https://www.aib-net.org/sites/default/files/assets/eecs/Residual%20Mix/2025_Final%20_Residual%20mix%20calculation%20results_25052026.xlsx",
            "factor_aib_residual_mix_2025.xlsx",
            "emission_factor",
        ),
        (
            "https://pasteur.epa.gov/uploads/10.23719/1531143/SupplyChainGHGEmissionFactors_v1.3.0_NAICS_CO2e_USD2022.csv",
            "factor_epa_supply_chain_v1_3.csv",
            "emission_factor",
        ),
        (
            "https://ourworldindata.org/grapher/per-capita-co-emissions-from-international-flights-tourism-adjusted.csv",
            "factor_owid_tourism_aviation.csv",
            "emission_factor",
        ),
        // Occupancy (2026-11-19): DfT TAG data book table A1.3.3.2.
        (
            "https://assets.publishing.service.gov.uk/media/x/tag-data-book-v2-03.xlsm",
            "factor_dft_tag_occupancy_2026.xlsx",
            "occupancy",
        ),
    ] {
        let u = vc_core::CanonicalUrl::parse(url).unwrap();
        let adapter = if file.starts_with("eurostat_") {
            "eurostat_jsonstat"
        } else {
            "factor_table"
        };
        let items = vc_sources::adapters::records::records(adapter, &u, &recorded(file))
            .expect("mapped")
            .expect("fixture maps");
        assert!(!items.is_empty(), "{file}");
        for it in items {
            assert_eq!(it.kind, kind, "{file}");
            if let Err(e) = reg.validate(&vc_core::KindId::new(it.kind), &it.data) {
                panic!("{file}: {} does not fit: {e:?}\n{:#}", it.kind, it.data);
            }
            let tmpl = reg.series_key_template(&vc_core::KindId::new(it.kind));
            assert_eq!(tmpl.is_some(), it.series_key.is_some(), "{file}");
            if let Some(k) = &it.series_key {
                assert_eq!(
                    reg.series_key(&vc_core::KindId::new(it.kind), &it.data, None)
                        .as_ref(),
                    Some(k),
                    "{file}: the adapter's series key is the registry's"
                );
            }
        }
    }
}

#[test]
fn ember_api_records_fit_the_kind_registry() {
    let r = root();
    let reg = vc_kinds::FileKindRegistry::load(&vc_config::KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    })
    .expect("kind registry");
    for (url, file) in [
        (
            "https://api.ember-energy.org/v1/carbon-intensity/yearly?is_aggregate_entity=false&start_date=2000",
            "ember_ci_yearly.json",
        ),
        (
            "https://api.ember-energy.org/v1/carbon-intensity/monthly?is_aggregate_entity=false&start_date=2019-01",
            "ember_ci_monthly.json",
        ),
    ] {
        let u = vc_core::CanonicalUrl::parse(url).unwrap();
        let items = vc_sources::adapters::records::records("ember_api", &u, &recorded(file))
            .expect("mapped")
            .expect("fixture maps");
        assert!(!items.is_empty(), "{file}");
        for it in items {
            assert_eq!(it.kind, "grid_carbon_intensity", "{file}");
            if let Err(e) = reg.validate(&vc_core::KindId::new(it.kind), &it.data) {
                panic!("{file}: does not fit: {e:?}\n{:#}", it.data);
            }
            assert_eq!(
                reg.series_key(&vc_core::KindId::new(it.kind), &it.data, None),
                it.series_key,
                "{file}: the adapter's series key is the registry's"
            );
        }
    }
}

#[test]
fn place_fact_records_fit_the_kind_registry() {
    // Place facts (2026-11-20): wikidata_facts (a dump batch of the slim Berlin item),
    // and factbook_json (a country file).
    let r = root();
    let reg = vc_kinds::FileKindRegistry::load(&vc_config::KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    })
    .expect("kind registry");
    let berlin = vc_sources::adapters::wikidata_facts::entity_of(
        &recorded("wikidata_entity_q64.json"),
        "Q64",
    )
    .unwrap()
    .unwrap();
    let batch = serde_json::to_vec(&serde_json::json!([berlin])).unwrap();
    for (adapter, url, body) in [
        (
            "wikidata_facts",
            "https://dumps.wikimedia.org/wikidatawiki/entities/latest-all.json.gz",
            batch,
        ),
        (
            "factbook_json",
            "https://raw.githubusercontent.com/factbook/factbook.json/master/europe/gm.json",
            recorded("factbook_gm.json"),
        ),
    ] {
        let u = vc_core::CanonicalUrl::parse(url).unwrap();
        let items = vc_sources::adapters::records::records(adapter, &u, &body)
            .expect("mapped")
            .expect("fixture maps");
        assert!(!items.is_empty(), "{adapter}");
        for it in items {
            assert_eq!(it.kind, "place_fact", "{adapter}");
            if let Err(e) = reg.validate(&vc_core::KindId::new(it.kind), &it.data) {
                panic!("{adapter}: does not fit: {e:?}\n{:#}", it.data);
            }
        }
    }
}

#[tokio::test]
async fn a_wikidata_bundle_stales_a_removed_statement_and_an_empty_bundle_stales_all() {
    // The EntityData bundle carries the item's status (it is one response, not a dump batch),
    // so a fact whose statement was removed on Wikidata goes stale on the next crawl, and an
    // item that is no longer a place (an empty bundle) stales every fact of its row.
    use vc_sources::adapters::wikidata_facts;
    let r = rig(&[], RigOpts::default()).await;
    let spec = vc_sources::spec::parse_spec(
        "id: wikidata-facts-test\nname: Wikidata EntityData (test)\ncategory: facts\n\
         source_class: reference\nlicence:\n  class: open\n  spdx: CC0-1.0\n  \
         attribution: Wikidata (CC0 1.0)\nmethod: api\n\
         seeds:\n- https://www.wikidata.org/wiki/Special:EntityData/Q64.json\n\
         adapter: wikidata_facts\nkinds:\n- place_fact\nllm_kinds: []\nrender: never\n\
         cadence: monthly\nmin_delay_s: 5\ndaily_budget: 100\nenabled: true\n",
    )
    .expect("spec");
    vc_sources::seed::seed_sources(&r.pool, &[spec], false)
        .await
        .expect("seed");
    let url = "https://www.wikidata.org/wiki/Special:EntityData/Q64.json";
    let berlin = wikidata_facts::entity_of(&recorded("wikidata_entity_q64.json"), "Q64")
        .unwrap()
        .unwrap();
    let bundle = |e: serde_json::Value| {
        serde_json::to_string(&wikidata_facts::bundle(e, Default::default())).unwrap()
    };
    // A bundle as the adapter spools it: a `resource` with the item's status.
    fn cap<'a>(url: &'a str, body: &'a str, minutes: i64) -> Cap<'a> {
        Cap {
            media: MediaType::Json,
            status: Some(200),
            fetch_mode: FetchMode::Adapter("wikidata_facts".into()),
            ..Cap::html(url, "wikidata-facts-test", body, at(minutes))
        }
    }
    let states = || async {
        sqlx::query_as::<_, (String, String)>(
            "SELECT r.record_key, r.state FROM records r WHERE r.kind = 'place_fact'
             ORDER BY r.record_key",
        )
        .fetch_all(&r.pool)
        .await
        .unwrap()
    };
    let b1 = bundle(berlin.clone());
    assert_processed(&run(&r, cap(url, &b1, 0)).await);
    let s1 = states().await;
    assert!(s1.len() >= 3, "{s1:?}");
    assert!(s1.iter().all(|(_, st)| st == "active"));

    // The nickname (P1449) is removed on Wikidata.
    let mut fewer = berlin.clone();
    fewer["claims"]
        .as_object_mut()
        .unwrap()
        .remove("P1449")
        .expect("Berlin has a nickname");
    let b2 = bundle(fewer);
    assert_processed(&run(&r, cap(url, &b2, 60)).await);
    let s2 = states().await;
    let stale: Vec<&String> = s2
        .iter()
        .filter(|(_, st)| st == "stale")
        .map(|(k, _)| k)
        .collect();
    assert_eq!(stale.len(), 1, "only the removed nickname: {s2:?}");
    let data: serde_json::Value = sqlx::query_scalar(
        "SELECT v.data FROM records r JOIN record_versions v
           ON v.record_id = r.record_id AND v.version = r.current_version
         WHERE r.state = 'stale'",
    )
    .fetch_one(&r.pool)
    .await
    .unwrap();
    assert_eq!(data["source_field"], "wikidata:P1449");

    // The item is no longer a place: an empty bundle stales everything.
    let b3 = bundle(serde_json::Value::Null);
    assert_processed(&run(&r, cap(url, &b3, 120)).await);
    assert!(states().await.iter().all(|(_, st)| st == "stale"));
}
