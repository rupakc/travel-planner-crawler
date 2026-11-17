# VoyagerBot

VoyagerBot is the web crawler of **Voyager**, an open-source (MIT) AI travel planner
([rupakc/travel-planner](https://github.com/rupakc/travel-planner)). It collects public
travel information: official travel advice, open datasets, event listings, and the
headlines of travel blogs and news feeds. Voyager's planning agents use it to cite
current, attributed facts instead of guessing. The crawler's code is public too:
[rupakc/travel-planner-crawler](https://github.com/rupakc/travel-planner-crawler).

It is a small crawler run by one person: a single machine, one request at a time per
site.

**Contact:** `crawler-contact@<domain>` (email), or an issue in this repository. We answer
opt-out and removal requests within 24 hours.

## How to recognise it

Every request carries exactly this User-Agent, for plain HTTP and for the headless
browser alike (`<version>` is the crawler's release):

```
VoyagerBot/<version> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)
```

- **robots.txt token:** `VoyagerBot`
- **Egress IP:** `<egress-ip>`. VoyagerBot crawls only from this one static address in
  Google Cloud (us-east1). It is published here before the first crawl. Requests that claim
  to be VoyagerBot from any other address are not ours: please report them.

We never change our identity: no other User-Agents, no rotating IPs or proxies, no
browser fingerprint tricks, no CAPTCHA solving. VoyagerBot does not log in, submit forms
or try to get past a bot challenge.

## How it behaves

- **robots.txt (RFC 9309).** We read it before crawling a host and cache it for up to
  24 hours. We use the `VoyagerBot` group if there is one, otherwise `*`.
  - A missing robots.txt (404/410) allows crawling.
  - A robots.txt that answers 401, 403 or a bot challenge means **do not crawl** that
    site at all, which is stricter than RFC 9309 requires.
  - A server error (5xx) or network failure means *disallow everything* until we can
    read it again.
- **Rate limits.**
  - At most **one request in flight per site**. Sub-domains of one registrable domain
    count as one site.
  - At least **5 seconds** between requests to the same site, or your `Crawl-delay` if it
    is longer. When your server slows down we slow down too: the gap grows to 10 × your
    recent response time (up to 1 hour).
  - Each site has a **daily request budget** of at most 3,000 requests. Most sites see far
    fewer: each page is recrawled on its own schedule (from hourly feeds to yearly
    datasets), with `If-Modified-Since` / `If-None-Match`, so an unchanged page costs you
    a `304`.
  - A redirect to another site is queued under that site's own limits, never followed
    at once.
- **429 / 503** and `Retry-After` are honoured; otherwise we back off exponentially up to
  24 hours. A **401, 403 or challenge page** makes us stop crawling the site for at least
  24 hours (doubling up to 30 days). We never retry with a different identity.
- **Page-level signals** are honoured: `noindex`, `noarchive`, `X-Robots-Tag`, TDM
  reservations (TDMRep) and `Content-Signal`. A site that reserves text and data mining,
  sends `ai-input=no`, or blocks the major AI crawlers in robots.txt gets title + link
  only: its text is never sent to a language model or embedded. We never train models.
- **What we keep.** Raw responses are stored for change detection and provenance.
  Copyrighted articles (blogs, news) are served to Voyager only as headline, a short
  excerpt (≤ 300 characters), facts and a link back, never the full text. The crawler's
  API is private to Voyager: the data is not resold or republished in bulk.
- **Rendering.** A small number of JavaScript-only pages are rendered in a sandboxed
  headless browser. It has no network of its own: every request it makes goes through
  the same robots, rate and identity rules. Images, fonts, media and trackers are not
  loaded.
- Sites whose terms forbid automated access are on a public
  [denylist](policy/denylist.yaml) and are never requested.

## Opting out

**robots.txt** is the fastest way and needs no contact with us:

```
User-agent: VoyagerBot
Disallow: /
```

To slow us down instead, put a `Crawl-delay` (in seconds) in that group.

You can also allow crawling but keep your content away from language models, either with
a `Content-Signal: ai-input=no` line or a TDM reservation, or by blocking the major AI
crawlers while allowing everyone else:

```
User-agent: GPTBot
User-agent: ClaudeBot
User-agent: CCBot
User-agent: Google-Extended
Disallow: /
```

We treat any of these as an AI reservation and keep only titles and links.

**Or ask us directly**: email `crawler-contact@<domain>`, or use the
[opt-out form](https://github.com/rupakc/travel-planner-crawler/issues/new?template=opt-out.yml).
Either covers whole domains, specific URLs, and removal of data already collected.
Issues are public; for a private request, email us or use
[private reporting](https://github.com/rupakc/travel-planner-crawler/security/advisories/new)
with a title starting "opt-out". When you ask, we:

1. stop requesting your site (or the paths you name) at once;
2. add it to the public denylist, so it stays excluded;
3. delete what we collected on the schedule below, and tell you when it is done.

### Our commitments (opt-out SLA)

| What | When |
|---|---|
| No further requests to disallowed paths | within 24 hours of our next robots.txt fetch (cached ≤ 24 h); at once for a direct request |
| Removed from Voyager's API and search index | within **24 hours** of the request |
| Removed from raw and text storage (archives rewritten, old object versions deleted) | within **7 days** |
| Gone from database backups (14-day WAL) and disk snapshots (7 days) | within **21 days** |

When robots.txt newly disallows a path, records derived from it are withdrawn
automatically. Personal data (for example an author name in a Q&A dump) is removed on
request the same way.

## Questions

Email `crawler-contact@<domain>` or open an issue in this repository. Security reports:
see [SECURITY.md](SECURITY.md).
