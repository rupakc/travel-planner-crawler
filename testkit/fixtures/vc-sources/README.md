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

## Built from documentation (not fetched)

These sources could not be sampled politely, so their fixtures follow the provider's documented
response format. Values are illustrative.

| File | Why not fetched | Based on |
|---|---|---|
| `ticketmaster_events.json` | needs an API key | the example response on the Discovery API v2 docs page (developer.ticketmaster.com), trimmed to one image per object |
| `wikidata_emergency_numbers.srj`, `wikidata_empty.srj` | `query.wikidata.org/robots.txt` disallows `/sparql` for `*` | W3C SPARQL 1.1 Query Results JSON Format |
| `nager_available_countries.json`, `nager_public_holidays_2026_fr.json` | `date.nager.at/robots.txt` redirects to `nagerholidays.com/robots.txt`, which disallows `/api/v` | Nager.Date v3 `CountryV3Dto` / `PublicHolidayV3Dto` (dates are the real 2026 French holidays) |
| `mobility_feeds_v2.csv` | `files.mobilitydatabase.org/robots.txt` disallows `/` | Mobility Database catalog CSV columns (MobilityData spreadsheet schema v2); `example.org` URLs |
