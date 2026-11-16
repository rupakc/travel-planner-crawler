# Crate: vc-render

**Purpose.** `ChromiumRenderer` renders JavaScript-only pages in headless Chromium through
**CDP Fetch interception**. Chromium has no network; every request it makes is paused and
answered by the renderer under the page's permit. Implements `vc_core::render::Renderer`.

Path: `crates/vc-render`. Dependency: `chromiumoxide`. The browser runs in the
`chromedp/headless-shell` container on the internal `render` network. Status: wired; the
fetcher role renders URLs whose fetch mode is `render` when `[render] enabled` is true.

## Modules and key items

| Item | What it does |
|---|---|
| `ChromiumRenderer` | the engine: opens a tab, drives the page, decides each paused request, collects exchanges |
| `cdp::CdpBrowser`, `cdp::CdpTab` | traits over the browser, so the decision logic is tested with a mock |
| `ChromiumBrowser` | the production `CdpBrowser` over chromiumoxide: a fresh browser context per render, `Fetch.enable` for every URL pattern, reconnect when the handler stops or after `restart_after_renders` |
| `decide::precheck` | the pure part: method and scheme, SSRF on IP literals, blocked resource types, trackers (`TRACKER_HOSTS`), denylist, permit scope, and for third-party hosts the allowlisted resource types (a filter hook; production passes `VoyagerPolicy::subresource_allowed`) |
| `ThirdPartyBorrow` | optional hook, called before a request to another politeness key, that can hold or refuse it (cross-process pacing). The fetcher sets it with `wiring::fetch::third_party_borrow`, which borrows the key through `PgFrontier::borrow_subresource`, waits up to 12 s for a busy key and releases it when the request ends |
| `decide::reason_for` | maps a `FetchError` to a `BlockReason` |
| `SubresourceCache` | content-addressed (BLAKE3), in memory, TTL 24 h, bounded by bytes; only 200 scripts and stylesheets; looked up after every policy check |
| `budget_from(config)` | builds the `RenderBudget` |

## Flow of one render

1. Open a fresh context and tab; enable Fetch interception.
2. Navigate. For each paused request: `precheck`, robots from the frontier cache (missing or
   stale: fail and queue a refresh), budget and per-key gap, then fetch through the
   `PermitClient` and `fulfillRequest`, or `failRequest` with a `BlockReason`.
3. Wait for network idle (no paused request for 1 s), scroll to the bottom a few times
   (stops when the page stops growing), click allowlisted expand selectors once.
4. Serialise the DOM. Return a `RenderOutcome`: the DOM, every exchange (for WARC records)
   and the blocked list.

Detect and park: a challenge, 401 or 403 on the page's own key aborts with
`FetchError::Challenge` (the caller parks the key). A 429 or 503 on the page's own key, for
the document or a sub-request, aborts with `FetchError::RateLimited` carrying the parsed
`Retry-After`, so the caller backs the key off. From an allowlisted third party, any of these
only fails that sub-request. A lost lease aborts. A cross-key redirect of the document ends
the render and is returned for enqueueing.

Only the page's own document loads as a document. Iframes and JavaScript or meta-refresh
navigations after load are refused (`BlockReason::ResourceType`). Third-party hosts are paced
process-wide: one request at a time per host across every render in the process, at least
`subresource_gap_ms` apart.

The fetcher maps the rendered page's own status through the normal HTTP ladder: 404/410 is
`Gone`, 429/503 rate-limits the key, other 5xx count as server errors, and only 2xx/304 store
the DOM as fetched.

## Config

`[render]`: `enabled` (false), `cdp_url`, `max_tabs` (2), `budget_s` (90),
`max_subrequests` (60), `max_bytes` (20 MiB), `network_idle_ms` (1000),
`subresource_gap_ms` (250), cache TTL and size, `restart_after_renders` (200).

## Tests

- `tests/engine.rs` (mock CDP): `spa_page_rendered`, `subrequest_to_private_ip_blocked`,
  `disallowed_subresource_failed`, `third_party_not_allowlisted_failed`,
  `render_budget_enforced`, `bytes_budget_exhausted_blocks`,
  `all_requests_recorded_as_exchanges`, `blocked_resource_types_failed`,
  `trackers_and_denylist_failed`, `non_get_failed`, `missing_robots_fails_and_enqueues_refresh`,
  `subresource_cache_reused_within_ttl_and_expires`, `document_challenge_aborts`,
  `third_party_403_is_not_a_park`, `lost_lease_aborts_before_any_request`,
  `cross_key_redirect_of_document_returned`, `auto_scroll_loads_lazy_content`,
  `user_agent_is_the_constant`, `subresource_gap_enforced_per_key`,
  `render_bounded_by_lease_deadline`, `subrequest_challenge_in_scope_aborts`,
  `in_scope_subrequest_429_is_fatal_with_retry_after`, `in_scope_503_and_document_429_are_fatal`,
  `third_party_429_only_fails_that_subrequest`, `extra_documents_refused`,
  `document_navigation_after_load_refused`, `allowlisted_host_types_enforced`,
  `third_party_host_serialised_across_concurrent_renders`,
  `third_party_borrow_hook_called_and_can_refuse`.
- `tests/chromium.rs`: the same key cases against a real headless-shell container;
  `fixture_image_matches_compose` keeps the test image equal to the one in
  `deploy/compose.yaml`. Ignored unless `VC_RENDER_IT=1` or `CI` is set.

More: [Rendering with Chromium](Rendering-with-Chromium).
