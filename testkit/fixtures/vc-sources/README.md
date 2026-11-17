# vc-sources fixtures

Recorded response samples for the adapter tests (`crates/vc-sources/tests/adapters.rs`), served by
wiremock. Each was fetched **once**, on 2026-11-15, with the one crawler identity
`VoyagerBot/0.1 (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)` after
checking the host's robots.txt, then trimmed (fewer items, shorter bodies) to keep the repo small.
The structure of every trimmed file is unchanged.

| File | Source (adapter) | How it was obtained | Licence of the content |
|---|---|---|---|
| `ecb_eurofxref_daily.xml` | ECB euro reference rates (`ecb_fx`) | fetched whole (robots: allowed, Crawl-delay 5) | ECB, reuse with attribution "Source: ECB" |
| `fcdo_index.json` | GOV.UK Content API `/api/content/foreign-travel-advice` (`fcdo_content_api`) | fetched; `links.children` trimmed to 3 countries, other link lists to 1 | Open Government Licence v3.0 |
| `fcdo_france.json` | GOV.UK Content API `/api/content/foreign-travel-advice/france` | fetched; part bodies cut after the first paragraph past 600 chars, change history to 2 | Open Government Licence v3.0 |
| `fcdo_changes.atom` | GOV.UK `foreign-travel-advice.atom` change feed | fetched; 3 entries kept | Open Government Licence v3.0 |
| `state_dept_advisories.json` | `cadataapi.state.gov/api/TravelAdvisories` (`state_dept_advisories`) | fetched (robots.txt 404 = allowed); 2 entries, summaries cut to 400 chars | US Government work, public domain |
| `canada_index.json` | `data.international.gc.ca/travel-voyage/index-alpha-eng.json` (`canada_advisories`) | fetched (robots.txt 404); 3 countries kept | Open Government Licence – Canada |
| `met_norway_compact.json` | MET Norway Locationforecast 2.0 compact, Oslo (`met_norway`) | fetched (robots: allowed for `*`); 3 timeseries steps kept | CC BY 4.0, "Weather data from MET Norway" |
| `ourairports_airports.csv` | OurAirports `airports.csv` (`ourairports_csv`) | first 8 KiB via a Range request, cut at the last full line | Public domain |
| `wikivoyage_pages_articles.xml.bz2` | `enwikivoyage-latest-pages-articles-multistream.xml.bz2` (`wikivoyage_dump`) | first 1.5 MB via a Range request, decompressed; siteinfo + 4 pages (2 articles, 1 redirect, 1 project page) kept, re-compressed with bzip2 | CC BY-SA 4.0, Wikivoyage contributors |
| `se_travel_posts.xml` | `Posts.xml` of `travel.stackexchange.com.7z` in archive.org item `stackexchange_20251231` (`se_travel_dump`) | fetched via archive.org's in-archive file view; header + 1 tag-wiki row + 1 question + its 2 answers kept | CC BY-SA (2.5/3.0/4.0 per row `ContentLicense`), Stack Exchange Travel contributors |
| `govuk_world_orgs_search.json` | GOV.UK search API `filter_format=worldwide_organisation&count=3` (`govuk_world_orgs`) | fetched 2026-11-17 (robots: `*` disallows only `/*/print$`, `/search/all*`); whole | Open Government Licence v3.0 |
| `govuk_org_tirana.json`, `govuk_org_madrid.json` | GOV.UK Content API `/api/content/world/organisations/<slug>` | fetched 2026-11-17; kept the identity fields, `details.office_contact_associations`, `links.contacts`, `links.main_office` (opening-times HTML cut to its first sections), `links.world_locations` | Open Government Licence v3.0 |
| `canada_offices.json` | `data.international.gc.ca/travel-voyage/opendata-offices-list-alpha-eng.json` (`canada_offices`) | fetched 2026-11-17 (robots.txt 404); 4 of 230 country entries kept (Azores `PT-20`, The Gambia, Hong Kong, Chad) | Open Government Licence – Canada |
| `govuk_medical_collection.json` | GOV.UK Content API `/api/content/government/collections/doctors-and-medical-facilities-worldwide-list` (`govuk_medical_lists`) | fetched 2026-11-17; 3 of 167 documents kept | Open Government Licence v3.0 |
| `govuk_medical_albania.json`, `govuk_medical_albania_list.json` | the Albania publication and its HTML attachment (Content API) | fetched 2026-11-17; publication body cut to its first paragraph, the attachment kept whole | Open Government Licence v3.0 |
| `enwiki_visa_stream.xml.bz2` | one bzip2 stream of `enwiki-latest-pages-articles-multistream.xml.bz2` (`enwiki_visa`), fetched with one Range request at the offset the multistream index gives for "Visa requirements for Albanian citizens" | fetched 2026-11-17; decompressed, 3 of 100 pages kept (the visa article with its main table cut to 8 rows and its territories table to 1, one other article cut to 600 characters, one redirect), re-compressed | CC BY-SA 4.0, Wikipedia contributors |
| `enwiki_visa_index.txt.bz2` | multistream index lines (`offset:page_id:title`) | built from the real index lines around that article (streamed once on 2026-11-17), offsets moved so the stream starts at byte 1000 and ends at the fixture's length | CC BY-SA 4.0, Wikipedia contributors |

