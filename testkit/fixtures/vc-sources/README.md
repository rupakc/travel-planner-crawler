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
| `wikivoyage_de_pages_articles.xml.bz2` | `dewikivoyage-latest-pages-articles.xml.bz2` of 2026-11-01 (`wikivoyage_dump`, `wikivoyage-dump-de`) | first 6 MB of the stream (2026-11-18), decompressed; siteinfo + 3 pages (the article Jotunheimen with `{{GeoData}}` and 21 `{{vCard}}` listings, 1 redirect, 1 project page) kept, contributor names blanked, re-compressed with bzip2 | CC BY-SA 4.0, Wikivoyage contributors (de.wikivoyage.org) |
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

### US country information and food-hygiene registers (2026-11-18)

Fetched once each with the VoyagerBot identity (after each host's robots.txt; cadataapi.state.gov's
robots.txt answers 404 and was not re-requested, and only its listed `/api/CountryTravelInformation`
path was requested), then trimmed. The structure of every file is unchanged except where noted.

| File | Source (adapter) | How it was obtained | Licence of the content |
|---|---|---|---|
| `state_dept_country_info.json` | `cadataapi.state.gov/api/CountryTravelInformation` (`state_dept_country_info`) | one request (5.9 MB, 211 countries); 4 kept (AF, AS, HK, JA); local laws, health and safety cut before their fourth titled paragraph, transport and description to 300 characters, the other entry texts to 6000 characters | US Government work, public domain |
| `fsa_establishments_p1.json` | FHRS API `/Establishments?localAuthorityId=120&businessTypeId=1&pageSize=3` (`fsa_fhrs`) | fetched with `x-api-version: 2` (robots.txt 404); `meta` edited to page 1 of 2, `links` cut to `self` | Open Government Licence v3.0, Food Standards Agency |
| `fsa_establishments_p2.json` | FHRS API, City of London pubs and bars, `pageSize=1000` | fetched; 2 of 193 establishments kept (one rated, one awaiting inspection), `meta` edited to page 2 of 2 | Open Government Licence v3.0, Food Standards Agency |
| `nyc_restaurant_inspections.json` | NYC Open Data Socrata `43nn-pn8j` (`nyc_restaurant_inspections`), the adapter's own grouped query with `$limit=6` | fetched whole (robots.txt: Crawl-delay 1, `/resource` allowed) | NYC Open Data terms of use (DOHMH) |
| `toronto_dinesafe_package.json` | CKAN `package_show?id=dinesafe` (`toronto_dinesafe`) | fetched (robots.txt allows `/api/`); `result` cut to identity and licence fields and each resource's id, name, format, `datastore_active`, url | Open Government Licence – Toronto |
| `toronto_dinesafe_page.json` | CKAN `datastore_search` on the datastore resource, the adapter's distinct field list with `limit=6` | fetched whole | Open Government Licence – Toronto |

### Generic open-data readers: socrata, udata_resource, wfs_geojson (2026-11-18)

Fetched once each with the VoyagerBot identity after each host's robots.txt (Socrata hosts:
Crawl-delay 1, `/resource/` allowed; www.data.gouv.fr allows `/api/1/datasets/`; the Atout France,
Vienna and Zürich hosts have no robots rules), then trimmed. Personal columns of the registers were
never requested.

