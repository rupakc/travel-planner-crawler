# Crate: vc-fetch

**Purpose.** `ReqwestPermitClient`: the **only** path for bytes from a crawl target. It
implements `vc_core::fetch::PermitClient` and refuses any request without a live permit.

Path: `crates/vc-fetch`. Dependencies: `reqwest` (no proxies, no cookies, no automatic
redirects or decompression), `hickory-resolver`, `async-compression`, `flate2`.

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
4. **Pacing:** wait until the last request's end + the permit's delay.
5. **Lease check + global rate token.**
6. **The request**: the one `USER_AGENT`, timeouts `min(own timeout, deadline − now)`.

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

**Streams** (`PermitClient::stream`, for dumps) check the deadline before every chunk.
Callers must `Frontier::extend` before streaming a long dump.

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
`subresource_on_allowlisted_host`, `paced_by_permit_delay`, `stream_yields_decoded_and_caps`.

## Limits

At most 5 redirects; ports 80/443; no cookies; HTTP only through permits.
