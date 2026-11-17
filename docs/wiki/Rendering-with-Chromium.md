# Rendering with Chromium

A few pages show their content only after JavaScript runs. For those, the crawler uses a
headless Chromium. The browser is **isolated**: it has no network of its own, and every
request it wants to make is answered by the crawler under the same rules as any other
request.

## When a page is rendered

Each source sets `render: never | auto | always`.

- `never`: plain HTTP only (most sources).
- `always`: every page of the source is rendered.
- `auto`: the HTML parser flags a page as `needs_render` when it has less than 200
  characters of main text or looks like an empty single-page-app shell. The processor then
  switches that URL to `render` mode and makes it due now (if a fetch of it is running, right
  after it, through `refetch_at`). The render is an ordinary frontier claim, so the key's
  delay, budget, parking and robots.txt all apply. It happens at most once per content
  hash (`url_state.render_requested_hash`, migration 1907) and only for a URL still in
  `http` mode, so a repeated or redelivered capture never re-queues. The URL stays a render
  URL afterwards. The processor does this only when `render.enabled` is on; with rendering
  off the fetcher would just fetch the same shell over HTTP.

Rendering is off by default (`render.enabled = false`), and production does not turn it
on yet ([Configuration](Configuration)).

## Isolation

| Measure | How |
|---|---|
| No network | Chromium runs on a Docker network (`render`) marked `internal`, with no route out. Its only peer is the fetcher. |
| No data access | It cannot reach Postgres, GCS or the metadata server. |
| Sandbox on | Chrome's own sandbox stays on (no `--no-sandbox`), with a seccomp profile (`deploy/chromium/seccomp.json`). |
| Least privilege | Non-root user, read-only root filesystem, all Linux capabilities dropped, `no-new-privileges`. |
| Memory | 1.5 GB limit. |
| Pinned image | `chromedp/headless-shell`, pinned by digest. |
| Fresh state | Each render uses a fresh browser context: no shared cookies, storage or HTTP cache. The CDP session is restarted after 200 renders. |
| No side traffic | `deploy/chromium/run.sh` turns off DNS prefetch, network prediction, prerendering, pings and background services, so Chromium does not even try to send anything outside interception. |
| Nothing to reach | The fetcher's metrics endpoint is on a separate `metrics` network, so Chromium's network has no listener but the CDP connection. |

## CDP Fetch interception

**CDP** (Chrome DevTools Protocol) lets a program drive Chrome. The renderer turns on
`Fetch.enable` for every URL pattern. Chrome then **pauses** every request (document, script,
stylesheet, XHR, fetch) and asks the renderer what to do.

```mermaid
sequenceDiagram
  participant C as Chromium, no network
  participant R as Renderer in the fetcher
  participant DB as Frontier in Postgres
  participant W as Website or allowlisted CDN
  C->>R: Fetch.requestPaused
  R->>R: method, SSRF, resource type, extra document, tracker, denylist, scope
  R->>DB: robots.txt from the origins cache
  R->>R: render budget and sub-resource gap
  opt request to an allowlisted CDN
    R->>DB: borrow the CDN's politeness key
  end
  alt every check passes
    R->>DB: lease check and global rate token
    R->>W: GET as VoyagerBot
    W-->>R: response
    R->>C: Fetch.fulfillRequest
  else a check fails
    R->>C: Fetch.failRequest with a reason
  end
```

For each paused request the renderer:

1. **checks policy:** GET over http(s) only; SSRF; blocked resource types (images, media,
   fonts and anything else that does not build the DOM); **extra documents** (see below);
   known trackers; the denylist; the permit scope (the page's own key, or a host in
   `policy/subresource_allowlist.yaml` **with that resource type** listed for it); robots.txt
   from the frontier's cache. A missing or stale robots entry fails the request and queues
   a refresh: the renderer never fetches robots.txt itself;
2. **fetches through the `PermitClient`**: lease check + global rate token per request, the
   one `VoyagerBot` User-Agent, one request at a time. Sub-requests wait the
   **sub-resource gap** (250 ms) instead of the page delay; the gap is applied inside the
   permit client's `admit` and never exceeds the key's delay. The next page of the claim
   still waits the full delay. A third-party request first borrows that host's key (below);
3. **answers** with `Fetch.fulfillRequest`, or `Fetch.failRequest` with a reason (`ssrf`,
   `denylisted`, `robots_disallowed`, `robots_unavailable`, `out_of_scope`, `resource_type`,
   `tracker`, `budget`, `method`, `fetch_failed`).

### One document per render

Only the page's own document loads as a document. Iframes (same key or third party) and
navigations by script or meta refresh, even back to the page itself, are refused as
`resource_type` and never fetched. Worker and service-worker scripts are refused too.

Every real request becomes a normal exchange, stored as WARC `request`/`response` records
next to the final DOM (`resource` record), in the same spool. Refused requests are listed
in the render outcome.

### Known gap: child targets are not intercepted

