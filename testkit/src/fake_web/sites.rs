//! The seven scripted sites of spec §12 (the E2E "mini internet").
//!
//! | Host | Site | What it exercises |
//! |---|---|---|
//! | [`BLOG`] | blog with RSS + sitemap | feed discovery; a post that changes in cycle `content_change_cycle` (new version); robots.txt that starts disallowing `/private/` in cycle `robots_change_cycle` (Review Focus 1); a cross-key redirect and a redirect to a private IP (Review Focus 2); a link to a denylisted host; `410 Gone` |
//! | [`SPA`] | SPA attractions site | an empty shell that needs rendering; its script (from the CDN) loads JSON from the site and tries a private IP and the metadata server (must be blocked) and an image (blocked type) |
//! | [`EVENTS`] | events with JSON-LD | `schema.org/Event` JSON-LD; a date change in cycle `content_change_cycle` |
//! | [`HOSTILE`] | hostile site | every page is a Cloudflare-style challenge (`403`, `cf-mitigated: challenge`): detect → park |
//! | [`SLOW`] | slow site | `Crawl-delay`, latency on every response, a `503 Retry-After` page |
//! | [`VISA`] | official visa page | an official source (`visa.immigration.test`, key `immigration.test` ≠ host); `X-Robots-Tag: noarchive` and `noindex` pages; a robots-disallowed `/admin/` |
//! | [`CDN`] | CDN asset host | the SPA's JS/CSS (allowlisted third-party sub-resources) and an image |
//!
//! [`DENIED`] is never served as a site; it is on the oracle's denylist and linked from the blog.

use std::time::Duration;

use super::{Reply, Site};

/// Blog with RSS.
pub const BLOG: &str = "blog.test";
/// SPA attractions site that needs rendering.
pub const SPA: &str = "attractions.test";
/// Events with JSON-LD.
pub const EVENTS: &str = "events.test";
/// Hostile site serving a challenge.
pub const HOSTILE: &str = "hostile.test";
/// Slow site with Crawl-delay.
pub const SLOW: &str = "slow.test";
/// Official visa page (key `immigration.test`).
pub const VISA: &str = "visa.immigration.test";
/// CDN asset host.
pub const CDN: &str = "cdn.test";
/// A denylisted host (linked from the blog, never to be requested).
pub const DENIED: &str = "denied.test";
/// The private address the blog redirects to and the SPA script tries.
pub const PRIVATE_IP: &str = "10.0.0.5";
/// The metadata server the SPA script tries.
pub const METADATA_IP: &str = "169.254.169.254";

/// Every site host, in table order.
pub const HOSTS: [&str; 7] = [BLOG, SPA, EVENTS, HOSTILE, SLOW, VISA, CDN];

/// The body of the challenge interstitial (Cloudflare-style markers).
pub const CHALLENGE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en-US"><head><title>Just a moment...</title>
<meta http-equiv="refresh" content="390">
<script src="/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1"></script></head>
<body><div class="main-wrapper"><h1>Checking if the site connection is secure</h1>
<form id="challenge-form" action="/?__cf_chl_f_tk=fake" method="POST">
<input type="hidden" name="md" value="fake"></form>
<noscript>Enable JavaScript and cookies to continue</noscript></div></body></html>
"#;

/// Knobs of the scripted sites.
#[derive(Debug, Clone)]
pub struct SiteOptions {
    /// Cycle from which content changes (blog post, event date, new feed item). Default 1.
    pub content_change_cycle: usize,
    /// Cycle from which the blog's robots.txt disallows `/private/`. Default 2.
    pub robots_change_cycle: usize,
    /// The slow site's `Crawl-delay` in seconds. Default 10.
    pub slow_crawl_delay_s: u32,
    /// The slow site's latency per response. Default 2 s.
    pub slow_latency: Duration,
    /// `Retry-After` (seconds) of the slow site's `/busy`. Default 120.
    pub slow_retry_after_s: u32,
}

impl Default for SiteOptions {
    fn default() -> Self {
        SiteOptions {
            content_change_cycle: 1,
            robots_change_cycle: 2,
            slow_crawl_delay_s: 10,
            slow_latency: Duration::from_secs(2),
            slow_retry_after_s: 120,
        }
    }
}

