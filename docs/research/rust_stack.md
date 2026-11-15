# Rust stack for a polite travel-data crawler (research, 2026-11-15)

Scope: a 24x7 crawler on one or two small GCP VMs (~$100/month total). Hard rules: obey robots.txt (RFC 9309), send an honest bot User-Agent with a contact URL, back off on 403/429/challenge pages. **No** evasion of any kind: no stealth plugins, fingerprint spoofing, UA rotation, CAPTCHA solving or proxy rotation. A challenge page means "stop and back off for that host".

Method: versions, release dates, licences and download counts were read from the crates.io API on 2026-11-15 ("90d" = recent downloads in the last 90 days). Repo activity comes from the GitHub API. Gemini prices come from the official Vertex AI pricing page (cloud.google.com/vertex-ai/generative-ai/pricing), read the same day.

Status legend: **Active** = released in the last ~6 months. **Stable/slow** = old release but small, done, still widely used. **Stale** = no release for 2+ years, so treat it as unmaintained. **Dead** = do not use.

---

## 1. HTTP client

| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **reqwest** | **0.13.5** | 2026-09-08 | 917M / 268M | Active (recommended) |
| hyper-util | 0.1.21 | 2026-09-24 | 639M / 218M | Active (lower level) |
| async-compression | 0.4.52 | 2026-11-02 | 275M / 78M | Active (used by reqwest) |
| reqwest-middleware | 0.5.2 | 2026-05-19 | 99M / 25M | Active |
| reqwest-retry | 0.9.1 | 2026-02-05 | 58M / 14M | Active (but see note) |
| http-cache-reqwest | 0.16.0 | 2025-06-26 | 4.3M / 1.0M | Slowing |
| hickory-resolver | 0.26.3 | 2026-09-10 | 102M / 34M | Active (DNS cache) |
| governor | 0.10.4 | 2025-12-16 | 95M / 24M | Active (GCRA rate limiter) |

**Recommendation: reqwest 0.13.5.** Why: it has every feature we need as cargo features (`http2`, `gzip`, `brotli`, `zstd`, `deflate`, `rustls`, `hickory-dns`, `stream`, `cookies`, `charset`; `http3` is experimental). It sits on hyper 1.x, so we can drop to hyper-util later if a hot path needs it.

- Suggested features: `default-features = false, features = ["rustls", "http2", "gzip", "brotli", "zstd", "deflate", "stream", "charset", "hickory-dns"]`. In 0.13 the TLS feature is named `rustls` (0.12 called it `rustls-tls`).
- Conditional GET: reqwest has no built-in HTTP cache. Do it ourselves. Store `ETag` and `Last-Modified` per URL in Postgres, send `If-None-Match` / `If-Modified-Since`, and treat `304` as "unchanged, reschedule". This is about 30 lines of code. http-cache-reqwest exists, but it is a disk cache for browser-like reuse, which is the wrong shape for a crawler with millions of URLs. Skip it.
- Retries: do **not** put reqwest-retry on the fetch path. Retry and backoff must be per-host decisions made by the frontier (section 9), not per-request loops. A `Retry-After` from a 429 must push back the whole host.
- Always set: `User-Agent: TravelPlannerBot/1.0 (+https://<our-domain>/bot)`. Also set `From: <bot-contact-email>`, a reasonable `Accept`, connect and total timeouts, a max body size enforced while streaming, and `redirect::Policy::limited(5)`. Re-check robots.txt when a redirect lands on another host.
- Per-host politeness: a `governor` keyed limiter works as an in-process safety net. The real source of truth is `next_fetch_at` in the frontier.
- hyper-util 0.1.21 is the right choice only if you need connection-level control that reqwest hides. That isn't needed here.

## 2. Headless browser control for JS-rendered pages

| Option | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **chromiumoxide** (CDP, async/tokio) | **0.9.1** | 2026-02-25 | 5.9M / 2.4M | Active-ish: last release 9 months ago, last push 2026-04 |
| chromey (spider-rs fork of chromiumoxide) | 2.58.2 | 2026-09-12 | 350K / 49K | Active, but it is spider's fork and carries spider's stealth/firewall features |
| spider_chrome (older name of that fork) | 2.37.130 | 2026-03-25 | 1.2M / 100K | Superseded by chromey |
| headless_chrome (CDP, sync, launches Chrome) | 1.0.22 | 2026-06-11 | 3.6M / 0.7M | Active, but the API is blocking |
| fantoccini (WebDriver, async) | 0.22.2 | 2026-11-15 | 4.2M / 0.57M | Active (jonhoo) |
| thirtyfour (WebDriver, async) | 0.38.3 | 2026-11-14 | 2.4M / 0.7M | Active |
| playwright-rs (drives the Playwright driver) | 0.21.0 | 2026-11-14 | 272K / 172K | Active, young |
| playwright (octaltree) | 0.0.20 | 2022-08-20 | 115K | **Dead**, do not use |

**Recommendation: chromiumoxide 0.9.1 over CDP against a separate headless-shell container. Keep a Playwright sidecar as plan B.** Why: chromiumoxide is async/tokio-native (it fits the reqwest/axum runtime), speaks the Chrome DevTools Protocol directly, and can connect to an already-running browser with `Browser::connect("http://chrome:9222")`. That lets the browser live in its own container with its own memory cap.

