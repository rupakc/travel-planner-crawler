# Sources and Licences

Every site, feed, API or dump the crawler uses is described by **one YAML file** in
`sources/`. Adding a website needs no code. Each item that comes out of the crawler carries
its source, licence and attribution.

## The source catalogue

There are 117 source files: 98 enabled and 19 disabled. They were generated from the
research list `docs/research/seeds.yaml` by `scripts/seeds_to_sources.py`; change that
script's tables, not its output, for lasting changes.

| Method | Count | Examples |
|---|---|---|
| `rss` (RSS or Atom feeds) | 52 | travel blogs, travel news, UK FCDO advice feed |
| `api` | 31 | US State Department advisories, ECB FX rates, MET Norway, Ticketmaster |
| `dump` | 20 | Wikivoyage, Stack Exchange Travel, OurAirports, Mobility Database |
| `html` | 9 | official pages without a feed |
| `sitemap` | 5 | sites best discovered through sitemaps |

Sources cover every Voyager agent domain: visa and advisories, weather, FX, events,
places and activities, walking tours, getting around, SIM, emergency numbers, tips,
flights reference data, plus blogs, news and one forum dump.

Why some are disabled: non-commercial or unknown licences (`restricted`), live prices (a
non-goal; they stay on SerpAPI in Voyager), partner APIs without an agreement, forums other
than the Stack Exchange dump, the full Wikidata dump (SPARQL is used instead), and stale or
paid sources.

## A source file

```yaml
id: fcdo-atom                 # equals the file name
name: UK FCDO travel advice Atom
category: visa                # the Voyager domain it serves
source_class: official
licence:
  class: open
  spdx: OGL-UK-3.0
  attribution: Contains public sector information licensed under the Open Government Licence v3.0
  url: https://www.nationalarchives.gov.uk/doc/open-government-licence/version/3/
method: rss
feeds:
- https://www.gov.uk/foreign-travel-advice.atom
kinds: [advisory]
llm_kinds: []                 # kinds that may use Gemini
render: never                 # never | auto | always
cadence: hourly
min_delay_s: 5                # can only raise the delay
daily_budget: 3000            # can only lower the budget
enabled: true
```

| Field | Meaning |
|---|---|
| `id`, `name`, `category` | identity; `category` is the Voyager domain (visa, weather, events, blogs, news, …) |
| `source_class` | who publishes it (below) |
| `licence` | `class`, `spdx`, `attribution`, `url` |
| `method` | `html`, `rss`, `sitemap`, `api` or `dump` |
| `seeds`, `sitemaps`, `feeds` | starting URLs |
| `adapter` | name of a built-in API/dump adapter, for `api` and `dump` sources |
| `include`, `exclude`, `max_depth` | which discovered links to follow |
| `kinds`, `llm_kinds` | which record kinds to produce; which may use the LLM |
| `render` | `never`, `auto`, `always` |
| `cadence` | `hourly`, `h2`, `h6`, `h12`, `daily`, `weekly`, `monthly`, `quarterly`, `yearly`, `on_demand` |
| `min_delay_s` | at least 5; robots Crawl-delay and latency can only raise it |
| `daily_budget` | at most 3,000; a source can only lower it |
| `politeness_key` | group several domains under one key |
| `enabled` | `restricted` sources must be `false` |
| `needs_key`, `key_secret` | API key held in Secret Manager (the name only, never the key) |
| `max_retention_days`, `display_requirements` | for `api_terms` sources |

The schema is `schemas/source.schema.json`. Enum values are snake_case. The loader
(`vc-sources`) also refuses denylisted seeds and high-stakes kinds from the wrong source
class.

## Source classes

| Class | Meaning | Count |
|---|---|---|
| `official` | government or intergovernmental sites and APIs only | 23 |
| `reference` | encyclopedic and open data (Wikipedia, Wikidata, Wikivoyage, OurAirports) | 22 |
| `editorial` | blogs, news, guide sites | 56 |
| `community` | forums (the Stack Exchange Travel dump; two more are disabled) | 3 |
| `commercial_api` | company APIs used under their terms | 13 |

