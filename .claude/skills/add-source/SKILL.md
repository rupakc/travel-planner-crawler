---
name: add-source
description: Add or reclassify a VoyagerBot crawl source - vet robots.txt, terms and the denylist, choose licence and source class, write sources/<id>.yaml against schemas/source.schema.json, and test it. Use when asked to add a site, feed, API or dump to the crawler.
---

# Add a crawl source

Spec: §5.9 (licences), §5.12 (extending), §17.8 (classes). Never evade: a bot wall means "do not crawl".

## 1. Is it allowed at all?

1. Denylist: `grep -n '<domain-or-brand>' policy/denylist.yaml`. `<brand>.*` entries cover every public suffix. If listed → stop.
2. robots.txt, one honest request:
   ```bash
   curl -sS -o /dev/null -w '%{http_code}\n' -A 'VoyagerBot/0.1 (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)' https://<host>/robots.txt
   curl -sS -A 'VoyagerBot/0.1 (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)' https://<host>/robots.txt | head -80
   ```
   - 401/403/challenge page → do not crawl; add a denylist entry `{host, reason, source}` instead.
   - Read the `VoyagerBot` group, else `*`. Note `Crawl-delay`, `Content-Signal`, and whether GPTBot/ClaudeBot/CCBot/Google-Extended are blocked (= AI reservation: no LLM use).
3. Terms: find the ToS / API terms. Automated access forbidden → denylist. Not verifiable → `facts_only` at best (or `restricted` if non-commercial/unknown).
4. Prefer an API, dump or feed over HTML.

## 2. Classify

| licence.class | When |
|---|---|
| `open` | CC0, public domain, OGL, CC BY, permissive (Apache/MIT data) |
| `share_alike` | CC BY-SA, ODbL (kept separate, never merged) |
| `facts_only` | editorial copyright (blogs, news), unverified ToS |
| `api_terms` | provider terms (set `max_retention_days`, `display_requirements`) |
| `restricted` | non-commercial or unknown → `enabled: false` (not ingested in v1) |

`source_class`: `official` (government/intergovernmental only), `reference` (Wikipedia, Wikidata, OSM, open datasets), `editorial`, `community` (v1: SE dump only), `commercial_api`.
Kinds: spec §5.7 list. `visa_rule`/`advisory` need `official`; `emergency_numbers` needs `official` or `reference`. `llm_kinds` ⊆ `kinds`, only for unstructured text.

## 3. Write the YAML

- Seed-derived source (id exists in `docs/research/seeds.yaml`)? Edit the tables in `scripts/seeds_to_sources.py` (LICENCES, OFFICIAL, KINDS_OVERRIDE, DISABLED, …) and run `python3 scripts/seeds_to_sources.py`.
- Otherwise create `sources/<id>.yaml` (file name = `id`, lowercase-hyphen). Copy a similar file, e.g. `sources/met-norway.yaml` (API), `sources/blog-nomadicmatt.yaml` (feed), `sources/freetour.yaml` (sitemap + excludes). Enums are snake_case; cadence ∈ `hourly h2 h6 h12 daily weekly monthly quarterly yearly on_demand`. `min_delay_s` ≥ 5 and ≥ Crawl-delay; `daily_budget` ≤ 3000. `needs_key: true` → `key_secret: crawler-src-<id>-key` (Secret Manager name, never the key). No `{template}` URLs.
- API or dump → it needs an adapter (`crates/vc-sources/src/adapters/`, `adapter: <name>`), which is a Task 10-owned change with a wiremock fixture.

## 4. Verify

```bash
python3 -m pytest -q scripts/tests              # schema, denylist, class rules
vcrawl admin source validate <id>               # once Task 15 lands: robots + 3 permit-gated sample fetches
```

Then a fixture + golden test (Task 10 owns `crates/vc-sources`). PR with the robots excerpt, terms quote (URL + date) and licence URL in the description. New sources start `enabled: true` only after review; when in doubt ship `enabled: false`.