- CDP and Chrome compatibility: chromiumoxide generates its protocol bindings from a pinned CDP revision. Chrome keeps old CDP domains stable, so a newer Chrome normally works. Pin the Chrome image tag and bump it deliberately. Watch chromiumoxide's release cadence: if it stalls further, the drop-in is **chromey**, with its stealth features left off. Never enable chrome_stealth, spoof or ua_generator-style features in any fork.
- headless_chrome uses a blocking API (threads), which is awkward with tokio. Usable, but not preferred.
- fantoccini/thirtyfour (WebDriver) need a chromedriver next to Chrome (more moving parts) and give less network-level visibility than CDP. Network-idle detection is harder without CDP `Network.*` events (WebDriver BiDi is improving but is not needed here).
- **Playwright sidecar** (Node or Python, `microsoft/playwright`, very active): the most mature `waitForLoadState('networkidle')` and auto-wait, plus `route()` for request blocking. It costs a second language runtime and about 150–300 MB of RAM. Use it only if chromiumoxide rendering proves flaky. playwright-rs exists but is young.
- **Docker image:** `chromedp/headless-shell` (MIT; repo pushed 2026-10-29; tags `stable` and `158.0.8127.18` updated 2026-11-12). It is the stripped `headless_shell` build: small (roughly 1/3 of full Chrome) and exposes CDP on 9222. **Pin the version tag, not `latest`.** Run it with `--shm-size=1g` (or `--disable-dev-shm-usage`), a container memory limit, and one or two tabs at a time on an e2-small/e2-medium. Restart the browser every N pages to contain leaks.
- Our crawler identifies itself in the browser too. Set the UA override to the same honest bot UA (`Network.setUserAgentOverride` or `--user-agent=`). Do **not** mask `navigator.webdriver` or any other headless signal.

**Legitimate ways to make rendering more complete (none of them evade detection):**
1. **Render only when needed.** Fetch with reqwest first. Use the browser only when the HTML lacks the content (empty `<main>`, a tiny text/markup ratio, a known SPA shell, or JSON-LD missing when expected). Most travel pages (tourism boards, Wikivoyage-style sites, events listings) serve server-side HTML or JSON-LD. Rendering is about 50–100x the cost of a plain fetch.
2. **Wait for network idle.** Subscribe to CDP `Network.requestWillBeSent` / `loadingFinished` / `loadingFailed`. Consider the page settled when 0 requests have been in flight for 500 ms, with a hard 15–20 s cap. Also wait for `Page.lifecycleEvent` `networkIdle` (`Page.setLifecycleEventsEnabled`).
3. **Auto-scroll for lazy-loading.** Run `window.scrollBy(0, innerHeight)` in steps (e.g. every 250 ms) until `scrollHeight` stops growing or a cap is hit (e.g. 10 screens). Then wait for network idle again. This fires IntersectionObserver lazy-loads the same way a reader scrolling would.
4. **Expand "read more" / accordions.** Click elements matching a small allow-list of selectors (`[aria-expanded="false"]`, `details:not([open])` → set `open`, buttons whose text matches /read more|show more|see all|mehr|plus/i) inside the main content only. Cap the number of clicks and never submit forms.
5. **Block what we don't need.** Use `Fetch.enable` with patterns, or `Network.setBlockedURLs`, to block images, media, fonts and known analytics/ad hosts. This saves bandwidth and CPU for both sides and is a good-citizen behaviour.
6. **Read the post-render DOM** (`document.documentElement.outerHTML`), plus any `application/ld+json` that was injected at runtime.
7. Respect robots.txt for **sub-resources** too. Simplest approach: let the browser fetch only same-host scripts/XHR needed for content, and block third-party hosts.

## 3. robots.txt (RFC 9309)

| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **texting_robots** | **0.2.2** | 2023-03-29 | 1.2M / 512K | Stable/slow (last push 2024-02); still widely downloaded |
| robotstxt (Folyd, port of Google's C++ parser) | 0.3.0 | 2021-02-13 | 669K / 91K | **Stale** (no commits since 2021) |
| robots_txt / robots-txt | 0.7.0 | 2020-03-31 | 96K | **Dead** |
| robotparser | 0.11.0 | 2022-03-30 | 62K | **Stale** |
| robotstxt-rs | 0.1.4 | 2025-10-01 | 1.6K | Tiny, unproven |

**Recommendation: texting_robots 0.2.2, vendored or wrapped behind our own trait, plus our own RFC 9309 fetch semantics.** Why: it was written by Stephen Merity (ex-Common Crawl), is tested against Google's robots.txt test suite and Moz's reppy cases, is fuzzed, and handles `Allow`/`Disallow` longest-match, `*` and `$` wildcards, `Crawl-delay` and `Sitemap:`. It is "done" more than "dead", and the parsing problem is small and stable. The folyd `robotstxt` crate is a faithful Google port, but it hasn't been touched since 2021.

The parser alone is not enough. Our code must implement the RFC 9309 **fetch** rules:
- 2xx → parse. Use only the first 500 KiB (RFC 9309 says crawlers MUST parse at least 500 KiB).
- 3xx → follow up to 5 redirects.
- **4xx (incl. 401/403/404) → allow all** (RFC 9309 §2.3.1.3).
- **5xx or network error → assume complete disallow** (§2.3.1.4). Keep using a cached copy if we have one. If the site stays unreachable for 30+ days, the RFC allows treating robots.txt as unavailable. The conservative choice for us is to keep disallowing.
- Cache for up to 24 h (RFC: SHOULD NOT use a cached copy longer than 24 h unless unreachable).
- Match on our product token (`TravelPlannerBot`), falling back to `*`. Honour `Crawl-delay` as a floor on the per-host delay, even though it isn't in the RFC.

## 4. Sitemaps, feeds, HTML, extraction, URLs, language

### Sitemaps
| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| sitemap (svmk) | 0.4.1 | 2020-11-03 | 627K / 43K | **Stale** (reader + writer, quick-xml-based, last push 2023) |
| sitemapo | 0.3.0 | 2026-10-21 | 2.9K | Active but tiny (supports txt + xml + index) |
| sitemap-rs | 0.7.1 | 2026-11-13 | 125K | Active, but **writer only** |
| **quick-xml** | **0.42.0** | 2026-08-22 | 551M / 178M | Active |

**Recommendation: write our own streaming sitemap reader on quick-xml 0.42.0 (about 150 lines), plus `async-compression` (gzip) for `.xml.gz`.** Why: the sitemap protocol is tiny (`<urlset>/<url>/<loc>/<lastmod>/<changefreq>/<priority>`, `<sitemapindex>/<sitemap>/<loc>`, plus plain-text sitemaps). Neither mature reader crate is maintained. A streaming reader also lets us enforce the protocol caps: 50,000 URLs and 50 MB uncompressed per file. Use `<lastmod>` to schedule re-crawls. Discover sitemaps from robots.txt `Sitemap:` lines and `/sitemap.xml`. sitemapo is a reasonable thing to evaluate, but it is very young.

### RSS/Atom
- **feed-rs 3.0.0** (2026-09-27, 2.7M / 672K, MIT): **Active**. It parses RSS 0.9x/1.0/2.0, Atom and JSON Feed into one model. It is the de-facto choice and handy for events and news feeds from tourism boards.

### HTML parsing
| Crate | Latest | Released | Downloads (total / 90d) | Use |
|---|---|---|---|---|
| **scraper** | **0.29.0** | 2026-10-21 | 37M / 11M | DOM + CSS selectors (html5ever). Default for extraction |
| **lol_html** (Cloudflare) | **3.0.3** | 2026-10-28 | 7.5M / 2.5M | Streaming rewriter/selector, low memory. Good for link extraction and JSON-LD grabbing on huge pages without building a DOM |
| html5ever | 0.40.1 | 2026-09-14 | 152M / 59M | Underlying parser |
| encoding_rs 0.8.44 + chardetng 1.0.0 | | 2026-10 / 2026-03 | | Charset decoding for non-UTF-8 pages (header → meta → chardetng) |

**Recommendation: scraper 0.29.0 for per-page extraction, lol_html 3.0.3 for the hot "extract links + ld+json" pass.**

### Main-content extraction (readability ports)
| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **dom_smoothie** | **0.19.0** | 2026-10-27 | 997K / 662K | **Active** (repo pushed 2026-11-09). Close port of Mozilla Readability.js, also has text/markdown output |
| readability (kumabook) | 0.3.0 | 2023-12-20 | 1.7M / 347K | **Stale** (old algorithm port, last push 2024-04) |
| llm_readability (spider-rs) | 0.0.17 | 2026-04-30 | 97K / 13K | Pre-1.0, part of the spider ecosystem, low adoption |
| htmd (HTML→Markdown) | 0.5.6 | 2026-10-15 | 11M / 8.4M | Active. Useful to make compact LLM input |
| fast_html2md | 0.0.65 | 2026-11-13 | 426K | spider-rs, pre-1.0 |

**Recommendation: dom_smoothie 0.19.0 (then htmd 0.5.6 if we want markdown for Gemini).** Why: it is the most faithful, actively maintained Readability port. It returns title, byline, excerpt and clean content, which keeps the Gemini input small (cost lever).

### schema.org JSON-LD / microdata
| Crate | Latest | Released | Status |
|---|---|---|---|
| json-ld (spruceid) | 0.22.0 | 2026-10-30 | Active, but a full JSON-LD 1.1 processor (expansion/compaction, remote contexts). Heavy, and it would fetch `@context` URLs. **Not needed** |
| microdata | 0.1.1 | 2025-05-31 | ~1.4K downloads. Effectively unused |
| schema_org_types | 0.3.1 | 2023-11-27 | **Stale** |

**Recommendation: no dedicated crate.** Pull `<script type="application/ld+json">` blocks with scraper/lol_html, parse them with `serde_json`, flatten `@graph` and arrays, and match on `@type`. The types we care about are `TouristAttraction`, `Event`, `Hotel`/`LodgingBusiness`, `Restaurant`, `Place`, `Offer`, `PostalAddress`, `GeoCoordinates`, `OpeningHoursSpecification`, `AggregateRating`. Map them into our own serde structs and treat every field as optional. For microdata (`itemscope`/`itemprop`), write about 100 lines over scraper. Never dereference `@context` URLs, because that would mean extra fetches to schema.org.

### URL normalization
- **url 2.5.8** (2026-01-05, 1.07B downloads, servo): **Active**. It is a WHATWG URL parser and already lowercases the scheme and host, does IDNA/punycode, strips default ports and normalizes percent-encoding and dot-segments.
- On top, write our own `canonicalize()`: drop the fragment; sort query parameters; drop tracking params (`utm_*`, `gclid`, `fbclid`, `mc_eid`, `ref`, session ids); optionally strip trailing `index.html`. Keep the trailing slash as-is, because it is semantically significant on some servers. Prefer `<link rel=canonical>` when it is same-site. Use **psl 2.1.255** (2026-11-15, data refreshed continuously) for the registrable domain, which gives the politeness key "per registrable domain/IP" and same-site checks. url-normalize (0.1.1) and urlnorm (0.1.6) are tiny. Do not depend on them.
- Dedup: hash the canonical URL (xxh3/blake3) as the frontier primary key. Add a SimHash of extracted text to drop near-duplicate pages.

### Language detection
| Crate | Latest | Released | Downloads (total / 90d) | Notes |
|---|---|---|---|---|
| **whatlang** | **0.18.0** | 2025-10-16 | 4.9M / 2.1M | Trigram-based, ~70 languages, tiny and very fast. Active enough |
| lingua | 1.8.0 | 2026-03-09 | 2.9M / 972K | Most accurate (incl. short text), 75 languages, but models are large (100s of MB with all languages, can load lazily per language) |

**Recommendation: whatlang 0.18.0 first.** Why: it costs almost nothing on a small VM. Also trust `<html lang>` and `Content-Language` when present. Use lingua 1.8.0 only with a restricted language set (e.g. our 7 UI languages plus a few more) if whatlang's accuracy on short snippets proves insufficient.

## 5. Existing Rust crawler frameworks: build on one or compose?

**spider (spider-rs)**: latest **2.53.9** (2026-09-05). 2.1M downloads total, but only 56K in the last 90 days. MIT. 2.8K GitHub stars, repo pushed 2026-11-06. Very fast release cadence (thousands of versions), maintained mostly by one company (Spider Cloud).

Features (from its Cargo.toml): crawl/scrape engine, sitemap, robots cache, `etag_cache`, `rate_limit`, `auto_throttle`, `adaptive_concurrency`, `priority_frontier`, `warc`, Chrome (via its chromey fork) and WebDriver backends, `smart` (HTTP first, Chrome when needed), cron, disk/S3 cache, OpenAI/Gemini integrations, search-API integrations, and a hosted "Spider Cloud" mode.

**Evasion features it contains, which we would have to disable:**
- `ua_generator` (random/rotating realistic browser UAs) and `real_browser`. **Both are enabled by the default `basic` feature.**
- `spider_fingerprint` is a **non-optional** dependency (browser-fingerprint emulation), so it is always compiled in.
- `chrome_stealth`, `webdriver_stealth`, `spoof`.
- `wreq`/`rquest`-based client features. wreq is a reqwest fork whose purpose is TLS/HTTP2 browser fingerprint impersonation.
- `spider_cloud` (documented as "anti-bot, proxy, API modes") and `socks` proxying.
- The default features also pull in Linux-specific performance features (`io_uring`, `numa`, `splice`, `zero_copy`).

**Verdict: do not build on spider. Compose our own core.** Reasons:
1. The defaults are wrong for us: they turn on UA generation and real-browser emulation, and fingerprinting code is always linked in. We would have to run `default-features = false` and audit every release (it ships very often) to prove the evasion paths stay off. Shipping evasion code at all, even disabled, contradicts our policy and makes the honesty claim harder to defend.
2. Its frontier is in-memory, per crawl. We need a durable, multi-day, multi-site frontier with per-host scheduling, recrawl policy and conditional GET state, which is Postgres-backed (section 9). That is the part we would have to write anyway.
3. The pieces we actually need are small, well-maintained crates (reqwest, texting_robots, quick-xml, scraper, dom_smoothie, chromiumoxide, warc). The custom core (frontier, politeness, fetch loop, extraction pipeline) is maybe 3–5K lines, and we own every policy decision.
4. Small 90-day download count relative to total, plus single-vendor maintenance. That is a supply-chain and API-churn risk.

Reading spider's source for ideas (its `auto_throttle`/`etag_cache` logic) is fine. Other frameworks: `voyager`, `dyer` and `crusty` are all stale or tiny. No other Rust framework is worth adopting.

## 6. Storage

### WARC
| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **warc** (jedireza) | **0.4.0** | 2024-11-27 | 2.5M / 284K | Stable/slow. Last commit 2024-11, 10 open issues. Reads and writes WARC 1.0/1.1 records, with gzip |
| warc-rs | 0.3.3 | 2026-11-10 | 330 | Brand new, unproven |

**Recommendation: warc 0.4.0, wrapped so it can be replaced.** Why: it is the only established crate. WARC writing is simple, and we only need `warcinfo`, `request`, `response`, `revisit` and `metadata` records. If it ever blocks us, writing records by hand is about 200 lines.
- Compression: the standard interoperable choice is per-record gzip (`.warc.gz`). The **zstd** crate **0.14.4** (2026-11-12, 482M downloads) supports the WARC zstd convention (`.warc.zst`, per-record frames with an optional dictionary). It is about 2–3x smaller than gzip at similar speed. Prefer `.warc.zst` for our own archive if we don't need to feed pywb/OpenWayback tooling. Otherwise use `.warc.gz`.
- Do we need WARC at all? It is good practice (reproducible re-extraction without re-crawling, which is also polite). Roll files at ~1 GB, upload to GCS Nearline/Coldline, and keep only parsed records in Postgres.

### Postgres client
| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **sqlx** | **0.9.0** | 2026-05-21 | 190M / 59M | Active |
| tokio-postgres | 0.7.18 | 2026-06-12 | 88M / 28M | Active |
| deadpool-postgres | 0.14.2 | 2026-08-26 | 29M / 9M | Active (pool for tokio-postgres) |
| **pgvector** (pgvector-rust) | **0.4.2** | 2026-05-22 | 22.5M / 6M | Active. Has `sqlx` (>=0.8,<0.10, so sqlx 0.9 is supported), `postgres` and `diesel` features, plus `halfvec`/`sparsevec`/`bit` types |

**Recommendation: sqlx 0.9.0 + pgvector 0.4.2 (feature `sqlx`).** Why: compile-time-checked queries (`query!`, with offline mode for CI via `cargo sqlx prepare`), built-in migrations and a pool, and first-class pgvector types. tokio-postgres is slightly faster with less magic. Choose it only if compile-time query checking is unwanted. Both are fine. For the `FOR UPDATE SKIP LOCKED` frontier, either works.

### GCS client
| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **google-cloud-storage** (official, googleapis/google-cloud-rust) | **1.22.0** | 2026-11-11 | 30M / 12M | **Active, official, GA** |
| gcloud-storage (yoshidan, the former `google-cloud-storage`) | 1.3.0 | 2026-03-19 | 4.8M / 1.8M | Community. Still maintained, but superseded |
| gcloud-sdk (abdolence) | 0.33.3 | 2026-11-03 | 7.7M / 2M | Community, generated gRPC/REST |

**Which is current:** Google announced its official Rust SDK on 2025-09-09 and the google-cloud-rust repo then reached 1.0 (GA) for 140+ services. yoshidan **donated the `google-cloud-storage` crate name to Google**, so `google-cloud-storage` ≥ 1.0 is now the official crate. yoshidan's implementation continues as `gcloud-storage`. Old blog posts that say "google-cloud-storage (yoshidan)" refer to versions ≤ 0.2x.

**Recommendation: official google-cloud-storage 1.22.0.** Why: maintained by Google with ADC built in, and it has resumable uploads with automatic resume and checksumming. A good fit for WARC file uploads.

## 7. Gemini on Vertex AI from Rust

| Crate | Latest | Released | Downloads (total / 90d) | Status |
|---|---|---|---|---|
| **google-cloud-aiplatform-v1** (official) | **1.19.0** | 2026-11-11 | 648K / 349K | Active, GA |
| google-cloud-auth (official) | 1.19.0 | 2026-11-11 | 49M / 19M | Active |
| gcp_auth (djc) | 0.12.7 | 2026-06-22 | 19M / 6.8M | Active, small, well-regarded |

**Option A (recommended): official google-cloud-aiplatform-v1 1.19.0** with cargo feature `prediction-service`.
- `PredictionService::builder().build().await?` uses ADC by default and **defaults to the global endpoint `https://aiplatform.googleapis.com`**, which matches our `GCP_LOCATION=global`. Then call `client.generate_content().set_model("projects/{P}/locations/global/publishers/google/models/gemini-3.5-flash-lite").set_contents([...]).set_generation_config(...).send().await?`. `stream_generate_content()` also exists.
- Why: typed request/response, retries/backoff policies, ADC on GCE (metadata server) with no extra code, and the same release train as the GCS crate. Downside: a large generated crate (compile time), and the protobuf-shaped types are verbose.

**Option B: plain REST with reqwest + an ADC token from google-cloud-auth 1.19.0 (or gcp_auth 0.12.7).**
- Why: tiny dependency footprint, and the JSON body is identical to what our Python backend sends. It is easy to debug and to share fixtures with `backend/tests`. On the VM, ADC resolves through the metadata server. Request the scope `https://www.googleapis.com/auth/cloud-platform`.
- A reasonable choice if compile times matter. Either option is fine. Pick A unless the build-time cost bites.

**REST request shape (structured JSON output):**

```
POST https://aiplatform.googleapis.com/v1/projects/{PROJECT}/locations/global/publishers/google/models/gemini-3.5-flash-lite:generateContent
Authorization: Bearer {ADC access token}
Content-Type: application/json
```
```json
{
  "systemInstruction": { "parts": [{ "text": "Extract travel facts from the page. Return JSON only." }] },
  "contents": [
    { "role": "user", "parts": [{ "text": "<page title>\n<url>\n<readability text, truncated>" }] }
  ],
  "generationConfig": {
    "temperature": 0.1,
    "maxOutputTokens": 2048,
    "responseMimeType": "application/json",
    "responseSchema": {
      "type": "OBJECT",
      "properties": {
        "kind":   { "type": "STRING", "enum": ["attraction", "event", "hotel", "restaurant", "other"] },
        "name":   { "type": "STRING" },
        "city":   { "type": "STRING" },
        "country":{ "type": "STRING" },
        "start_date": { "type": "STRING", "nullable": true },
        "price":  { "type": "OBJECT", "nullable": true,
                    "properties": { "amount": { "type": "NUMBER" }, "currency": { "type": "STRING" } } },
        "facts":  { "type": "ARRAY", "items": { "type": "STRING" } }
      },
      "required": ["kind", "name", "country"],
      "propertyOrdering": ["kind", "name", "city", "country", "start_date", "price", "facts"]
    },
    "thinkingConfig": { "thinkingLevel": "MINIMAL" }
  }
}
```
- `responseSchema` uses the OpenAPI 3.0 subset (`type` in upper case, `nullable`, `enum`, `required`, `propertyOrdering`). Gemini 2.5+ also accepts `responseJsonSchema` (standard JSON Schema). Send only one of the two. In Rust, `schemars` 1.2.3 can generate a JSON Schema from our structs for `responseJsonSchema`.
- The answer is in `candidates[0].content.parts[0].text` (a JSON string). Parse it with serde. `usageMetadata` gives prompt, candidate and thoughts token counts, so log them like the Python app's `llm_usage`.
- Handle 429 `RESOURCE_EXHAUSTED` with backoff. Treat a 403/429 that mentions billing or budget as "spend cap hit" and pause the AI stage, mirroring `core/llm.py`.

**Model and price (official Vertex AI pricing page, 2026-11-15):**
- Model id **`gemini-3.5-flash-lite`**. Its model page lists `global`, `us`, `eu` (and `asia-south1`). Thinking levels are `MINIMAL` (default), `LOW`, `MEDIUM`, `HIGH`. Structured output is supported.
- **Standard, global endpoint: $0.30 / 1M input tokens (text/image/video/audio), $2.50 / 1M output tokens (incl. reasoning), $0.03 / 1M cached input.** Non-global (regional) endpoints are 10% more: $0.33 / $2.75 / $0.033. The same rate applies above 200K tokens.
- **Flex/Batch: $0.15 input / $1.25 output** (global). Priority: $0.54 / $4.50.
- For comparison: `gemini-3.1-flash-lite` global is **$0.25 / $1.50** (batch $0.125 / $0.75). The page still lists Gemini 2.5 Flash-Lite at $0.10 / $0.40, but it is the older generation, so check its retirement date before relying on it.
- **Budget note:** the crawler's extraction is offline and not latency-sensitive, so **use Batch/Flex (50% off)**. Example: 1M pages/month × (2K input + 300 output tokens) = 2B in + 0.3B out. That is 2,000 × $0.15 + 300 × $1.25 = $300 + $375 = **$675/month on batch, which blows the $100 budget.** The LLM stage must therefore be selective. Send only pages whose JSON-LD is missing or incomplete, run dom_smoothie first, and cap input at about 1.5K tokens. Or run `gemini-3.1-flash-lite` batch, whose output is 40% cheaper. Roughly 100–150K LLM-processed pages/month fits about $60–90.

## 8. Read API, observability, config, testing

| Purpose | Crate | Latest | Released | Status / why |
|---|---|---|---|---|
| Web server | **axum** | **0.8.9** | 2026-04-14 | Active (tokio-rs). The standard |
| Middleware | tower-http | 0.7.1 | 2026-08-31 | Compression, CORS, timeouts, request ids, tracing layer |
| Tracing | **tracing** | **0.1.44** | 2025-12-18 | Active. De facto |
| | tracing-subscriber | 0.3.23 | 2026-03-13 | `fmt().json()` layer |
| OTel API/SDK | opentelemetry / opentelemetry_sdk | 0.33.1 | 2026-10-09 | Active, still 0.x (expect breaking changes on bumps) |
| OTel export | opentelemetry-otlp | 0.33.1 | 2026-10-09 | OTLP gRPC/HTTP |
| Bridge | tracing-opentelemetry | 0.34.0 | 2026-09-23 | Active (pairs with otel 0.33) |
| GCP trace | opentelemetry-gcloud-trace | 0.31.0 | 2026-11-14 | Community. Direct export to Cloud Trace, lags otel by 2 minors |
| GCP log format | tracing-stackdriver | 0.10.0 | 2024-03-11 | **Stale**. A hand-rolled JSON field mapping is easier |
| Config | **figment** | **0.10.19** | 2024-05-17 | Stable/slow (no release in 2.5 years, repo pushed 2026-10) |
| Config (alt) | config | 0.15.27 | 2026-09-30 | Active |
| HTTP mocks | **wiremock** | **0.6.5** | 2025-08-24 | Active-enough, de facto |
| Containers | **testcontainers** | **0.28.0** | 2026-08-06 | Active |
| | testcontainers-modules | 0.15.0 | 2026-02-21 | Has the `postgres` module. Use image `pgvector/pgvector:pg17` (or pg18) via `.with_name()` |
| Snapshots | **insta** | **1.49.0** | 2026-10-03 | Active (mitsuhiko). Ideal for extraction golden tests |

**Observability on a small GCP VM, the cheap path:**
- Logs: `tracing-subscriber` JSON to stdout with fields named the way Cloud Logging expects (`severity`, `message`, `logging.googleapis.com/trace`). Ship them with the **Ops Agent** (or Docker's `gcplogs` driver). That avoids an OTel log pipeline. Cloud Logging includes 50 GiB/project/month free, so keep crawl logs at INFO with sampling.
- Metrics: either (a) OTLP to the Ops Agent's OTLP receiver, which forwards to Cloud Monitoring (custom metrics are billed per MiB past the free allotment, so keep cardinality low: **no per-URL or per-host labels**, only per-status/per-stage), or (b) the `metrics` + `metrics-exporter-prometheus` crates with Managed Prometheus scraping. Option (a) keeps one SDK.
- Traces: optional. If wanted, sample at 1% via OTLP → Cloud Trace.

**Config:** figment 0.10.19 (TOML file + `APP_` env overrides, with good error messages about where a value came from). It still works fine despite no release since 2024. If a recently released crate is required, use `config` 0.15.27.

**Testing:** wiremock for robots.txt/sitemap/429/Retry-After/304 behaviour. testcontainers with a real pgvector Postgres for the frontier (SKIP LOCKED semantics need a real DB). insta snapshots of extraction output over committed HTML fixtures. Also, a fake "challenge page" fixture to prove the crawler backs off.

## 9. Frontier / queue design

**Recommendation: Postgres-only frontier (no Redis) using `FOR UPDATE SKIP LOCKED`, two-level scheduling (host queue + URL queue), and per-host `next_fetch_at` with adaptive delay.** Why: at our scale (one or two VMs, tens of fetches per second at most), Postgres comfortably handles the throughput. It gives transactional state (crawl result + next schedule + robots cache in one commit) and survives restarts. A separate Redis would cost RAM and money and adds a second source of truth. Queue crates such as pgmq 0.33.7, apalis 1.2.0 and sqlxmq 0.6.0 are job-queue shaped and lack per-host politeness, so **hand-write the frontier**.

**Schema sketch:**
```sql
create table hosts (
  host            text primary key,          -- registrable domain or exact host (psl)
  next_fetch_at   timestamptz not null default now(),
  delay_ms        int  not null default 5000, -- current adaptive delay
  min_delay_ms    int  not null default 2000, -- floor; raised by robots Crawl-delay
  robots_body     bytea, robots_fetched_at timestamptz, robots_status int,
  backoff_until   timestamptz,               -- set on 429/503/challenge
  consecutive_err int not null default 0,
  ip              inet,                      -- optional: also throttle per IP
  leased_by       text, lease_until timestamptz
);
create table urls (
  url_hash  bytea primary key, url text not null, host text not null references hosts,
  priority  real not null default 0.5, depth smallint,
  next_crawl_at timestamptz not null default now(),
  etag text, last_modified text, content_hash bytea, last_status int,
  fetched_at timestamptz, change_rate real      -- for adaptive recrawl
);
create index on urls (host, next_crawl_at, priority desc);
create index on hosts (next_fetch_at) where backoff_until is null;
```

**Claim loop (one round trip per host lease):**
```sql
with h as (
  select host from hosts
  where next_fetch_at <= now()
    and (backoff_until is null or backoff_until <= now())
    and (lease_until is null or lease_until < now())
  order by next_fetch_at
  limit 32
  for update skip locked)
update hosts set leased_by = $worker, lease_until = now() + interval '2 minutes'
from h where hosts.host = h.host
returning hosts.host;
```
Then for each leased host: fetch one URL (`select ... from urls where host=$1 and next_crawl_at<=now() order by priority desc, next_crawl_at limit 1`), and in one transaction write the result, set `urls.next_crawl_at`, and set `hosts.next_fetch_at = now() + delay`. Release the lease. **At most one in-flight request per host** (Mercator's rule).

**Adaptive politeness:**
- `delay = max(min_delay, crawl_delay_from_robots, k × last_response_time)` with k ≈ 10 (Mercator/Heritrix "delay factor": wait 10x the time the server took). Clamp to [2 s, 60 s] for small sites.
- **429 / 503:** honour `Retry-After` (seconds or HTTP-date). Otherwise use exponential backoff on the host: `backoff_until = now() + min(2^n × 1 min, 24 h)`, and double `delay_ms`.
- **403, or a detected challenge page** (an interstitial/JS challenge, `cf-mitigated` header, a "verify you are human" title, a known challenge-provider page): **do not retry with a browser or different headers.** Mark the host `backoff_until = now() + 24 h` (then 7 days on repeat) and log it for a human. After N repeats, park the host and stop crawling it until reviewed or the owner opts in.
- 5xx / timeouts: increment `consecutive_err` and back off exponentially. After ~10 in a row, park the host.
- On success: decay `delay_ms` slowly back toward `min_delay` (additive decrease, e.g. −10% per success).
- Recrawl: `next_crawl_at` from sitemap `<lastmod>`/`changefreq` where present. Otherwise adapt to the observed change rate (content_hash changed or not, as in Cho & Garcia-Molina). Conditional GET (`ETag`/`Last-Modified` → 304) makes recrawls cheap for both sides.
- Global safety valves: a max total fetch rate, and a max concurrent hosts (e.g. 50–200 on an e2-small). Rendering sits on a separate, much smaller pool (1–2 tabs).

**Why not Redis/Kafka:** they help at thousands of fetches per second across many machines (IRLbot/BUbiNG scale). At our budget they add RAM and cost without fixing anything. If Postgres ever becomes the bottleneck, the first step is an in-memory per-host ready-queue, refilled in batches from Postgres (BUbiNG's "workbench" idea), not a new datastore.

**Writeups worth reading:**
- Heydon & Najork, *"Mercator: A Scalable, Extensible Web Crawler"*, WWW Journal 1999: front-queues for priority, back-queues one per host, and a heap of host next-fetch times. This is exactly the host/URL split above. Also Najork & Heydon, *"High-Performance Web Crawling"* (2002) and the crawler chapter of Manning/Raghavan/Schütze, *Introduction to Information Retrieval*, ch. 20 (free online), which presents the Mercator frontier clearly.
- Boldi, Marino, Santini, Vigna, *"BUbiNG: Massive Crawling for the Masses"* (WWW 2014 companion; ACM TWEB 2018; arXiv:1601.06919): per-host and per-IP politeness delays, the "workbench" of visit states, and near-duplicate detection. Open source (Java).
- Lee, Leonard, Wang, Loguinov, *"IRLbot: Scaling to 6 Billion Pages and Beyond"* (WWW 2008): budget-based spam/trap control (STAR) and the DRUM disk-based URL-seen structure.
- Cho & Garcia-Molina, *"Effective Page Refresh Policies for Web Crawlers"* (TODS 2003): recrawl scheduling.
- Common Crawl: the monthly crawls ran on Apache Nutch for years, with per-host politeness, robots.txt and WARC output. Their blog and `commoncrawl/nutch` fork describe the setup, and Stephen Merity's texting_robots came out of that work. StormCrawler (Apache incubator) is the streaming design reference, with a URL frontier spout over SQL/OpenSearch and per-host queues.
- RFC 9309 (Robots Exclusion Protocol, Sept 2022) for the robots rules in section 3.
- Postgres queue patterns: the PostgreSQL docs on `SELECT ... FOR UPDATE SKIP LOCKED`, and Brandur Leach's posts on Postgres job queues (the "transactionally staged job drain" and the bloat/vacuum caveats). Keep the `urls` table vacuum-friendly: tune `fillfactor` 70–80 for HOT updates, and use autovacuum settings per table.

## Summary table

| Area | Pick | Version |
|---|---|---|
| HTTP | reqwest (rustls, http2, gzip/br/zstd) | 0.13.5 |
| Browser | chromiumoxide + chromedp/headless-shell (pinned tag, e.g. 158.0.8127.18) | 0.9.1 |
| Browser fallback | Playwright sidecar (Node) | — |
| robots.txt | texting_robots + own RFC 9309 fetch rules | 0.2.2 |
| Sitemaps | own reader on quick-xml + async-compression | 0.42.0 / 0.4.52 |
| Feeds | feed-rs | 3.0.0 |
| HTML | scraper / lol_html | 0.29.0 / 3.0.3 |
| Main content | dom_smoothie (+ htmd) | 0.19.0 (0.5.6) |
| JSON-LD | serde_json over ld+json script blocks (no crate) | — |
| URL | url + psl + own canonicalize | 2.5.8 / 2.1.255 |
| Language | whatlang (lingua if needed) | 0.18.0 (1.8.0) |
| Framework | none (not spider 2.53.9: evasion features on by default) | — |
| WARC | warc + zstd | 0.4.0 / 0.14.4 |
| Postgres | sqlx + pgvector | 0.9.0 / 0.4.2 |
| GCS | google-cloud-storage (official) | 1.22.0 |
| Gemini | google-cloud-aiplatform-v1 (or reqwest + google-cloud-auth) | 1.19.0 |
| Model | gemini-3.5-flash-lite, global: $0.30 in / $2.50 out per 1M (batch $0.15 / $1.25) | — |
| API | axum + tower-http | 0.8.9 / 0.7.1 |
| Observability | tracing + tracing-subscriber + opentelemetry(-otlp) + tracing-opentelemetry | 0.1.44 / 0.3.23 / 0.33.1 / 0.34.0 |
| Config | figment (or config) | 0.10.19 (0.15.27) |
| Tests | wiremock, testcontainers(-modules), insta | 0.6.5, 0.28.0 (0.15.0), 1.49.0 |

Sources: crates.io API (per-crate JSON), GitHub API, docs.rs (google-cloud-aiplatform-v1 PredictionService), Vertex AI pricing page (cloud.google.com/vertex-ai/generative-ai/pricing), Vertex model page for gemini-3.5-flash-lite (docs.cloud.google.com/vertex-ai/generative-ai/docs/models/gemini/3-5-flash-lite), Google Cloud blog "Now available: Rust SDK for Google Cloud" (2025-09-09), google-cloud-storage 1.0.0 README on docs.rs (name donation), spider/Cargo.toml on GitHub, Docker Hub tags for chromedp/headless-shell.
