# Politeness and Robots

This crawler touches other people's servers. The rules on this page come **before**
everything else: breaking one is a failure even if the code "works". The public, site-owner
view of the same rules is in [BOT.md](https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md).

## Identity: one name, no disguises

Every request, from plain HTTP and from Chromium alike, carries exactly this User-Agent
(`vc_core::USER_AGENT`):

```
VoyagerBot/<version> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)
```

- The robots.txt token is `VoyagerBot`.
- The crawler uses one static external IP in Google Cloud us-east1. It will be published in
  BOT.md before the first crawl.
- There is no User-Agent, header or TLS-fingerprint rotation, no proxies, no stealth plugins,
  and no CAPTCHA solving. `deny.toml` bans the known evasion crates (`spider`,
  `spider_fingerprint`, `spider_chrome`, `chromey`, `ua_generator`, `fake_user_agent`,
  `chrome_stealth`, `wreq`, `rquest` and others), so CI fails if one is ever added.

## The gate: no permit, no request

A request to a crawl target is only possible with a **`ClaimPermit`**.

- Only `Frontier::claim` creates one, from a committed row in the `claims` table. The
  constructor is private to `vc-frontier`; `clippy.toml` forbids `ClaimPermit::mint` and
  `ClaimPermit::extend_to` everywhere else.
- Before **every** request, one statement checks the lease (epoch still current, database
  `now()` before the deadline) and takes a token from the global rate bucket. A lost lease
  stops the request (fail closed).
- This covers pages, robots.txt, sitemaps, feeds, API adapters, dumps, render sub-requests,
  and later `admin source validate` and the source scout.

## Keys

| Key | What it is | What it owns |
|---|---|---|
| **Origin** | scheme + host + port, such as `https://www.example.com` | its robots.txt |
| **Politeness key** | by default the registrable domain from the Public Suffix List, such as `example.com`; a source may group several domains | delay, one request in flight, daily budget, parking |
| **Shard** | a number on each key | which fetcher process may claim it (for scaling later) |

## robots.txt

The crawler follows RFC 9309, with stricter rules where the RFC leaves room.

| robots.txt answer | What we do |
|---|---|
| 2xx | Parse the first 500 KiB. Use the `VoyagerBot` group if there is one, otherwise `*`. Longest match wins. |
| 404 or 410 | Allow everything. |
| **401, 403, or a bot-challenge page** | **Do not crawl the whole key.** (Stricter than the RFC.) |
| 5xx or network error | Disallow everything, retry with back-off. After 30 days of errors, use the last good copy. |

- robots.txt is cached for up to 24 hours per origin, in the `origins` table.
- A missing or stale cache entry fails closed: the request is not made, and a refresh is
  queued.
- **AI reservation:** if robots.txt blocks the major AI crawlers (`GPTBot`, `ClaudeBot`,
  `CCBot`, `Google-Extended`) but allows `*`, we may crawl, but the text never goes to a
  language model or into embeddings.
- When robots.txt newly disallows a path, records built from that path are **tombstoned**
  (removed from the API and index).
- Every redirect hop is checked against robots.txt and the scope. A redirect to **another
  key is never followed inline**; it is enqueued as a new URL under its own key.

## Delay, concurrency and budgets

```
delay per key = max(robots Crawl-delay, source min_delay_s (at least 5 s), 10 × EWMA latency, penalty)
                capped at 1 hour
```

- **EWMA latency** is a running average of the site's response time (smoothing 0.3). A slow
  site automatically gets a longer delay.
- At most **one request in flight per key**, including render sub-requests.
- Each key has a **daily request budget** (default 3,000; a source can only lower it).
- A **global rate** of 4 requests per second holds across all processes, as a token bucket
  row in Postgres.
- A **daily raw-bytes cap** of 3 GB holds across all keys.

## Back-off and parking

| Answer | Reaction |
|---|---|
| 429 or 503 | Honour `Retry-After`; otherwise double the wait, up to 24 hours. |
| 401, 403, or a challenge page | **Park the key**: 24 h, then 48 h, and so on, up to 30 days. Never retried with a browser or another identity. |
| 5 server errors in a row | Park for 1 hour. |

Parks are counted in metrics and in a daily digest, not sent as one alert each. An alert
fires only if more than 25 % of active keys are parked.

**Challenge detection** (`vc-policy`) looks at the first 256 KiB of the body and the headers:

1. headers such as `cf-mitigated: challenge` (Cloudflare), `x-amzn-waf-action: captcha`
   (AWS WAF), `x-datadome` on a 4xx;
2. interstitial markers of known bot-management providers, whatever the status;
3. a CAPTCHA widget on a 401/403/429/503, or on a short page whose title asks to verify the
   visitor. A CAPTCHA on a normal long page (a newsletter form) is not a challenge.

## Page-level signals: the UsePolicy matrix

Opt-out signals are turned into a **UsePolicy**: what we may do with a capture and the
records from it. Signals can only make it stricter (the "meet" of all signals).

| Signal | Store raw | Index and serve | LLM extract | Embed | Serve to Voyager |
|---|---|---|---|---|---|
| none | yes | yes | yes | yes | yes |
| `noindex` (meta or `X-Robots-Tag`) | yes | no | yes | no | facts only |
| `noarchive` | change detection only, purged after 30 days | yes | yes | yes | yes, no cached copy |
| TDM reservation, `Content-Signal: ai-input=no`, or AI-bot block in robots.txt | yes | title and link only | no | no | link only |
| `ai-train=no` | recorded; we never train models anyway | | | | |

`noarchive` and TDM-reserved captures are stored under the `ephemeral/` prefix in GCS and
deleted after 30 days.

## Never-fetch list

`policy/denylist.yaml` lists hosts we never request, with a reason and a source for each
(about 40 entries). Examples: Booking, Expedia, Tripadvisor, Skyscanner, Google Flights and
Travel, Airbnb, Kayak, Reddit, GetYourGuide, Viator, FlyerTalk.

- A host entry also covers its subdomains.
- `brand.*` covers the brand under any public suffix (`expedia.*` covers `expedia.co.uk`).
- `path_prefix` limits an entry to some paths; `except_hosts` names partner-API hosts that
  are not covered (their sources stay disabled until there is a signed agreement).
- Adding an entry never needs approval. Removing one needs the owner's approval.

## SSRF guard and content limits

**SSRF** (server-side request forgery) means tricking a server into calling addresses it
should not. Every hop is checked:

- DNS is resolved once per hop and the address is pinned for the connection;
- blocked: loopback, RFC 1918 private ranges, 169.254.0.0/16 (link-local and the metadata
  server), 100.64.0.0/10, 0.0.0.0/8, `fc00::/7`, IPv4-mapped IPv6 and other special ranges;
  decimal, octal and hex IP forms are normalised first;
- ports 80 and 443 only; at most 5 redirects.

Content limits: bytes are counted **after** decompression (HTML 5 MB, other 10 MB, nested
gzip up to 2 layers); no XML DTDs or entity expansion; DOM depth and node caps; CPU time per
capture; a content-type allowlist.

## Opt-out commitments

| What | When |
|---|---|
| No more requests to newly disallowed paths | within 24 h of the next robots.txt fetch (cached at most 24 h) |
| Removed from the API and search index | within **24 hours** of a request |
| Removed from raw and text storage in GCS | within **7 days** |
| Gone from database backups and disk snapshots | within **21 days** |

Site owners can opt out with robots.txt, a GitHub issue form, or private reporting. See
[BOT.md](https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md).

Code: [vc-policy](Crate-vc-policy), [vc-frontier](Crate-vc-frontier), [vc-fetch](Crate-vc-fetch).