/// All seven sites.
pub fn all(o: &SiteOptions) -> Vec<Site> {
    vec![
        blog(o),
        spa(o),
        events(o),
        hostile(o),
        slow(o),
        visa(o),
        cdn(o),
    ]
}

/// `[before; n] ++ [after]`: a script that switches at cycle `n`.
fn switch(n: usize, before: Option<Reply>, after: Option<Reply>) -> Vec<Option<Reply>> {
    let mut v = vec![before; n];
    v.push(after);
    v
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>{title}</title></head>\n<body><main><h1>{title}</h1>\n{body}\n</main></body></html>\n"
    )
}

/// The blog with RSS.
pub fn blog(o: &SiteOptions) -> Site {
    let base = format!("http://{BLOG}");
    let robots_v1 = format!("User-agent: *\nAllow: /\n\nSitemap: {base}/sitemap.xml\n");
    let robots_v2 =
        format!("User-agent: *\nDisallow: /private/\nAllow: /\n\nSitemap: {base}/sitemap.xml\n");
    let index = page(
        "Travel notes",
        &format!(
            r#"<p>Notes from the road.</p>
<link rel="alternate" type="application/rss+xml" href="/feed.xml">
<ul>
<li><a href="/posts/hello">Hello Kyoto</a></li>
<li><a href="/posts/second">Osaka street food</a></li>
<li><a href="/private/draft">Draft</a></li>
<li><a href="/go/events">Events this month</a></li>
<li><a href="/go/internal">Internal</a></li>
<li><a href="http://{DENIED}/deal">A deal</a></li>
<li><a href="/gone">Old post</a></li>
</ul>"#
        ),
    );
    let item = |slug: &str, title: &str, date: &str| {
        format!(
            "<item><title>{title}</title><link>{base}/posts/{slug}</link><guid>{base}/posts/{slug}</guid><pubDate>{date}</pubDate></item>"
        )
    };
    let feed = |items: &[String]| {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Travel notes</title><link>{base}/</link><description>Notes</description>
{}
</channel></rss>
"#,
            items.join("\n")
        )
    };
    let i1 = item("hello", "Hello Kyoto", "Mon, 05 Jan 2026 09:00:00 GMT");
    let i2 = item(
        "second",
        "Osaka street food",
        "Tue, 06 Jan 2026 09:00:00 GMT",
    );
    let i3 = item("third", "Nara deer park", "Wed, 07 Jan 2026 09:00:00 GMT");
    let sitemap = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
<url><loc>{base}/</loc></url>
<url><loc>{base}/posts/hello</loc><lastmod>2026-01-05</lastmod></url>
<url><loc>{base}/posts/second</loc><lastmod>2026-01-06</lastmod></url>
<url><loc>{base}/private/draft</loc></url>
</urlset>
"#
    );
    let n = o.content_change_cycle;
    Site::new("blog", BLOG)
        .route_cycles(
            "/robots.txt",
            switch(
                o.robots_change_cycle,
                Some(Reply::text(robots_v1)),
                Some(Reply::text(robots_v2)),
            ),
        )
        .route("/", Reply::html(index))
        .route("/sitemap.xml", Reply::xml(sitemap))
        .route_cycles(
            "/feed.xml",
            switch(
                n,
                Some(Reply::rss(feed(&[i1.clone(), i2.clone()]))),
                Some(Reply::rss(feed(&[i1, i2, i3]))),
            ),
        )
        .route_cycles(
            "/posts/hello",
            switch(
                n,
                Some(Reply::html(page(
                    "Hello Kyoto",
                    "<p>Kinkaku-ji opens at 9:00. Entry costs 400 yen.</p>",
                ))),
                Some(Reply::html(page(
                    "Hello Kyoto",
                    "<p>Kinkaku-ji opens at 9:00. Entry now costs 500 yen.</p>",
                ))),
            ),
        )
        .route(
            "/posts/second",
            Reply::html(page(
                "Osaka street food",
                "<p>Try takoyaki in Dotonbori.</p>",
            )),
        )
        .route_cycles(
            "/posts/third",
            switch(
                n,
                None,
                Some(Reply::html(page(
                    "Nara deer park",
                    "<p>Deer crackers cost 200 yen.</p>",
                ))),
            ),
        )
        .route(
            "/private/draft",
            Reply::html(page("Draft", "<p>Not for publication.</p>")),
        )
        .route(
            "/go/events",
            Reply::redirect(301, &format!("http://{EVENTS}/")),
        )
        .route(
            "/go/internal",
            Reply::redirect(302, &format!("http://{PRIVATE_IP}/admin")),
        )
        .route("/gone", Reply::status(410))
}

