//! Handler tests (plan Task 6). Every input is untrusted: handlers must respect `Limits`, never
//! fetch, and refuse XML DTDs.

use std::io::Write;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use proptest::prelude::*;
use serde_json::{Value, json};
use testkit::vc_parse_fixtures::capture_meta;
use vc_core::ErrorClass;
use vc_core::media::MediaType;
use vc_core::parse::{ContentHandler, Limits, LinkKind, Parsed};
use vc_parse::{
    CsvHandler, FeedHandler, HtmlHandler, JsonHandler, SitemapHandler, hamming, parse, registry,
    simhash64,
};

const PAGE: &str = "https://example.com/events/jazz";

fn html(body: &str) -> Parsed {
    html_with(body, &Limits::default())
}

fn html_with(body: &str, lim: &Limits) -> Parsed {
    let meta = capture_meta(PAGE, MediaType::Html);
    HtmlHandler::default()
        .handle(&meta, body.as_bytes(), lim)
        .expect("html parses")
}

fn by_type<'a>(p: &'a Parsed, t: &str) -> Vec<&'a Value> {
    p.structured
        .iter()
        .filter(|v| v.get("@type").and_then(Value::as_str) == Some(t))
        .collect()
}

fn gz(data: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

// ---------------------------------------------------------------- structured data

#[test]
fn jsonld_event_extracted() {
    let page = r##"<!doctype html><html lang="en"><head><title>Jazz Night</title>
      <script type="application/ld+json">
      {"@context":"https://schema.org","@graph":[
        {"@type":"Event","@id":"#jazz","name":"Jazz Night","startDate":"2026-12-01T20:00",
         "location":{"@type":"Place","name":"Blue Note"}},
        {"@type":"WebPage","name":"Events"}]}
      </script>
      <script type="application/ld+json">[{"@context":"https://schema.org","@type":"Event","name":"Blues Brunch"}]</script>
      <script type="application/ld+json">{ this is not json </script>
      </head><body><p>Jazz tonight.</p></body></html>"##;
    let p = html(page);
    let events = by_type(&p, "Event");
    assert_eq!(events.len(), 2, "{:?}", p.structured);
    let jazz = events
        .iter()
        .find(|e| e["name"] == "Jazz Night")
        .expect("graph event");
    assert_eq!(jazz["startDate"], "2026-12-01T20:00");
    assert_eq!(jazz["@id"], "#jazz");
    assert_eq!(jazz["location"]["name"], "Blue Note");
    // The graph's @context is carried onto every flattened node.
    assert_eq!(jazz["@context"], "https://schema.org");
    assert!(events.iter().any(|e| e["name"] == "Blues Brunch"));
    assert_eq!(by_type(&p, "WebPage").len(), 1);
}

#[test]
fn microdata_place_extracted() {
    let page = r#"<html><body>
      <div itemscope itemtype="https://schema.org/Place">
        <h1 itemprop="name">Old Town Square</h1>
        <div itemprop="geo" itemscope itemtype="https://schema.org/GeoCoordinates">
          <meta itemprop="latitude" content="50.087">
          <meta itemprop="longitude" content="14.421">
        </div>
        <a itemprop="url" href="/places/old-town">More</a>
        <span itemprop="keywords">square</span><span itemprop="keywords">market</span>
      </div></body></html>"#;
    let p = html(page);
    let places = by_type(&p, "Place");
    assert_eq!(places.len(), 1, "{:?}", p.structured);
    let place = places[0];
    assert_eq!(place["@context"], "https://schema.org");
    assert_eq!(place["name"], "Old Town Square");
    assert_eq!(place["url"], "https://example.com/places/old-town");
    assert_eq!(place["geo"]["@type"], "GeoCoordinates");
    assert_eq!(place["geo"]["latitude"], "50.087");
    assert_eq!(place["geo"]["longitude"], "14.421");
    assert_eq!(place["keywords"], json!(["square", "market"]));
    // The nested item is not reported again as a top-level item.
    assert!(by_type(&p, "GeoCoordinates").is_empty());
}

// ---------------------------------------------------------------- sitemaps

#[test]
fn sitemap_index_and_gz() {
    let meta = capture_meta("https://example.com/sitemap.xml", MediaType::Sitemap);
    let index = r#"<?xml version="1.0" encoding="UTF-8"?>
      <sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
        <sitemap><loc>https://example.com/sitemap-events.xml.gz</loc><lastmod>2026-11-01</lastmod></sitemap>
        <sitemap><loc>https://example.com/sitemap-places.xml</loc></sitemap>
      </sitemapindex>"#;
    let p = SitemapHandler
        .handle(&meta, index.as_bytes(), &Limits::default())
        .unwrap();
    assert_eq!(p.links.len(), 2);
    assert!(p.links.iter().all(|l| l.kind == LinkKind::Sitemap));
    assert_eq!(
        p.links[0].url.as_str(),
        "https://example.com/sitemap-events.xml.gz"
    );
    assert_eq!(
        p.links[0].lastmod.unwrap().to_rfc3339(),
        "2026-11-01T00:00:00+00:00"
    );
    assert!(p.links[1].lastmod.is_none());

    let urlset = r#"<?xml version="1.0" encoding="UTF-8"?>
      <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
        <url><loc>https://example.com/e?id=1&amp;lang=en</loc><lastmod>2026-11-02T10:30:00+01:00</lastmod></url>
        <url><loc> https://example.com/e2 </loc><lastmod>2026-11-03T08:00Z</lastmod></url>
        <url><loc>https://example.com/e3</loc><lastmod>not a date</lastmod></url>
      </urlset>"#;
    let p = SitemapHandler
        .handle(&meta, &gz(urlset.as_bytes()), &Limits::default())
        .unwrap();
    let urls: Vec<_> = p.links.iter().map(|l| l.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://example.com/e?id=1&lang=en",
            "https://example.com/e2",
            "https://example.com/e3"
        ]
    );
    assert!(p.links.iter().all(|l| l.kind == LinkKind::Page));
    assert_eq!(
        p.links[0].lastmod.unwrap().to_rfc3339(),
        "2026-11-02T09:30:00+00:00"
    );
    assert_eq!(
        p.links[1].lastmod.unwrap().to_rfc3339(),
        "2026-11-03T08:00:00+00:00"
    );
    assert!(p.links[2].lastmod.is_none());

    // The entry cap holds.
    let lim = Limits {
        max_sitemap_entries: 2,
        ..Limits::default()
    };
    let p = SitemapHandler
        .handle(&meta, urlset.as_bytes(), &lim)
        .unwrap();
    assert_eq!(p.links.len(), 2);

    // A plain-text sitemap: one URL per line, junk skipped.
    let meta = capture_meta("https://example.com/sitemap.txt", MediaType::SitemapText);
    let p = SitemapHandler
        .handle(
            &meta,
            b"https://example.com/a\n\nnot a url\nhttps://example.com/b\r\nftp://x/y\n",
            &Limits::default(),
        )
        .unwrap();
    let urls: Vec<_> = p.links.iter().map(|l| l.url.as_str()).collect();
    assert_eq!(urls, ["https://example.com/a", "https://example.com/b"]);
}

// ---------------------------------------------------------------- feeds

#[test]
fn feed_rss_and_atom() {
    let rss = r#"<?xml version="1.0"?>
      <rss version="2.0"><channel><title>Travel Blog</title><link>https://blog.example.com/</link>
        <language>en-gb</language>
        <item><title>Lisbon in 3 days</title><link>https://blog.example.com/lisbon</link>
          <pubDate>Mon, 02 Nov 2026 10:00:00 GMT</pubDate></item>
        <item><title>Porto food</title><link>https://blog.example.com/porto?utm_source=rss</link></item>
      </channel></rss>"#;
    let meta = capture_meta("https://blog.example.com/feed", MediaType::Rss);
    let p = FeedHandler
        .handle(&meta, rss.as_bytes(), &Limits::default())
        .unwrap();
    assert_eq!(p.title.as_deref(), Some("Travel Blog"));
    assert_eq!(p.lang.as_deref(), Some("en"));
    let items: Vec<_> = p
        .links
        .iter()
        .filter(|l| l.kind == LinkKind::FeedItem)
        .collect();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].anchor, "Lisbon in 3 days");
    assert_eq!(items[0].url.as_str(), "https://blog.example.com/lisbon");
    assert_eq!(
        items[0].lastmod.unwrap().to_rfc3339(),
        "2026-11-02T10:00:00+00:00"
    );
    // Canonicalised (tracking parameter dropped).
    assert_eq!(items[1].url.as_str(), "https://blog.example.com/porto");

    let atom = r#"<?xml version="1.0" encoding="utf-8"?>
      <feed xmlns="http://www.w3.org/2005/Atom"><title>Atom Travel</title>
        <id>urn:uuid:1</id><updated>2026-11-03T00:00:00Z</updated>
        <entry><title>Kyoto temples</title><id>urn:uuid:2</id>
          <link rel="alternate" href="/kyoto"/><updated>2026-11-03T09:00:00Z</updated></entry>
      </feed>"#;
    let meta = capture_meta("https://atom.example.com/feed.xml", MediaType::Atom);
    let p = FeedHandler
        .handle(&meta, atom.as_bytes(), &Limits::default())
        .unwrap();
    assert_eq!(p.title.as_deref(), Some("Atom Travel"));
    assert_eq!(p.links.len(), 1);
    assert_eq!(p.links[0].kind, LinkKind::FeedItem);
    assert_eq!(p.links[0].anchor, "Kyoto temples");
    assert_eq!(p.links[0].url.as_str(), "https://atom.example.com/kyoto");
    assert_eq!(
        p.links[0].lastmod.unwrap().to_rfc3339(),
        "2026-11-03T09:00:00+00:00"
    );
}

