# Carbon footprint data

Voyager estimates the carbon footprint of a trip: its flights, trains, buses, ferries, cars and taxis, nights in hotels, meals and activities. The crawler supplies the published factors behind that estimate. It never computes a footprint itself.

## Kinds

| Kind | Storage | One item is |
|---|---|---|
| `emission_factor` | record | one published factor, for example 0.11704 kg CO2e per passenger.km for a long-haul economy flight with RF (DESNZ 2026), 6.7 kg CO2e per room per night in France, or 99.48 kg CO2e per kg of beef from the beef herd |
| `grid_carbon_intensity` | observation | one country's electricity carbon intensity for one year or one month, in gCO2e per kWh; series `grid_ci:<ISO2>:<period>:<basis>` (`period` is `annual` or `monthly`) |
| `occupancy` | record | one published average vehicle occupancy, in persons per vehicle, for example 1.61 for the average car in Great Britain (DfT TAG, 2023 value year from NTS 2009-12 survey data, per vehicle-km, driver included). It turns a per-vehicle factor into a per-passenger one |

Each `emission_factor` record carries:

- `activity` (a snake_case slug) and `activity_label` (the publisher's own label path);
- `category`: `flights`, `rail`, `bus`, `coach`, `ferry`, `car`, `taxi`, `motorbike`, `accommodation`, `food`, `activity`, `electricity`, `fuel` or `other`;
- `geography` and `country_code`, plus `region_code` for a sub-national factor (an eGRID subregion such as `CAMX`, or a US state such as `CA`);
- `unit` and `value`, copied as published;
- `gas_basis`: `co2`, `co2e`, `co2e_with_rf` or `co2e_without_rf`;
- `scope`, `year`, `publisher`, `dataset` and the publisher's `factor_id`.

Each `occupancy` record carries:

- `mode` (`car`, `taxi`, `bus`, `coach`, `bus_coach`, `rail`, `light_rail`, `ferry`, `motorcycle`, `van`, `all_vehicles` or `other`) and `mode_label` (the publisher's own row label);
- `trip_purpose` (`all`, `work`, `commuting`, `leisure` or `other`) and `time_period` (`all_week`, `weekday` or `weekend`) where the publisher splits them;
- `geography` and `country_code`;
- `value` and `unit` (`persons per vehicle`), copied as published;
- `basis`: `per_vehicle_km` (person-km per vehicle-km, the figure to use with per-vehicle-km factors) or `per_trip`, and `includes_driver`;
- `year` (the publisher's value year, which can be later than the data), `base_years` (the survey years behind the figure, when stated) and `method_note` (how the figure was built: survey basis, projection, whether the driver count is stated or derived);
- `publisher`, `dataset` and `as_of` (the edition date when the file states one).

All three kinds accept official and reference sources only.

## Adapter `factor_table`

Each leased row is one file, fetched in a single conditional request with a size cap and spooled raw. The processor then maps the stored file row by row. It never uses an LLM, never converts units, and turns a blank cell into no record rather than 0.

Each dataset table is one `FactorMapping` in `crates/vc-sources/src/adapters/factor_table.rs`. A mapping sets:

- how to recognise the file's URL (several mappings may match one file: one per sheet or per table);
- the format: CSV, a CSV inside a ZIP, an XLSX sheet, an XLSX sheet found by a name prefix whose merged cells are filled below the header (a merged "Powertrain" cell labels every row it covers, a blank unmerged cell stays blank), or an ODS sheet (all through calamine). The header is the first row that holds every required column, with line breaks inside a header cell read as spaces;
- for a sheet that stacks several tables, the anchor: the table's title cell (`Table 10`). The header is searched after it, and the table ends at the first row whose label cell is empty or starts with "Source" or "Note";
- row filters, an earliest year, and whether a published 0 means "no figure";
- the activity label: an optional fixed prefix plus columns, with trailing footnote letters dropped where the publisher uses them;
- the value column (by name, or by a header prefix);
- the year: a column, the first year written in a column (`Apr 2024 to Mar 2025` → 2024), the value header's suffix, the title above the header (`eGRID2023`), the last year in the file name, or the year that leads the file name (`2025_Final_Residual_mix_25052026.xlsx` → 2025, so a publication date stamp does not win);
- the unit and scope (fixed text or a column);
- the category and gas-basis tables;
- the geography: fixed, a country-name column, an ISO alpha-3 or alpha-2 column, a country named in the activity label (mobitool's "Electricity mix DE & Diesel" → DE), or a region code of one country;
- for presentation sheets: a header split over up to four rows (joined cell by cell, fewest rows first: mobitool's "sum" sits three rows above its "[g CO2-eq.]" unit), and group columns whose empty (merged) cells take the value above them;
- for `occupancy`: the exact row labels that are mapped, each with its mode and trip purpose, so partial splits and goods vehicles are never stored.

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
| `od-mobitool-ch` | mobitool factors v3.1 (XLSX, PSI for the Swiss FOEN, facts only) | life-cycle g CO2e per passenger-km (GWP100a: direct, energy chain, vehicle, maintenance, end of life, road) for about 115 passenger modes: bikes and scooters, motorbikes, cars by size and powertrain, city buses, coaches, tram, Swiss trains, trains and high-speed trains in Germany, France, Italy and Austria (stored under that country), passenger ship, cable car, airplane by haul and cabin. Freight (tkm) rows are not stored |
| `carbon-aib-residual-mix` | AIB European Residual Mixes 2025 (XLSX, facts only): sheet "CO2" | direct g CO2 per kWh of the production mix, the residual mix and the total supplier mix for 34 European countries (`emission_factor`, category `electricity`; the life-cycle `grid_carbon_intensity` series stay Ember/OWID). "NA" cells give no record |
| `od-epa-supply-chain-ghg` | US EPA Supply Chain GHG Emission Factors v1.3, NAICS CSV (public domain) | kg CO2e per 2022 US dollar spent (purchaser price, with margins) for 41 travel industries: passenger air, rail and water, transit, taxi, coach, sightseeing, car rental, travel agencies and tours, shows and sports, museums and parks, amusement, golf, skiing, marinas, hotels, B&Bs, campgrounds, restaurants and bars. Spend-based (US prices): use when no activity factor fits |
| `od-owid-tourism-aviation` | OWID grapher `per-capita-co-emissions-from-international-flights-tourism-adjusted.csv` (CC BY 4.0, from OECD and UN Tourism) | kg CO2 per person per year from international flights, allocated to the departure country and adjusted for tourism, per country 2013-2024 |
| `od-dft-tag-databook` | UK DfT TAG data book (XLSM, OGL v3): sheet A1.3.3.2 | `occupancy` records: persons per vehicle per vehicle-km, all-week average, driver included, for car (work, commuting, other and average) and PSV (bus and coach together, total). "2023 values" is TAG's value year: the car figures come from NTS survey years 2009-12 and the PSV total from NTS 1999-2001 (`base_years`). The PSV driver is derived, not stated (the sheet adds 1 driver to the work share and the purposes add up to the total): subtract 1 for passengers per bus or coach. The PSV rows per purpose split the total between purposes (they add up to it), so they are not stored. The data book has no taxi or rail occupancy |

US grid factors are `emission_factor` records, not `grid_carbon_intensity` observations: the observation series is keyed per country (`grid_ci:<ISO2>:…`) and in g per kWh, and the crawler never converts units.

## Eurostat `sdg_13_31` (adapter `eurostat_jsonstat`)

`od-eea-co2-cars-avg` reads the EEA's average CO2 per km of new passenger cars from Eurostat `sdg_13_31` (CC BY 4.0, one 14 KB JSON-stat request a year). The EEA datahub item only links file shares whose lists need JavaScript. Each country and year becomes one `emission_factor` (category `car`, unit `g CO2 per km`); the EU27 aggregate is skipped. The cube's status flags are kept in `scope`: a break in series (`b`, the 2017 test-cycle change) reads `type approval; series break`, an estimate (`e`) adds `estimated`, so values across the break never look comparable; provisional cells (`p`) are not stored until final.

## Serving to the app

The read API serves all three kinds (contract: `docs/Crawler-API.md`):

- `GET /v1/carbon/factors?category=&country=&region=&year=&gas_basis=&activity=&source=&limit=&cursor=` returns current `emission_factor` records, newest factor-set year first, with the same licence shaping as search;
- `GET /v1/carbon/grid?country=&period=&basis=&from=&to=&limit=` returns every `grid_ci:<country>:…` point of one country, from every source, newest first. `/v1/observations?series=` still serves a single series;
- `GET /v1/carbon/occupancy?mode=&country=&year=&trip_purpose=&basis=&source=&limit=&cursor=` returns current `occupancy` records, newest year first, with the same licence shaping.

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
- **DfT TAG data book, carbon table A3.3:** not mapped, since it repeats the DESNZ factors. The occupancy table is on (above).
- **US NHTS occupancy:** the only published occupancy table (2022, by vehicle type) is written into the site's compiled JavaScript bundle, and its other copies are PDFs. The downloadable data is trip microdata, from which occupancy would have to be computed with survey weights, and the adapter only copies published values.
- **Electricity Maps:** it needs a free API key from the owner.
- **Third batch, looked at 2026-11-19 and left off** (robots.txt first, one sample each):
  - Cornell hotel benchmarking index: the downloads redirect to a CDN whose robots.txt answers 403.
  - Impact CO2 `equivalents.csv`: no unit denominator or reference year per row (they live in the site's JavaScript).
  - JRC Covenant of Mayors factors: PDF-only on the JRC FTP. Danish ENS standard factors: PDFs. India CEA baseline: only user-guide PDFs reachable.
  - Spanish MITECO calculator: one column per year (needs a year-columns-to-rows option). Brazil MCTI average grid factor: one block per year, not a table.
  - Canada NIR Annex 7 electricity intensity: the file API redirects to a short-lived signed URL.
  - Japan MOE emission-unit database: presentation tables, and the per-pkm sheet names product codes without values.
  - ORNL TEDB: energy per passenger-mile (Btu), not emissions.
  - EMSA THETIS-MRV: export only through the JavaScript app. Brazil GHG Protocol tool: no workbook linked.
  - greenferries and the Japan MOE calculation site: their hosts did not resolve; re-test from the crawler VM.