/// The SPA attractions site (needs rendering).
pub fn spa(_o: &SiteOptions) -> Site {
    let shell = |title: &str| {
        format!(
            r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>{title}</title>
<link rel="stylesheet" href="http://{CDN}/css/site.css">
<script src="http://{CDN}/js/app.js" defer></script></head>
<body><div id="root"></div><noscript>You need to enable JavaScript to run this app.</noscript></body></html>
"#
        )
    };
    let data = serde_json::json!({
        "attractions": [
            {"id": "kinkaku-ji", "name": "Kinkaku-ji", "city": "Kyoto", "country": "Japan",
             "opening_hours": "09:00-17:00", "price": {"amount": 500, "currency": "JPY"}},
            {"id": "fushimi-inari", "name": "Fushimi Inari Taisha", "city": "Kyoto", "country": "Japan",
             "opening_hours": "00:00-24:00", "price": {"amount": 0, "currency": "JPY"}}
        ]
    });
    Site::new("spa", SPA)
        .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
        .route("/", Reply::html(shell("Kyoto attractions")))
        .route("/attractions/kinkaku-ji", Reply::html(shell("Kinkaku-ji")))
        .route("/api/attractions.json", Reply::json(&data))
        .subresources(&["/api/"])
}

/// The events site with JSON-LD.
pub fn events(o: &SiteOptions) -> Site {
    let base = format!("http://{EVENTS}");
    let event = |date: &str| {
        let ld = serde_json::json!({
            "@context": "https://schema.org",
            "@type": "Event",
            "@id": format!("{base}/events/jazz-night#event"),
            "name": "Jazz Night at the River",
            "startDate": date,
            "eventStatus": "https://schema.org/EventScheduled",
            "location": {"@type": "Place", "name": "Kamogawa Riverside",
                         "address": {"@type": "PostalAddress", "addressLocality": "Kyoto", "addressCountry": "JP"}},
            "offers": {"@type": "Offer", "price": "2500", "priceCurrency": "JPY"}
        });
        Reply::html(format!(
            "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><title>Jazz Night at the River</title>\n<script type=\"application/ld+json\">{ld}</script></head>\n<body><h1>Jazz Night at the River</h1><p>Live jazz on {date} in Kyoto, Japan.</p></body></html>\n"
        ))
    };
    let fair = serde_json::json!({
        "@context": "https://schema.org", "@type": "Event", "name": "Kyoto Food Fair",
        "startDate": "2026-03-01T10:00:00+09:00",
        "location": {"@type": "Place", "name": "Miyako Messe",
                     "address": {"@type": "PostalAddress", "addressLocality": "Kyoto", "addressCountry": "JP"}}
    });
    let sitemap = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
<url><loc>{base}/</loc></url>
<url><loc>{base}/events/jazz-night</loc></url>
<url><loc>{base}/events/food-fair</loc></url>
</urlset>
"#
    );
    Site::new("events", EVENTS)
        .route(
            "/robots.txt",
            Reply::text(format!("User-agent: *\nAllow: /\nSitemap: {base}/sitemap.xml\n")),
        )
        .route(
            "/",
            Reply::html(page(
                "Kyoto events",
                r#"<ul><li><a href="/events/jazz-night">Jazz night</a></li><li><a href="/events/food-fair">Food fair</a></li></ul>"#,
            )),
        )
        .route("/sitemap.xml", Reply::xml(sitemap))
        .route_cycles(
            "/events/jazz-night",
            switch(
                o.content_change_cycle,
                Some(event("2026-02-14T19:00:00+09:00")),
                Some(event("2026-02-21T19:00:00+09:00")),
            ),
        )
        .route(
            "/events/food-fair",
            Reply::html(format!(
                "<!DOCTYPE html>\n<html><head><title>Kyoto Food Fair</title><script type=\"application/ld+json\">{fair}</script></head><body><h1>Kyoto Food Fair</h1></body></html>\n"
            )),
        )
}

