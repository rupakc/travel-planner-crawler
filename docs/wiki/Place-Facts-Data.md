# Place facts data

Voyager shows short facts about the places on a trip: countries, regions, cities, towns, villages and features. A fact is historical, fun, popular, record-breaking or little known. The crawler stores each one as a single sentence **in our own words**, with a link to where it came from. It never copies source text: a short quoted value (an official name, a nickname, a motto) is the most a sentence holds of it.

The app section is `facts` (added 2026-11-20). The kind is `place_fact`, and the read endpoint is [`GET /v1/facts`](Read-API) (full contract in `docs/Crawler-API.md`).

## The `place_fact` record

| Field | |
|---|---|
| `place` | `geonames_id` (the join key: Voyager's `places` and `features` tables come from GeoNames), `wikidata_qid`, `admin_level` (`country`, `admin1`, `admin2`, `locality`, `village`, `feature`), `country` (ISO 3166-1 alpha-2), `name`, `name_lang`, an optional source-native id (`native_id`, e.g. `factbook:gm`) |
| `fact_type` | `historical`, `fun`, `popular`, `unbelievable` or `little_known` (mapping below) |
| `text` | at most 280 characters, in `lang`; optional `text_en`, always a machine translation made at ingest |
| `text_origin` | `templated` (our sentence over typed fields) or `rewritten` (our rewrite of a source sentence, from the extractors to come) |
| `value_lang` | the language of a quoted value in `text` (`de` for "Spree-Athen"), which can differ from `lang` |
| `llm_ok` | false for share-alike text and AI-reserved sources: such text never goes into a model prompt |
| `date_ref` | the year or date the fact refers to (`1244`, `1806-10-27`, `-0500` for 500 BCE), for "on this day" and timelines |
| `quantity` | `{value, unit, rank_scope}` behind a record or figure, from structured sources only |
| `source_field` | where in the source it came from (`wikidata:P571`, `factbook:Geography/Elevation/highest point`) |
| `source` | `name`, `publisher`, `sourced` (`yes`/`partial`/`no`: whether the source cites its own sources; for Wikidata, whether the statement has a reference). The API adds `source_id`, `source_class` and the deep `url` |
| `licence` | `class` (`open`, `share_alike`, `facts_only`, `api_terms`), `spdx`, `attribution` |
| `confidence` | `structured` (a typed field), `extracted` (a rule or model extraction from text; `evidence_offset` and `evidence` keep the span) or `corroborated` (two independent sources agree) |
| `ai_bot_blocked` | the source's robots.txt blocks AI bots: title and link only, never extracted text |
| `source_url`, `as_of`, `evidence` | provenance, as in every kind (`as_of` is the edit date, the crawl date, or a frozen edition's date) |

Mapping to `fact_type`:

- `historical`: inception and founding, dissolution, former names, significant events, heritage designations.
- `popular`: nicknames, mottos, official names, national symbols, population, area, elevation, World Heritage status.
- `unbelievable`: highest and lowest points and other records.
- `little_known`: name origins (named after), twin towns, borders.
- `fun`: editorial curiosities, once rewritten in our own words (a later step).

## Licences and model use

- Wikidata and the Factbook JSON are CC0 (the Factbook itself is a US Government work): their facts are `open`, `llm_ok: true`.
- Wikipedia text is CC BY-SA 4.0, and the rule is that we never copy article text and never merge share-alike text into our wording or send it to a model. A "Did you know" or "On this day" hook is a whole sentence of article text, and with no model allowed there is no compliant way yet to rewrite it, so this step has **no Wikipedia hook reader** (a first draft had one; it was taken out in review). The read API also refuses any record whose text is not our own (`text_origin` other than `templated` or `rewritten`).
- Editorial and `facts_only` sources (a later step) give rewritten sentences of at most 280 characters, under the served excerpt limit, always with a link. Facts from sources whose robots.txt reserves AI use are `ai_bot_blocked`, never `llm_ok`.

## Adapters (step one: structured sources, no LLM)

### `wikidata_facts`

Reads Wikidata items. The leased row is either:

- an **EntityData** URL (`https://www.wikidata.org/wiki/Special:EntityData/Q64.json`; robots.txt: `Allow: /wiki/Special:EntityData/*.`). The adapter fetches the item, then up to 12 items it points to, for their labels only (the current country, events, highest and lowest points, namesakes, twin towns, neighbours, heritage designations). Every response is spooled raw as a `subresource`. One **bundle** (`resource`, JSON: the slimmed item plus the labels) is the row's primary capture, and that bundle is what the processor maps. So the facts come from one capture. The bundle carries the item's HTTP status, so the processor treats it as one response (not a status-less dump batch) and reconciles the row in full: a statement that is removed or reverted on Wikidata makes its fact stale on the next crawl. An item that is no longer a place (it lost P1566 and P297) gets an empty bundle, which stales all of its row's facts.
- a **dump** URL (`…/wikidatawiki/entities/…json.gz|bz2`, the existing `wikidata-dump` source, still off: the full dump is far over the 2 GB dump cap). Items with a GeoNames id (P1566) or an ISO 3166-1 code (P297) are slimmed on the fly (filter `wikidata-facts-v1`). The dump has no labels of other items, so facts that need one (twin towns, events, highest points) are left out on this route.

`/w/api.php`, `Special:ItemByTitle` and `query.wikidata.org/sparql` are disallowed by robots.txt. None is used.

| Property | Fact | Type |
|---|---|---|
| P571 inception | "Berlin was founded in 1244." | historical |
| P576 dissolved | "… was dissolved in 1990." | historical |
| P793 significant event + P585 | "A key event in the history of Berlin: Fall of Berlin, on 27 October 1806." | historical |
| P1448 official name (with an end date) | "… was officially called "…" until 1990." | historical |
| P1448 official name (current, unlike the label) | "The official name of … is "…"." | popular |
| P1435 heritage designation (+ P580) | "… was designated … in 1979." | historical |
| P1449 nickname | "Berlin is nicknamed "Spree-Athen"." | popular |
| P1451 motto | "The motto of … is "…"." | popular |
| P1082 population (preferred, else the latest P585) | "Berlin had a population of 3,700,577 in 2025." | popular |
| P2044 elevation | "Berlin lies 34 m above sea level." | popular |
| P2046 area (km²) | "Berlin covers an area of 891.12 km²." | popular |
| P757 World Heritage Site ID | "… is a UNESCO World Heritage Site (site 1234)." | popular |
| P610 / P1589 highest / lowest point (+ P2044) | "The highest point of Berlin is Arkenberge (121.9 m)." | unbelievable |
| P138 named after | "… is named after …." | little_known |
| P190 twinned with (up to 5 named) | "Berlin is twinned with Los Angeles and Paris." | little_known |
| P47 shares border with (up to 5 named) | "Berlin shares a border with Brandenburg and Barnim District." | little_known |

The sentences are English, so only English labels (and Wikidata's language-independent `mul` labels) are kept: a place or a referenced item without one is left out of the sentences rather than named in another language. Official names, nicknames and mottos are quoted in their own language, English values first, and the record keeps that language in `value_lang`. Deprecated statements are ignored. For single-valued facts, a preferred statement wins over normal ones. The place gets its GeoNames id from the first best P1566 and its country from its own P297, or from the P297 of its current P17 (the one without an end time). `admin_level` is `country` for an item with P297 and `admin1` for one with an ISO 3166-2 code (P300). `confidence` is `structured`.

### `factbook_json`

Reads the CIA World Factbook, final edition (2026-02-04, frozen), from the factbook.json project (`raw.githubusercontent.com/factbook/factbook.json/<ref>/<region>/<code>.json`, CC0; robots.txt answers 404, which means allowed). One row is one country file, fetched with a conditional GET. Every fact has `as_of: 2026-02-04` and the country resolved from the "conventional short form" name. A Factbook fact is keyed by its ISO code only; the read API adds the GeoNames id and QID of the Wikidata country fact for that code (when one is held), so `?geonames_id=` and `?wikidata_qid=` find it too. A file whose name does not resolve (oceans, the World, the EU) yields nothing.

| Field | Fact | Type |
|---|---|---|
| Geography / Elevation / highest point, lowest point | "The highest point of Germany is Zugspitze (2,963 m)." | unbelievable |
| Geography / Area / total | "Germany covers a total area of 357,022 km²." | popular |
| Government / National heritage / total World Heritage Sites | "Germany has 56 UNESCO World Heritage Sites." | popular |
| Government / National symbol(s) (split into symbols and colours; skipped when an item is not a short name) | "Germany has eagle as its national symbol." / "… and its national colours are black, red and yellow." | popular |
| Government / Country name / former | "Former names of Germany: German Reich." | historical |
| Government / Independence (its first date) | "Germany dates its independence to 18 January 1871." | historical |

### Not yet: USGS GNIS feature history

The GNIS "Feature Description/History" dump (`prd-tnm.s3.amazonaws.com/StagedProducts/GeographicNames/`, US public domain) is listed ADD. It is left for later because it is not cheap to map. Its history field is free prose (up to 3,000 characters), so it needs the rewrite step to become one sentence of our own. GNIS ids also need a join to GeoNames, through Wikidata P590 (GNIS ID) and P1566. The existing `zip_stream` + `csv_filter` readers can stream it once that extractor exists.

## Sources

This change adds the readers, the kind and the endpoint only. The sources that use them come in a separate PR: `od-wikidata-entitydata` and the Factbook files (no Wikipedia hook sources: see above). That PR is also where the existing `wikidata-dump` source gets `place_fact` and the section `facts`. Until then no source has the `facts` section, so `docs/SOURCES.md` shows none.

## Rollout

1. Structured and global (this step): Wikidata properties and the Factbook, so that every place gets at least one fact.
2. The official and heritage registers of each country, and the gazetteer name-origin fields (GNIS, NSW GNR, NZ Gazetteer, Logainm, …).
3. Editorial curiosity sources come last, behind corroboration. This step needs the `place_fact` extractor: candidate sentences, then place resolution, then fact-type classification, a rewrite in our own words and a dedupe against the facts we already hold. The evidence span is kept, and `confidence` stays `extracted` until a second source agrees.

Not done yet:

- Duplicates across sources are not reconciled: Wikidata and the Factbook can both give a country's highest point or area. Both are served, each with its own source; dedupe and `corroborated` come with the extractor step.
- The bare-country join scans the current place facts for each country-only fact, which is fine at today's volume; an expression index on the place's `country` and `admin_level` would speed it up.