// ---------------------------------------------------------------- signals and safety

#[test]
fn meta_noindex_signal() {
    let p = html(r#"<html><head><meta name="robots" content="NOINDEX, follow"></head></html>"#);
    assert!(p.body_signals.noindex);
    assert!(!p.body_signals.noarchive);

    let p = html(r#"<html><head><meta name="VoyagerBot" content="noarchive"></head></html>"#);
    assert!(p.body_signals.noarchive);
    assert!(!p.body_signals.noindex);

    // `none` means noindex + nofollow.
    let p = html(
        r#"<html><head><meta name="robots" content="none"></head>
           <body><a href="/x">x</a></body></html>"#,
    );
    assert!(p.body_signals.noindex);
    assert!(p.links.iter().all(|l| l.kind != LinkKind::Page));

    // Another crawler's group does not apply to us.
    let p = html(r#"<html><head><meta name="googlebot" content="noindex"></head></html>"#);
    assert!(!p.body_signals.noindex);

    // TDMRep in HTML.
    let p = html(r#"<html><head><meta name="tdm-reservation" content="1"></head></html>"#);
    assert!(p.body_signals.tdm_reserved);
    let p = html(r#"<html><head><meta name="tdm-reservation" content="0"></head></html>"#);
    assert!(!p.body_signals.tdm_reserved);
    let p = html(r#"<html><head><link rel="tdm-reservation" href="/tdm.json"></head></html>"#);
    assert!(p.body_signals.tdm_reserved);
}

const BILLION_LAUGHS: &str = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [
 <!ENTITY lol "lol">
 <!ENTITY lol1 "&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;">
 <!ENTITY lol2 "&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;">
 <!ENTITY lol3 "&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;">
 <!ENTITY lol4 "&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;">
 <!ENTITY lol5 "&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;&lol4;">
 <!ENTITY lol6 "&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;&lol5;">
 <!ENTITY lol7 "&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;&lol6;">
 <!ENTITY lol8 "&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;&lol7;">
 <!ENTITY lol9 "&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;&lol8;">
]>"#;

#[test]
fn xml_entity_expansion_rejected() {
    let started = Instant::now();
    let lim = Limits::default();
    let cases: Vec<(MediaType, &dyn ContentHandler, String)> = vec![
        (
            MediaType::Sitemap,
            &SitemapHandler,
            format!("{BILLION_LAUGHS}<urlset><url><loc>https://example.com/&lol9;</loc></url></urlset>"),
        ),
        (
            MediaType::Rss,
            &FeedHandler,
            format!("{BILLION_LAUGHS}<rss version=\"2.0\"><channel><title>&lol9;</title></channel></rss>"),
        ),
        (
            MediaType::Atom,
            &FeedHandler,
            format!("{BILLION_LAUGHS}<feed xmlns=\"http://www.w3.org/2005/Atom\"><title>&lol9;</title></feed>"),
        ),
        // External entity (XXE), with a comment and PI before the DOCTYPE.
        (
            MediaType::Rss,
            &FeedHandler,
            r#"<?xml version="1.0"?><!-- c --><?pi x?><!DOCTYPE r [<!ENTITY x SYSTEM "file:///etc/passwd">]><rss><channel><title>&x;</title></channel></rss>"#.to_string(),
        ),
    ];
    for (media, h, body) in cases {
        let meta = capture_meta("https://example.com/x.xml", media);
        let err = h.handle(&meta, body.as_bytes(), &lim).unwrap_err();
        assert_eq!(err.class(), ErrorClass::Policy, "{media}: {err}");
    }
    // Also inside a gzip'd sitemap.
    let meta = capture_meta("https://example.com/x.xml.gz", MediaType::Sitemap);
    let body = gz(format!("{BILLION_LAUGHS}<urlset></urlset>").as_bytes());
    assert_eq!(
        SitemapHandler
            .handle(&meta, &body, &lim)
            .unwrap_err()
            .class(),
        ErrorClass::Policy
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn dom_depth_capped() {
    let depth = 20_000;
    let mut page = String::from("<html><body><p>Shallow text that stays.</p>");
    page.push_str(&"<div>".repeat(depth));
    page.push_str("DEEPMARKER <a href=\"/deep\">deep</a>");
    page.push_str(&"</div>".repeat(depth));
    page.push_str("</body></html>");
    let lim = Limits {
        max_dom_depth: 64,
        ..Limits::default()
    };
    let p = html_with(&page, &lim);
    let text = p.main_text.unwrap_or_default();
    assert!(text.contains("Shallow text"), "{text}");
    assert!(!text.contains("DEEPMARKER"));
    assert!(p.links.iter().all(|l| !l.url.as_str().ends_with("/deep")));
}

#[test]
fn dom_node_count_capped() {
    let mut page = String::from("<html><body>");
    for i in 0..5_000 {
        page.push_str(&format!("<p><a href=\"/p{i}\">page {i}</a></p>"));
    }
    page.push_str("</body></html>");
    let lim = Limits {
        max_nodes: 1_000,
        ..Limits::default()
    };
    let p = html_with(&page, &lim);
    assert!(!p.links.is_empty());
    assert!(p.links.len() < 1_000, "{}", p.links.len());
    assert!(p.links.iter().any(|l| l.url.as_str().ends_with("/p0")));
}

// ---------------------------------------------------------------- html basics

#[test]
fn html_links_text_lang_and_canonical() {
    let page = r##"<!doctype html><html lang="pt-BR"><head><title> Lisboa  guia </title>
      <base href="https://example.com/guides/">
      <link rel="canonical" href="https://example.com/guides/lisboa">
      <link rel="alternate" type="application/rss+xml" href="/feed.xml" title="Feed">
      <link rel="alternate" hreflang="en" href="https://example.com/en/lisbon">
      <style>.x{}</style><script>var hidden = "SCRIPTTEXT";</script></head>
      <body><nav><a href="porto" rel="nofollow  Sponsored">Guia do   Porto</a>
      <a href="javascript:void(0)">js</a><a href="mailto:a@b.c">mail</a>
      <a href="porto">dup</a><a href="https://example.com/events/jazz#top">top</a></nav>
      <article><h1>Lisboa</h1><p>Lisboa é a capital de Portugal e a maior cidade do país. A cidade
      tem bairros históricos, miradouros e uma longa tradição de fado e de pastelaria. O elétrico 28
      atravessa Alfama, a Graça e a Baixa, e o rio Tejo está sempre perto.</p></article>
      </body></html>"##;
    let p = html(page);
    assert_eq!(p.title.as_deref(), Some("Lisboa guia"));
    assert_eq!(p.lang.as_deref(), Some("pt"));
    let text = p.main_text.clone().unwrap();
    assert!(text.contains("capital de Portugal"));
    assert!(!text.contains("SCRIPTTEXT"));
    let pages: Vec<_> = p
        .links
        .iter()
        .filter(|l| l.kind == LinkKind::Page)
        .collect();
    assert_eq!(pages.len(), 1, "{pages:?}"); // porto, deduplicated; self-links (#top) dropped
    let porto = pages
        .iter()
        .find(|l| l.url.as_str() == "https://example.com/guides/porto")
        .unwrap();
    assert_eq!(porto.anchor, "Guia do Porto");
    assert_eq!(porto.rel, ["nofollow", "sponsored"]);
    let kinds = |k| p.links.iter().filter(|l| l.kind == k).count();
    assert_eq!(kinds(LinkKind::Canonical), 1);
    assert_eq!(kinds(LinkKind::Feed), 1);
    assert_eq!(kinds(LinkKind::Alternate), 1);
    assert!(p.simhash.is_some());
    assert!(!p.needs_render);
}

#[test]
fn html_lang_detected_from_text_without_attribute() {
    let p = html(
        "<html><body><p>Der Kölner Dom ist eine römisch-katholische Kirche in Köln und die \
         Kathedrale des Erzbistums Köln. Er ist eines der bekanntesten Bauwerke Deutschlands.</p>\
         </body></html>",
    );
    assert_eq!(p.lang.as_deref(), Some("de"));
}

#[test]
fn spa_shell_needs_render() {
    let p = html(
        r#"<html><head><script src="/app.js"></script></head>
           <body><div id="root"></div><noscript>You need to enable JavaScript.</noscript></body></html>"#,
    );
    assert!(p.needs_render);
    let long = "Plenty of server-rendered text about the old town. ".repeat(20);
    let p = html(&format!(
        r#"<html><head><script src="/app.js"></script></head><body><div id="root"><p>{long}</p></div></body></html>"#
    ));
    assert!(!p.needs_render);
}

#[test]
fn main_text_capped_at_limit() {
    let long = "Word ".repeat(10_000);
    let lim = Limits {
        max_text_bytes: 1_000,
        ..Limits::default()
    };
    let p = html_with(&format!("<html><body><p>{long}</p></body></html>"), &lim);
    assert!(p.main_text.unwrap().len() <= 1_000);
}

#[test]
fn links_capped_at_limit() {
    let mut page = String::from("<html><body>");
    for i in 0..100 {
        page.push_str(&format!("<a href=\"/p{i}\">p</a>"));
    }
    let lim = Limits {
        max_links: 10,
        ..Limits::default()
    };
    assert_eq!(html_with(&page, &lim).links.len(), 10);
}

#[test]
fn legacy_charset_decoded() {
    // "Café" in windows-1252.
    let mut body = b"<html><head><meta charset=\"windows-1252\"><title>Caf".to_vec();
    body.push(0xE9);
    body.extend_from_slice(b"</title></head></html>");
    let meta = capture_meta(PAGE, MediaType::Html);
    let p = HtmlHandler::default()
        .handle(&meta, &body, &Limits::default())
        .unwrap();
    assert_eq!(p.title.as_deref(), Some("Café"));
}

// ---------------------------------------------------------------- json, csv, registry

#[test]
fn json_handler_flattens_jsonld_and_keeps_api_json() {
    let meta = capture_meta("https://example.com/api/x", MediaType::Json);
    let p = JsonHandler
        .handle(
            &meta,
            br#"{"@context":"https://schema.org","@graph":[{"@type":"Place","name":"A"},{"@type":"Place","name":"B"}]}"#,
            &Limits::default(),
        )
        .unwrap();
    assert_eq!(by_type(&p, "Place").len(), 2);
    let p = JsonHandler
        .handle(&meta, br#"{"rates":{"EUR":1.0}}"#, &Limits::default())
        .unwrap();
    assert_eq!(p.structured, vec![json!({"rates":{"EUR":1.0}})]);
    // Too deep → rejected rather than overflowing the stack.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(
        JsonHandler
            .handle(&meta, deep.as_bytes(), &Limits::default())
            .is_err()
    );
}

#[test]
fn csv_handler_rows_as_objects() {
    let meta = capture_meta("https://example.com/x.csv", MediaType::Csv);
    let p = CsvHandler
        .handle(
            &meta,
            b"\xEF\xBB\xBFcode,name\nLIS,Lisbon\nOPO,\"Porto, PT\"\nshort\n",
            &Limits::default(),
        )
        .unwrap();
    assert_eq!(
        p.structured,
        vec![
            json!({"code":"LIS","name":"Lisbon"}),
            json!({"code":"OPO","name":"Porto, PT"}),
            json!({"code":"short"}),
        ]
    );
}

#[test]
fn registry_dispatch_and_sniff() {
    let r = registry();
    for m in [
        MediaType::Html,
        MediaType::Rss,
        MediaType::Atom,
        MediaType::Sitemap,
        MediaType::SitemapText,
        MediaType::Json,
        MediaType::Csv,
    ] {
        assert!(r.for_media(&m).is_some(), "{m}");
    }
    assert!(r.for_media(&MediaType::Pdf).is_none());

    // `parse` dispatches on the stored type and refines generic XML/Other by sniffing,
    // including through gzip.
    let meta = capture_meta("https://example.com/s.xml.gz", MediaType::Other);
    let body =
        gz(b"<?xml version=\"1.0\"?><urlset><url><loc>https://example.com/a</loc></url></urlset>");
    let p = parse(&r, &meta, &body, &Limits::default())
        .unwrap()
        .expect("sniffed as sitemap");
    assert_eq!(p.links.len(), 1);
    let meta = capture_meta("https://example.com/x.pdf", MediaType::Pdf);
    assert!(
        parse(&r, &meta, b"%PDF-1.7", &Limits::default())
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------- simhash

fn words() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec("[a-z]{3,9}", 300..500)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn simhash_near_duplicates_close(
        base in words(),
        other in words(),
        edits in prop::collection::vec((any::<prop::sample::Index>(), "[a-z]{3,9}"), 1..4),
    ) {
        let mut near = base.clone();
        for (i, w) in &edits {
            let i = i.index(near.len());
            near[i] = w.clone();
        }
        let a = simhash64(&base.join(" "));
        let b = simhash64(&near.join(" "));
        let c = simhash64(&other.join(" "));
        prop_assert_eq!(a, simhash64(&base.join("  \n ")), "whitespace-insensitive");
        prop_assert!(hamming(a, b) <= 12, "near: {}", hamming(a, b));
        prop_assert!(hamming(a, c) > 12, "far: {}", hamming(a, c));
    }
}

#[test]
fn simhash_is_stable() {
    // Pinned: the hash is stored, so it must never change across releases.
    assert_eq!(simhash64(""), 0);
    let h = simhash64("The quick brown fox jumps over the lazy dog");
    assert_eq!(
        h,
        simhash64("the QUICK brown fox, jumps over the lazy dog!")
    );
    assert_eq!(format!("{h:016x}"), "18916b020a684455");
}