## Built from documentation (not fetched)

These sources could not be sampled politely, so their fixtures follow the provider's documented
response format. Values are illustrative.

| File | Why not fetched | Based on |
|---|---|---|
| `ticketmaster_events.json` | needs an API key | the example response on the Discovery API v2 docs page (developer.ticketmaster.com), trimmed to one image per object |
| `wikidata_emergency_numbers.srj`, `wikidata_empty.srj` | `query.wikidata.org/robots.txt` disallows `/sparql` for `*` | W3C SPARQL 1.1 Query Results JSON Format |
| `nager_available_countries.json`, `nager_public_holidays_2026_fr.json` | `date.nager.at/robots.txt` redirects to `nagerholidays.com/robots.txt`, which disallows `/api/v` | Nager.Date v3 `CountryV3Dto` / `PublicHolidayV3Dto` (dates are the real 2026 French holidays) |
| `mobility_feeds_v2.csv` | `files.mobilitydatabase.org/robots.txt` disallows `/` | Mobility Database catalog CSV columns (MobilityData spreadsheet schema v2); `example.org` URLs |

### Open-data adapters (2026-11-18)

Built from documentation, not fetched: only each host's robots.txt was requested (once, as
VoyagerBot). Values are illustrative unless noted.

| File | Source (adapter) | Based on |
|---|---|---|
| `openholidays_countries.json`, `openholidays_public_de.json`, `openholidays_school_de.json` | OpenHolidays API (`openholidays`) | the API's OpenAPI document (`CountryResponse`, `HolidayResponse`, `LocalizedText`, `SubdivisionReference`); dates are the real 2026 German ones |
| `nyc_parks_events.json` | NYC Parks Events Listing, Socrata `fudw-fgrp` (`json_events`) | the dataset's `columns.json` field names |
| `stockholm_events_p1.json`, `stockholm_events_p2.json` | Visit Stockholm public events list (`json_events`) | the DRF browsable page of `/api/public-v1/events/` (keys, nesting, paging fields); `example.org` organiser URLs |
| `eurostat_prc_ppp_ind.json` | Eurostat dissemination API, `prc_ppp_ind` (`eurostat_jsonstat`) | JSON-stat 2.0 as the API returns it (sparse `value`/`status` objects keyed by flat index) |
| `wdi_csv.zip` | World Bank `WDI_CSV.zip` (`wdi_csv`) | the bulk download's layout: `WDICSV.csv` (Country Name, Country Code, Indicator Name, Indicator Code, one column per year) plus metadata members; written with Python `zipfile` (deflate) |