| File | Source (adapter) | How it was obtained | Licence of the content |
|---|---|---|---|
| `socrata_chicago_food_inspections.json` | Chicago Data Portal `4ijn-s7e5` (`socrata`), the mapping's `$select`/`$where`/`$order` with `$limit=7` | fetched whole (two licences, seven inspections) | City of Chicago data terms |
| `socrata_austin_food_inspections.json` | City of Austin `ecmv-9xxi` (`socrata`), the mapping's query with `$limit=6` | fetched whole | public domain |
| `socrata_catalonia_tourism_register.json` | Generalitat de Catalunya `t2h3-cgys` (`socrata`), the mapping's `$select` | assembled from three queries with that `$select`: the first 3 rows of the mapping's filter, 3 hotels/hostals and 1 campsite | Catalonia open data terms (unconfirmed; facts only) |
| `socrata_nyc_public_restrooms.json` | NYC Open Data `i7jb-7jku` (`socrata`), the mapping's query with `$limit=5` (plus `:id`) | fetched whole | NYC Open Data terms of use |
| `udata_atout_france_dataset.json` | www.data.gouv.fr `/api/1/datasets/hebergements-touristiques-classes-en-france/` (`udata_resource`) | fetched; cut to identity, licence and each resource's id, title, format, type, url, `last_modified` | Licence Ouverte 2.0 |
| `udata_atout_france_classes.csv` | Atout France `hebergements_classes.csv` (`udata_resource`) | one download (3.7 MB, 21,362 rows); header plus the first 3 rows and the first campsite, tourist residence, holiday village and collective hostel | Licence Ouverte 2.0 |
| `wfs_wien_sights.json`, `wfs_wien_museums.json`, `wfs_wien_toilets.json`, `wfs_wien_wifi.json` | data.wien.gv.at WFS `GetFeature` for `ogdwien:SEHENSWUERDIGOGD`, `MUSEUMOGD`, `WCANLAGEOGD`, `WLANWIENATOGD` (`wfs_geojson`) | fetched; first 3 / 2 / (first, first with an accessible cabin) / 2 features kept | CC BY 4.0, Stadt Wien – data.wien.gv.at |
| `wfs_zuerich_wc.json`, `wfs_zuerich_wc_rs.json` | www.ogd.stadt-zuerich.ch WFS `GetFeature` for `poi_zueriwc_view` and `poi_zueriwc_rs_view` (`wfs_geojson`) | fetched; first feature kept, staff/e-mail/fax properties removed | CC0, Stadt Zürich |

### factor_table: carbon factor tables (2026-11-19)

| File | Source | How it was cut | Licence |
|---|---|---|---|
| `factor_desnz_flat_2026.xlsx` | UK DESNZ "ghg-conversion-factors-2026-flat-format-revised.xlsx", sheet "Factors by Category" | the sheet's 6 title/header rows plus 34 real rows copied unchanged with openpyxl (Butane fuel rows, UK electricity, flights, ferry, taxi, bus, rail, and the hotel stays for UK, UK (London), Argentina (blank), France, Hong Kong and Korea) into a new workbook with the same sheet name | OGL v3 |
| `factor_owid_food_poore.csv` | OWID grapher `ghg-per-kg-poore.csv?v=1&csvType=full&useColumnShortNames=true` | header plus the first 7 rows | CC BY 4.0 |
| `factor_owid_grid_ci.csv` | OWID grapher `electricity-mix.csv?frequency=annual&metric=carbon_intensity&source=total&...` | header plus 6 rows (ASEAN and World aggregates, France 2023 and 2024, Germany, India) | CC BY 4.0 |

