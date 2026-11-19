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
- `geography` and `country_code`;
- `unit` and `value`, copied as published;
- `gas_basis`: `co2`, `co2e`, `co2e_with_rf` or `co2e_without_rf`;
- `scope`, `year`, `publisher`, `dataset` and the publisher's `factor_id`.

Both kinds accept official and reference sources only.

## Adapter `factor_table`

Each leased row is one file, fetched in a single conditional request with a size cap and spooled raw. The processor then maps the stored file row by row. It never uses an LLM, never converts units, and turns a blank cell into no record rather than 0.

Each dataset is one `FactorMapping` in `crates/vc-sources/src/adapters/factor_table.rs`. A mapping sets:

- how to recognise the file's URL;
- the format: CSV, or an XLSX sheet whose header is found by its required columns;
- row filters;
- the activity-label columns;
- the value column (by name, or by a header prefix that carries the year);
- the unit;
- the category and gas-basis tables;
- the geography: fixed, a country-name column, or an ISO alpha-3 column.

| Source | File | Yields |
|---|---|---|
| `od-desnz-ghg-factors` | UK DESNZ GHG conversion factors, flat file (XLSX, OGL v3, yearly in June) | kg CO2e factors for flights by haul and cabin (with and without RF), ferry, rail, bus, taxi, motorbike, cars, hotel stays per country, and UK grid electricity |
| `od-owid-food-ghg` | OWID grapher `ghg-per-kg-poore.csv` (CC BY 4.0) | kg CO2e per kg for each food product (Poore and Nemecek 2018, global) |
| `carbon-owid-grapher-ci` | OWID grapher `electricity-mix.csv`, metric `carbon_intensity` (CC BY 4.0) | gCO2e per kWh per country and year (life-cycle; aggregates skipped) |

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
- **US EPA eGRID and EF Hub, and the Australian NGA factors:** these are presentation workbooks that need per-table mappings.
- **AGRIBALYSE:** it needs a Dataverse API walk.
- **Electricity Maps:** it needs a free API key from the owner.