High-stakes kinds depend on the class: `visa_rule` and `advisory` only from `official`;
`emergency_numbers` from `official`, or from `reference` when two independent reference
sources agree.

## Licence classes

| Class | Examples | How it is used | Sources |
|---|---|---|---|
| `open` | CC0, public domain, OGL, CC BY | may merge into entities; attribution where required | 28 |
| `share_alike` | CC BY-SA (Wikipedia, Wikivoyage, Stack Exchange), ODbL (OpenStreetMap) | kept as separate records, **never merged** with other licences; served with licence and attribution; derived summaries keep the same licence | 10 |
| `facts_only` | blogs, news, other editorial sites | only extracted facts + headline + excerpt of at most 300 characters + link; the full text is never served | 58 |
| `api_terms` | Ticketmaster, Nager.Date | usable per the provider's terms; the source sets `max_retention_days` and `display_requirements` | 14 |
| `restricted` | non-commercial (Open-Meteo free tier, Meteostat) or unknown terms | **not ingested** | 7 |

MET Norway (CC BY 4.0) replaces Open-Meteo for weather. Every API item carries
`licence {class, spdx, attribution, url}`.

## Built-in adapters (APIs and dumps)

An adapter never claims a key itself. The fetcher claims, leases a URL row whose fetch mode
is `adapter:<id>`, and hands the permit to the adapter. Every request goes through the same
permit client. Each response is spooled raw **before** anything is derived from it. Fan-out
(one row per country or per year) is done by enqueueing more `adapter:<id>` rows, so
politeness applies to each.

| Adapter | Source | Notes |
|---|---|---|
| `canada_advisories` | Global Affairs Canada travel advice JSON | advisory level and text per country |
| `ecb_fx` | ECB euro reference rates XML | daily, 90-day and historical |
| `fcdo_content_api` | UK FCDO advice via the GOV.UK Content API | |
| `met_norway` | MET Norway Locationforecast 2.0 | coordinates at most 4 decimals, as MET requires |
| `mobility_database_catalog` | Mobility Database `feeds_v2.csv` (CC0), streamed | keeps active GTFS Schedule feeds |
| `nager_holidays` | Nager.Date public holidays | countries, then one row per year and country |
| `ourairports_csv` | OurAirports `airports.csv` (public domain), streamed | keeps large, medium and small airports |
| `se_travel_dump` | Stack Exchange Travel `Posts.xml` (CC BY-SA), streamed | author names kept only as attribution |
| `state_dept_advisories` | US State Department advisories API | |
| `ticketmaster_discovery` | Ticketmaster Discovery API v2 | the API key never reaches the spool; no key = no request |
| `wikidata_sparql` | Wikidata SPARQL, paginated | emergency numbers, plug types, voltage, airports with IATA codes |
| `wikivoyage_dump` | English Wikivoyage pages-articles (CC BY-SA 4.0), bzip2 streamed | main-namespace pages |

## Forums and personal data

v1 ingests only the **Stack Exchange Travel data dump** (CC BY-SA). Author display names are
kept only as the required attribution string. Erasure requests use
`admin purge --author`. Other open forums block honest bots anyway.

## Adding a source

1. Check the site's robots.txt for `VoyagerBot` and `*`, its terms of service, and the
   denylist.
2. Add `sources/<id>.yaml`. Pick `licence.class` and `source_class` carefully. Unknown
   terms mean `restricted` and `enabled: false`.
3. Run `python3 -m pytest -q scripts/tests`. Once the binary is wired:
   `vcrawl admin source validate <id>` (robots, denylist, three permit-gated sample fetches).
4. Add a fixture and a golden test.

For a new API or dump, implement `SourceAdapter` in `crates/vc-sources/src/adapters/`,
register it in `adapters::REGISTRY`, and add a recorded fixture. See [vc-sources](Crate-vc-sources).

The research behind the catalogue (robots checks, terms, licences) is in `docs/research/`.