Second batch (2026-11-19; each file fetched once after its host's robots.txt, rows copied unchanged with openpyxl/csv):

| File | Source | How it was cut | Licence |
|---|---|---|---|
| `factor_egrid_summary_2023.xlsx` | US EPA `summary_tables_rev2.xlsx` (eGRID2023) | sheets "Table 1" and "Table 3": the title and 3 header rows, then AKGD, CAMX, NYCW and U.S. (Table 1) and CA, NY, VT and U.S. (Table 3) | US public domain |
| `factor_egrid_puerto_rico_synthetic.xlsx` | synthetic (not real figures), built from the eGRID2023 layout above | the same title and header rows; PRMS, CAMX and U.S. (Table 1) and PR, CA and U.S. (Table 3) with round made-up values, to test the Puerto Rico country mapping | test data, no licence |
| `factor_epa_ef_hub_2025.xlsx` | US EPA `ghg-emission-factors-hub-2025.xlsx`, sheet "Emission Factors Hub" | sheet rows 1-3, 101-116 (Table 2 with its source lines), 501-517 (Table 10 with its source and notes) and 521-526 (start of Table 11, to check the table end) | US public domain |
| `factor_owid_travel_mode.csv` | OWID grapher `carbon-footprint-travel-mode.csv?v=1&csvType=full&useColumnShortNames=true` | the whole file (16 rows) | CC BY 4.0 |
| `factor_owid_co2_data.csv` | OWID `owid-co2-data.csv` (first 80 KB, one Range request) | header plus Afghanistan 2009, 2023, 2024 and Africa 2023, 2024 | CC BY 4.0 |
| `factor_orr_6100.ods` | ORR `table-6100-estimates-of-normalised-passenger-carbon-dioxide-equivalent-emissions.ods` | the whole file (8 KB) | OGL v3 |
| `factor_fueleconomy_vehicles.csv.zip` | FuelEconomy.gov `vehicles.csv.zip` | a new deflate ZIP with `vehicles.csv`: header plus a 2021 Toyota Prius, the 2026 Prius (id 49301) and the 2026 Tesla Model Y (id 49743) | US public domain |
| `factor_concito_v1_2.xlsx` | CONCITO `Version 1.2_DK_ENG_rev_18122025_0.xlsx` | sheets DK, GB, NL, FR, ES: header plus Ra00001, Ra00008 and Ra00010 each | CC BY 4.0 |
| `factor_rivm_2024.xlsx` | RIVM `Database-milieubelasting-voedingsmiddelen-database-versie-23-september-2024-...xlsx` | sheets "tot-en-met-retail" and "tot-en-met-consumptie": header plus 3 rows each | CC BY 4.0 |
| `eurostat_sdg_13_31.json` | Eurostat `sdg_13_31?format=JSON&lang=EN` | the whole response (14 KB) | CC BY 4.0 |

Occupancy (2026-11-19; fetched once after the host's robots.txt):

| File | Source | How it was cut | Licence |
|---|---|---|---|
| `factor_dft_tag_occupancy_2026.xlsx` | DfT `tag-data-book-v2-03.xlsm` (May 2026 v2.03, 6 MB) | sheet A1.3.3.2 whole (rows 1-38) and the first 30 rows of sheet A1.3.3, values only (no formulas, styles or macros) copied with openpyxl into a new workbook with the same sheet names; numeric cells then set back to the original file's exact cell texts (openpyxl rounds to 16 digits) | OGL v3 |

Third batch (2026-11-19; each file fetched once after its host's robots.txt):

| File | Source | How it was cut | Licence |
|---|---|---|---|
| `factor_mobitool_v3_1.xlsx` | mobitool `mobitool-Faktoren-v3.1-20250408.xlsx` (6 MB, assets.ctfassets.net) | sheet "mobitool-Faktoren-v3.1", rows 1-26, 51-54, 59-64, 100-106, 113-127 and 206-207 (header rows, bikes and buses, coaches, cars, Swiss trains, ship, cable car, trains abroad, airplanes, two freight-rail rows), columns A-AK, values only, with every merged region that lies wholly inside a kept block re-created at its new rows; numeric cells then set back to the original file's exact cell texts | facts only (all rights reserved; values used as facts) |
| `factor_aib_residual_mix_2025.xlsx` | AIB `2025_Final _Residual mix calculation results_25052026.xlsx` (80 KB) | sheet "CO2" whole (35 rows) and the first 10 rows of "Residual Mixes", values only | facts only |
| `factor_epa_supply_chain_v1_3.csv` | US EPA `SupplyChainGHGEmissionFactors_v1.3.0_NAICS_CO2e_USD2022.csv` (120 KB) | header plus 17 rows as published (14 travel industries, farming, freight air, casinos) | US public domain |
| `factor_owid_tourism_aviation.csv` | OWID `per-capita-co-emissions-from-international-flights-tourism-adjusted.csv` (32 KB) | header plus the Africa, France and United States rows as published | CC BY 4.0 |


### ember_api: Ember API carbon intensity (2026-11-19)

| File | Source | How it was cut | Licence |
|---|---|---|---|
| `ember_ci_yearly.json` | `GET api.ember-energy.org/v1/carbon-intensity/yearly?is_aggregate_entity=false&start_date=2000` (owner key, not stored) | `data` cut to 7 real rows (France, Germany, India 2023 and 2024; Kosovo 2024); `stats.number_of_records` set to 7, `query_value_range` removed | CC BY 4.0 (Ember) |
| `ember_ci_monthly.json` | `GET …/v1/carbon-intensity/monthly?is_aggregate_entity=false&start_date=2019-01` | `data` cut to 4 real rows (France and New Zealand, January and February 2025); same `stats` edits | CC BY 4.0 (Ember) |