/// The hostile site: robots.txt is fine, every page is a challenge.
pub fn hostile(_o: &SiteOptions) -> Site {
    Site::new("hostile", HOSTILE)
        .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
        .fallback(Reply::challenge())
}

/// The slow site with `Crawl-delay` and latency.
pub fn slow(o: &SiteOptions) -> Site {
    let mut s = Site::new("slow", SLOW)
        .route(
            "/robots.txt",
            Reply::text(format!(
                "User-agent: *\nCrawl-delay: {}\nAllow: /\n",
                o.slow_crawl_delay_s
            )),
        )
        .route(
            "/",
            Reply::html(page(
                "Slow guides",
                r#"<ul><li><a href="/guide/kyoto">Kyoto</a></li><li><a href="/guide/osaka">Osaka</a></li><li><a href="/busy">Busy</a></li></ul>"#,
            )),
        )
        .route(
            "/guide/kyoto",
            Reply::html(page("Kyoto guide", "<p>Temples, gardens and tea.</p>")),
        )
        .route(
            "/guide/osaka",
            Reply::html(page("Osaka guide", "<p>Castles and street food.</p>")),
        )
        .route(
            "/busy",
            Reply::status(503).header("retry-after", &o.slow_retry_after_s.to_string()),
        )
        .latency(o.slow_latency);
    s.min_delay_ms = Some(u64::from(o.slow_crawl_delay_s) * 1000);
    s
}

/// The official visa page.
pub fn visa(_o: &SiteOptions) -> Site {
    Site::new("visa", VISA)
        .route("/robots.txt", Reply::text("User-agent: *\nDisallow: /admin/\n"))
        .route(
            "/",
            Reply::html(page(
                "Immigration Bureau",
                r#"<ul><li><a href="/visa/japan">Visa requirements</a></li><li><a href="/notice">Notice</a></li><li><a href="/admin/">Admin</a></li></ul>"#,
            )),
        )
        .route(
            "/visa/japan",
            Reply::html(page(
                "Visa requirements for Japan",
                r#"<table><tr><th>Nationality</th><th>Visa</th><th>Max stay</th></tr>
<tr><td>United States</td><td>Not required</td><td>90 days</td></tr>
<tr><td>India</td><td>Required (eVisa)</td><td>90 days</td></tr></table>
<p>Emergency: police 110, ambulance and fire 119.</p>"#,
            )),
        )
        .route(
            "/notice",
            Reply::html(page("Notice", "<p>Office closed on public holidays.</p>"))
                .header("x-robots-tag", "noarchive"),
        )
        .route(
            "/internal-memo",
            Reply::html(page("Memo", "<p>Internal.</p>")).header("x-robots-tag", "noindex"),
        )
        .route("/admin/", Reply::status(401))
}

/// The CDN asset host (sub-resources of the SPA).
pub fn cdn(_o: &SiteOptions) -> Site {
    let app = format!(
        r#"(function () {{
  var root = document.getElementById('root');
  fetch('/api/attractions.json')
    .then(function (r) {{ return r.json(); }})
    .then(function (d) {{
      root.innerHTML = '<h1>Kyoto attractions</h1><ul>' + d.attractions.map(function (a) {{
        return '<li class="attraction"><h2>' + a.name + '</h2><p>' + a.city + ', ' + a.country +
          '. Open ' + a.opening_hours + '. Entry ' + a.price.amount + ' ' + a.price.currency + '.</p></li>';
      }}).join('') + '</ul>';
    }});
  // Must be blocked by the renderer (SSRF guard), never reach the network.
  fetch('http://{METADATA_IP}/computeMetadata/v1/').catch(function () {{}});
  fetch('http://{PRIVATE_IP}/internal').catch(function () {{}});
  // Images are a blocked resource type.
  var img = new Image(); img.src = 'http://{CDN}/img/logo.png';
}})();
"#
    );
    // A 1x1 transparent PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    Site::new("cdn", CDN)
        .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
        .route("/js/app.js", Reply::js(app))
        .route(
            "/css/site.css",
            Reply::css("body{font-family:sans-serif}.attraction h2{font-size:1.2em}\n"),
        )
        .route("/img/logo.png", Reply::new(200, "image/png", PNG))
        .subresources(&["/js/", "/css/", "/img/"])
}
