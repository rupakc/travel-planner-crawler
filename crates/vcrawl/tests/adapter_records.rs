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