`Fetch.enable` applies to **one CDP target**. The renderer (chromiumoxide 0.9.1) attaches to
the page target only; that version cannot auto-attach to child targets and send them their
own `Fetch.enable`. Requests made **inside** a child target are therefore not paused by the
renderer:

- dedicated, shared and service **workers** (a worker's own `fetch()`/XHR/`importScripts`);
- **out-of-process iframes** (OOPIFs: cross-site frames run in their own renderer process).

How this is contained:

| Mitigation | Effect |
|---|---|
| Documents refused | Only the page's own document is fetched. Every other document request (iframes, JS or meta-refresh navigations) is refused, so an OOPIF never gets a document to run. |
| Workers get nothing | A worker script paused at the page passes the same checks as any script (an unknown type is `other` and refused; `blob:`, `data:` and other non-http(s) URLs are refused). A request Chromium makes from the worker's own target is not paused and fails: there is no network (next row). |
| Internal render network | Anything a child target requests on its own has nowhere to go: Chromium sits on the `internal` Docker network `render`, with no route out. |
| Nothing to reach on `render` | The fetcher's `/metrics` and `/healthz` listen on the separate `metrics` network (`VC_OBS_METRICS_ADDR`), never on `render`; the only peer Chromium can see is the fetcher's outgoing CDP connection. |

A future chromiumoxide with `Target.setAutoAttach` support should intercept child targets too
(`Fetch.enable` per attached target) and turn this gap into a test.

## Allowlisted sub-resource hosts

`policy/subresource_allowlist.yaml` lists a few public CDNs (for example
`cdnjs.cloudflare.com`, `cdn.jsdelivr.net`, `unpkg.com`) with the resource types allowed
from each (lower-case CDP types such as `script` or `stylesheet`). A request of a type not
listed for that host is out of scope. Denylisted hosts are refused even if listed. If the
policy files cannot be read, every third-party request is refused. These hosts are seeded
as politeness keys so their robots.txt stays fresh. Each sub-request to them still passes
the full gate and counts against the global rate.

Third-party hosts are paced twice:

| Layer | Scope | Rule |
|---|---|---|
| In-process lock | every tab and renderer in the fetcher process | one request at a time per host, at least the sub-resource gap apart; a wait past the render deadline refuses the request as `budget` |
| Key borrow | every process, through Postgres | the request **borrows** the CDN's own politeness key: granted only if its `next_allowed_at` has passed, it is not parked and no claim is in flight. The key's `next_allowed_at` then moves on by its delay, and again when the request ends. A busy key is asked again every 250 ms for up to 12 s (never past the render deadline), so a second file from the same CDN arrives after the CDN's delay; still busy, that sub-request is skipped. |

The borrow is the renderer's `ThirdPartyBorrow` hook, set by the fetcher with
`wiring::fetch::third_party_borrow` (over `PgFrontier::borrow_subresource`). Cache hits skip
both layers.

## Budgets

| Limit | Default |
|---|---|
| Time per render | 90 s (also bounded by the claim deadline) |
| Sub-requests per render | 60 |
| Decoded bytes per render | 20 MB |
| "Network idle" | no paused request outstanding for 1 s |
| Sub-resource gap (same host) | 250 ms, never more than the key's delay |
| Concurrent tabs | at most 2 |

After network idle, the renderer scrolls to the bottom a few times (stopping when the page
stops growing) and clicks allowlisted "expand" selectors once, then saves the DOM.

## Sub-resource cache

Scripts and stylesheets are cached in memory, content-addressed by BLAKE3, for 24 hours,
shared across renders (max 64 MiB). Most of a site's JS and CSS is therefore fetched about
once a day. The cache is looked up **after** every policy check, so it never bypasses
robots, scope or the denylist. Only 200 responses are cached.

## Blocks and errors

- A challenge, 401 or 403 on the page's **own key** aborts the render and parks the key.
  Nothing is retried in another way.
- A 429 or 503 on the page's own key, on the document or on any sub-request, also ends the
  render. The key backs off like any rate-limited key, honouring `Retry-After`
  ([Politeness and Robots](Politeness-and-Robots)).
- A 403, 429 or 503 from an allowlisted third-party CDN only fails that sub-request; it does
  not park the page's key.
- A lost lease aborts the render before any further request.
- A document that redirects to another key ends the render; the caller enqueues the target.

The rendered page's own status follows the same ladder as a plain fetch:

| Page status | Result |
|---|---|
| 2xx, 304 | DOM stored as fetched |
| 404, 410 | URL gone |
| 429, 503 | key rate-limited (with `Retry-After`), render stops |
| other 5xx | counts toward the key's run of server errors |
| other non-2xx | permanent failure for that URL |

> **Status:** the fetcher renders URLs queued in `render` mode (the seeds of `always`
> sources, links and redirects found from them, and `auto` pages the parser flagged
> `needs_render`), with the third-party borrow above. Tested with a mock CDP and against the
> pinned headless-shell image; the `auto` re-queue in `crates/vcrawl/tests/render_requeue.rs`.
> Rendering is still off in production (`render.enabled = false`).

Code: [vc-render](Crate-vc-render).
