# Crate: vc-policy

**Purpose.** Pure politeness and compliance decisions: robots.txt (RFC 9309 plus our
stricter 401/403 rule), the UsePolicy matrix, the denylist, the SSRF guard and challenge
detection. Implements `vc_core::policy::Policy`. **No I/O** except `VoyagerPolicy::new`,
which reads the two policy files once at startup.

Path: `crates/vc-policy`. Dependencies: `url`, `psl`, `regex`, `serde_norway`.

## Key types and functions

| Item | What it does |
|---|---|
| `VoyagerPolicy` | the `Policy` implementation; holds the denylist and the sub-resource allowlist |
| `robots::ParsedRobots`, `AgentRules`, `Rule` | parsed robots.txt; group selection (`VoyagerBot`, else `*`, case-insensitive); longest match wins; `*` and `$` patterns |
| `robots::is_allowed(body, agent, path)` | one-shot check |
| `robots::matches`, `normalize` | pattern matching on percent-normalised paths |
| `denylist::DenyMatcher` | host + subdomains, `brand.*` under any public suffix, `path_prefix`, `except_hosts`; IP hosts never match brands |
| `SubresourceAllowlist`, `SubresourceHost` | exact hosts and resource types for renders |
| `ssrf::check(host, ip, port, allowed_ports)` | returns the rule that blocks, or nothing |
| `ssrf::v4_rule`, `v6_rule`, `ip_rule`, `literal_ip` | range checks; parses decimal, octal and hex IPv4 forms |
| `challenge::detect(exchange)` | returns the provider or reason if the response is a bot challenge |

## Rules it encodes

- robots.txt: 2xx parse (first 500 KiB, lines up to 16 KiB); 404/410 allow all; 401, 403 or
  a challenge block the whole key; 5xx or network error disallow all, and after 30 days use
  the last good copy. Crawl-delay is capped at 1 hour. robots.txt itself is always allowed
  unless the key is blocked.
- AI-bot block (GPTBot, ClaudeBot, CCBot, Google-Extended) with `*` allowed: AI reservation.
- A newly disallowed path yields tombstones for its records.
- Challenge signals: provider headers (`cf-mitigated`, `x-amzn-waf-action`, `x-datadome`),
  interstitial markers, CAPTCHA widgets on 401/403/429/503 or short "verify you are human"
  pages. Only the first 256 KiB is scanned. A plain 403 is a block but not a "challenge";
  either way the key parks.
- SSRF: loopback, RFC 1918, link-local and metadata, CGNAT, `0/8`, ULA, IPv4-mapped IPv6
  and other special ranges; ports 80 and 443 only.

## Config

`[policy]`: file paths, robots TTL and size, last-good days, AI-bot tokens, allowed ports.
See [Configuration](Configuration).

## Tests

- `tests/robots.rs`: Google's robots test corpus, status cases (`robots_401_403_blocks_key`,
  `robots_5xx_disallows`, `robots_5xx_uses_last_good_after_30_days_only`, `robots_404_allows`),
  `ai_bot_block_sets_ai_reservation`, **`robots_refresh_tombstones`**, group precedence,
  wildcards, BOM and line endings, truncation, a proptest that the parser never panics.
- `tests/ssrf.rs`: property tests over every blocked range and port, metadata server by IP
  and name, encodings.
- `tests/denylist.rs`: subdomains, brand wildcards, path prefixes, `except_hosts`, invalid
  files, missing files fail startup.
- `tests/challenge.rs`: provider fixtures (`testkit/fixtures/challenges/`), normal pages,
  plain 403, markers beyond the scan window.
- Fuzz target `fuzz/fuzz_targets/fuzz_robots.rs`.

## Limits

Decisions only: it never fetches. robots.txt bodies come from the frontier's cache; the
fetcher stores them.
