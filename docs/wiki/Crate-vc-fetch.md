# Crate: vc-fetch

**Purpose.** `ReqwestPermitClient`: the **only** path for bytes from a crawl target. It
implements `vc_core::fetch::PermitClient` and refuses any request without a live permit.

Path: `crates/vc-fetch`. Dependencies: `reqwest` (no proxies, no cookies, no automatic
redirects or decompression), `hickory-resolver`, `async-compression`, `flate2`. Status:
wired; the fetcher role builds it in `wiring::fetch`.

## Per-hop checks, in order

For the requested URL and each redirect hop:

1. **Scope.** The URL must be covered by the permit (its key or the source's extra hosts) or,
   for render sub-requests, be on the sub-resource allowlist. Denylisted hosts and private IP
   literals are refused first, so they are never handed back for enqueueing. A first URL out
   of scope is `OutOfScope`; a redirect out of scope is `CrossKeyRedirect` and is **never
   followed** (the caller enqueues it).
2. **robots.txt** from the frontier cache; missing or stale is `RobotsUnavailable` (fail
   closed). robots.txt requests themselves skip this step.
3. **DNS once per hop.** Every resolved address goes through `Policy::ssrf_check`; the first
   good one is pinned for the connection.
4. **Pacing:** wait until the last request's end + the permit's delay. Render sub-requests
   (`RequestPurpose::SubResource`) wait the **sub-resource gap** instead: default 250 ms,
   set by `[render] subresource_gap_ms`, never more than the permit delay. The page delay
   still applies between pages.
5. **Lease check + global rate token.**
6. **The request**: the one `USER_AGENT`, timeouts `min(own timeout, deadline − now)`.

**Credentials on redirects.** An `Authorization` header is sent only to the scheme, host and
port of the requested URL. Any hop that changes one of them drops it, even inside one
politeness key. An API call (`RequestPurpose::Api`) redirected from `https` to `http` is
refused (`OutOfScope`), so tokens never travel in clear text.

## Responses

- 401, 403 and anything `Policy::is_challenge` flags: `FetchError::Challenge` (park; never
  retried).
- Other error statuses are normal exchanges (the caller decides).
- `Retry-After` is parsed (seconds or HTTP date) by `retry_after`.
- Bodies are decoded (gzip, deflate) under a decoded-byte cap (HTML 5 MB, other 10 MB);
  nested `.gz` payloads are probed up to `max_nested_gzip` layers under the same cap.
- The content-type allowlist applies to 2xx responses.
- robots.txt bodies are truncated at 500 KiB instead of failing.
- Conditional GET: a 304 is returned as such, so the caller writes a revisit.

**Streams** (`PermitClient::stream`, for dumps) hold the permit's `DeadlineHandle`: they
check the current deadline before every chunk, see a `Frontier::extend` made after the
stream opened, and record when the body ended for pacing. The handle cannot move the
deadline or make requests.

## Key items

`ReqwestPermitClient`, the `Resolve` trait with `HickoryResolver` (production) and
`StaticResolver` (tests), `parse_retry_after`, `retry_after`, `ROBOTS_MAX_BYTES`.

## Config

`[fetch]`: timeouts, redirects, byte caps, nested gzip, content types, daily raw-bytes cap.
See [Configuration](Configuration).

## Tests

`tests/fetch.rs` (against a local server and a real permit): `conditional_get_304`,
`gzip_bomb_capped`, `nested_gz_capped`, **`redirect_cross_key_enqueued`**,
`redirect_same_key_followed`, **`redirect_to_private_ip_blocked`**,
`first_hop_private_ip_blocked_before_connect`, `redirect_disallowed_path_stopped`,
`too_many_redirects`, `timeout_bounded_by_deadline`, `deadline_passed_is_not_a_request`,
`ua_is_exact_constant`, `lost_lease_aborts_before_request`, `challenge_and_403_detected`,
`content_type_not_allowed`, `robots_missing_fails_closed`, `out_of_scope_and_denylisted`,
`subresource_on_allowlisted_host`, `paced_by_permit_delay`,
`subresources_paced_by_gap_not_page_delay`,
`subresource_gap_is_configurable_and_never_exceeds_delay`, `stream_yields_decoded_and_caps`,
`stream_sees_extend_after_open`, `authorization_dropped_on_port_change`,
`authorization_kept_on_same_origin_redirect`, `host_with_public_and_private_addresses_refused`.
Unit tests: `authorization_dropped_on_scheme_change`, `api_https_to_http_downgrade_refused`.

`tests/ssrf_voyager_policy.rs` runs the client with the **real** `VoyagerPolicy` and policy
files (not a fake): `voyager_policy_refuses_private_resolutions_end_to_end` (private,
loopback, metadata and mixed resolutions refused before any connection) and
`voyager_policy_refuses_disallowed_port`.

## Limits

At most 5 redirects; ports 80/443; no cookies; HTTP only through permits.
