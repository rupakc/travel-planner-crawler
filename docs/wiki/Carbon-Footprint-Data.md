# Carbon footprint data

Voyager estimates the carbon footprint of a trip: its flights, trains, buses, ferries, cars and taxis, nights in hotels, meals and activities. The crawler supplies the published factors behind that estimate. It never computes a footprint itself.

## Kinds

| Kind | Storage | One item is |
|---|---|---|
| `emission_factor` | record | one published factor, for example 0.11704 kg CO2e per passenger.km for a long-haul economy flight with RF (DESNZ 2026), 6.7 kg CO2e per room per night in France, or 99.48 kg CO2e per kg of beef from the beef herd |
| `grid_carbon_intensity` | observation | one country's electricity carbon intensity for one year or one month, in gCO2e per kWh; series `grid_ci:<ISO2>:<period>:<basis>` (`period` is `annual` or `monthly`) |

Each `emission_factor` record carries:

- `activity` (a snake_case slug) and `activity_label` (the publisher's own label path);
- `category`: `flights`, `rail`, `bus`, `coach`, `ferry`, `car`, `taxi`, `motorbike`, `accommodation`, `food`, `activity`, `electricity`, `fuel` or `other`;
- `geography` and `country_code`, plus `region_code` for a sub-national factor (an eGRID subregion such as `CAMX`, or a US state such as `CA`);
- `unit` and `value`, copied as published;
- `gas_basis`: `co2`, `co2e`, `co2e_with_rf` or `co2e_without_rf`;
- `scope`, `year`, `publisher`, `dataset` and the publisher's `factor_id`.

Both kinds accept official and reference sources only.

## Adapter `factor_table`

Each leased row is one file, fetched in a single conditional request with a size cap and spooled raw. The processor then maps the stored file row by row. It never uses an LLM, never converts units, and turns a blank cell into no record rather than 0.

Each dataset table is one `FactorMapping` in `crates/vc-sources/src/adapters/factor_table.rs`. A mapping sets:

- how to recognise the file's URL (several mappings may match one file: one per sheet or per table);
- the format: CSV, a CSV inside a ZIP, an XLSX sheet or an ODS sheet (both through calamine). The header is the first row that holds every required column, with line breaks inside a header cell read as spaces;
- for a sheet that stacks several tables, the anchor: the table's title cell (`Table 10`). The header is searched after it, and the table ends at the first row whose label cell is empty or starts with "Source" or "Note";
- row filters, an earliest year, and whether a published 0 means "no figure";
- the activity label: an optional fixed prefix plus columns, with trailing footnote letters dropped where the publisher uses them;
- the value column (by name, or by a header prefix);
- the year: a column, the first year written in a column (`Apr 2024 to Mar 2025` → 2024), the value header's suffix, the title above the header (`eGRID2023`), or the file name;
- the unit and scope (fixed text or a column);
- the category and gas-basis tables;
- the geography: fixed, a country-name column, an ISO alpha-3 column, or a region code of one country.

A file whose mapped sheet or table is missing is refused as a whole.

| Source | File | Yields |
|---|---|---|
| `od-desnz-ghg-factors` | UK DESNZ GHG conversion factors, flat file (XLSX, OGL v3, yearly in June) | kg CO2e factors for flights by haul and cabin (with and without RF), ferry, rail, bus, taxi, motorbike, cars, hotel stays per country, and UK grid electricity |
| `od-owid-food-ghg` | OWID grapher `ghg-per-kg-poore.csv` (CC BY 4.0) | kg CO2e per kg for each food product (Poore and Nemecek 2018, global) |
| `carbon-owid-grapher-ci` | OWID grapher `electricity-mix.csv`, metric `carbon_intensity` (CC BY 4.0) | gCO2e per kWh per country and year (life-cycle; aggregates skipped) |
| `od-epa-egrid` | US EPA eGRID2023 summary tables (XLSX, public domain): Table 1 and Table 3 | lb CO2e per MWh, total output emission rate, per eGRID subregion and per US state (`region_code`), plus the US total (stored once, from Table 3). Puerto Rico (subregion `PRMS`, state `PR`) is stored under its own ISO code `PR` |
| `od-epa-ef-hub` | US EPA GHG Emission Factors Hub 2025 (XLSX, one sheet of stacked tables): Table 10 and Table 2 | kg CO2 per vehicle-mile or passenger-mile for car, light truck, motorcycle, rail, bus and air by haul; kg CO2 per unit of each transport fuel. CH4 and N2O are separate columns and are not summed into CO2e |
| `od-owid-travel-mode` | OWID grapher `carbon-footprint-travel-mode.csv` (CC BY 4.0) | g CO2e per passenger-km for 16 UK modes (DESNZ 2022; flights with RF). A secondary copy (scope `secondary copy of UK DESNZ factors`): for GB, prefer `od-desnz-ghg-factors`, the current factors at source |
| `od-owid-co2-data` | OWID `owid-co2-data.csv` (CC BY 4.0, 15 MB) | t CO2 per person (territorial and consumption-based) and t CO2e per person (GHG) per country, from 2010; category `other`, for destination comparisons |
| `od-orr-rail-environment` | ORR Table 6100 (ODS, OGL v3) | g CO2e per passenger-km of GB passenger rail per financial year (traction energy) |
| `od-fueleconomy-gov` | FuelEconomy.gov `vehicles.csv.zip` (public domain) | tailpipe g CO2 per mile per US model, model years from 2022 |
| `od-concito-big-climate-db` | The Big Climate Database v1.2 (XLSX, CC BY 4.0): sheets DK, GB, NL, FR, ES | kg CO2e per kg per food and country, cradle to retail |
| `od-rivm-food-lca` | RIVM Dutch food LCA database 2024 (XLSX, CC BY 4.0) | kg CO2e per kg, cradle to retail and cradle to consumption (NL). RIVM: not critically reviewed, so compare food groups rather than single foods |

US grid factors are `emission_factor` records, not `grid_carbon_intensity` observations: the observation series is keyed per country (`grid_ci:<ISO2>:…`) and in g per kWh, and the crawler never converts units.

## Eurostat `sdg_13_31` (adapter `eurostat_jsonstat`)

`od-eea-co2-cars-avg` reads the EEA's average CO2 per km of new passenger cars from Eurostat `sdg_13_31` (CC BY 4.0, one 14 KB JSON-stat request a year). The EEA datahub item only links file shares whose lists need JavaScript. Each country and year becomes one `emission_factor` (category `car`, unit `g CO2 per km`); the EU27 aggregate is skipped. The cube's status flags are kept in `scope`: a break in series (`b`, the 2017 test-cycle change) reads `type approval; series break`, an estimate (`e`) adds `estimated`, so values across the break never look comparable; provisional cells (`p`) are not stored until final.

## Serving to the app

The read API serves both kinds (contract: `docs/Crawler-API.md`):

- `GET /v1/carbon/factors?category=&country=&region=&year=&gas_basis=&activity=&source=&limit=&cursor=` returns current `emission_factor` records, newest factor-set year first, with the same licence shaping as search;
- `GET /v1/carbon/grid?country=&period=&basis=&from=&to=&limit=` returns every `grid_ci:<country>:…` point of one country, from every source, newest first. `/v1/observations?series=` still serves a single series.

**Adding a dataset.** Add a mapping, record a small excerpt in `testkit/fixtures/vc-sources/`, and give the source `adapter: factor_table` with the file URL as its only seed. All of these go in `scripts/seeds_to_sources.py`.

## Adapter `ember_api`

The Ember API (`carbon-ember-api`, CC BY 4.0, attribution "Ember") gives the carbon intensity of electricity generation for every country, yearly from 2000 and monthly from 2019. Leaving `entity_code` out returns all countries in one call, so the source has two rows and a weekly refresh costs two requests:

- `/v1/carbon-intensity/yearly?is_aggregate_entity=false&start_date=2000` (about 5,300 rows);
- `/v1/carbon-intensity/monthly?is_aggregate_entity=false&start_date=2019-01` (about 7,900 rows).

Each country row becomes one `grid_carbon_intensity` observation: `period` `annual` or `monthly`, `date` the first day of the period, `basis` `lifecycle`, `publisher` `Ember`. Region aggregates, unknown codes, missing or negative values and malformed dates are skipped. The same series key as the OWID grid file is used, so the two publishers line up per country.

**The key.** The owner's free key is in Secret Manager as `crawler-src-ember-key` (the source's `key_secret`). Ember takes it as the `api_key` query parameter: the adapter adds it to the request only, strips it from every stored URL, redirect and error, and refuses a leased row that already carries it. Ember states no rate limit; the source's `daily_budget` is 10.

## Still off

Each of these sources is off, and `docs/SOURCES.md` records its reason.

- **Ember yearly CSV:** its file host's robots.txt disallows everything except PDFs. The same data now comes from the Ember API (above).
- **SNCF:** the files are only available under `/api/`, which robots.txt disallows.
- **French Base Carbone:** it is on `static.data.gouv.fr`, which robots.txt disallows.
- **Australian NGA factors:** `www.dcceew.gov.au` answered no request from VoyagerBot (robots.txt and the publication page reset or time out), so the workbook URL is unconfirmed. Re-test from the crawler VM.
- **AGRIBALYSE (Dataverse):** `entrepot.recherche.data.gouv.fr` robots.txt disallows everything for `*`.
- **FAOSTAT emissions intensities:** `fenixservices.fao.org` robots.txt answered 521 (a 5xx means disallow). Retry later.
- **DfT TAG data book:** its occupancies are persons per vehicle, not emission factors, so they need their own kind first (owner decision). Its carbon table repeats DESNZ.
- **US NHTS occupancy:** occupancy has to be computed from trip microdata, and the adapter only copies published values.
- **Electricity Maps:** it needs a free API key from the owner.
