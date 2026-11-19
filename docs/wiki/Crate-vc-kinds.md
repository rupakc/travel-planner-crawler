# Crate: vc-kinds

**Purpose.** `FileKindRegistry`: the data-driven registry of record kinds. Kinds are not
code. Each one is three files, which this crate loads, checks and turns into `KindDef`s:

| File | Holds |
|---|---|
| `kinds.toml` | the policy per kind: version, storage (`record` or `observation`), allowed source classes, verbatim fields, high-stakes flag, `min_reference_agreement`, `series_key` |
| `schemas/<kind>.v<N>.json` | JSON Schema draft 2020-12; also used as Vertex `responseSchema` |
| `prompts/<kind>.md` | the extraction system prompt |

Path: `crates/vc-kinds`. Dependencies: `jsonschema`, `toml`. Status: wired; the processor
loads the registry at startup.

## The 30 v1 kinds

`activity`, `advisory`, `airport`, `amenity`, `article`, `attraction`, `cheapest_fare`, `embassy`,
`emergency_numbers`, `emission_factor`, `event`, `fx_rate`, `grid_carbon_intensity`, `hotel`, `medical_facility`, `packing_hint`, `place`,
`price_level`, `public_holiday`, `qa_thread`, `restaurant`, `route`, `sim_offer`, `tip`, `transit_item`,
`transport_option`, `visa_reference`, `visa_rule`, `walking_tour`, `weather_normal`. `cheapest_fare` (series
`fare:<ORIG>:<DEST>:<YYYY-MM>`, cached fares, not live prices), `fx_rate`, `price_level` (series
`price_level:<ISO2>:<category>:<basis>`) and `weather_normal` are observations; the rest are
records. `price_level` and `public_holiday` were added on 2026-11-18 (open-data adapters).
`emission_factor` (record) and `grid_carbon_intensity` (observation, series
`grid_ci:<ISO2>:<period>:<basis>`) were added on 2026-11-19 for the trip carbon footprint
(adapter `factor_table`; see [Carbon Footprint Data](Carbon-Footprint-Data)).
`transit_item` (2026-11-18) is one public-transport operator, line or station from a GTFS
feed (`gtfs_static`): GTFS gives no city, which `transport_option` requires.

Added 2026-11-18 (adapter-only, no source lists them as `llm_kinds`):

- `embassy`: one office of one country serving travellers in another (address, phones as
  written plus E.164 only when written internationally, hours, contact form), `official` only.
- `medical_facility`: one hospital, clinic or doctor from an official list, with the list's
  title, link and own updated date (`list_updated`; some lists are years old), `official` only.
- `visa_reference`: one passport-by-destination row from a reference work (Wikipedia),
  `reference` only, `provenance: wikipedia`. Cross-check data for `visa_rule`, never the
  answer (a reference source can never produce `visa_rule`).
- `restaurant`: one place that serves food as an official food-safety register lists it
  (UK FSA FHRS/FHIS, NYC DOHMH letter grades, Toronto DineSafe), with its latest published
  rating or inspection result and that rating's date (`hygiene.rating_date`, required), `official`
  only. The licence attribution comes from the source spec like every record's.
- `amenity`: one public toilet, drinking fountain or free public Wi-Fi spot as a city lists it
  (NYC Public Restrooms, Vienna and Zürich open data), with hours, operator and stated
  wheelchair access, `official` only. Mapped by the generic `socrata` and `wfs_geojson` readers.

## What the loader refuses

Whatever the files say, the loader will not weaken the grounding contract:

- `visa_rule` and `advisory` are high-stakes and `official` only.
- `emergency_numbers` is high-stakes: `official`, or `reference` with
  `min_reference_agreement >= 2`.
- A high-stakes kind never has an empty source-class list (empty would mean "all"), and the
  flag cannot be turned off.
- Every schema is draft 2020-12, has `additionalProperties: false` on each object,
  `maxLength` or `enum` on each string, `maxItems` on each array, and uses only keywords Vertex
  understands (no `$ref`).
- Every `verbatim` pointer must resolve to a scalar. `*` segments expand to concrete indices
  `0..maxItems`, so `KindDef::verbatim_fields` holds plain JSON pointers.
- Observation kinds need a `series_key` template over their own fields (plus `{place_id}`);
  record kinds have none.
- Every `<kind>.v<N>.json` must belong to a `kinds.toml` entry.

Provenance hooks: every v1 schema has `source_url`, `as_of` and `evidence` (up to three
verbatim quotes), all verbatim-checked.

## Key items

`FileKindRegistry`, `V1_KINDS`, `HIGH_STAKES_KINDS`, `OFFICIAL_ONLY_KINDS`,
`MIN_REFERENCE_AGREEMENT`, `DRAFT_2020_12`, and the `schema_lint` checks.

## Config

`[kinds]`: paths of `kinds.toml`, the schemas directory and the prompts directory.

## Tests

- `tests/registry.rs` (the real files): `every_kind_has_schema_prompt_policy_fixture`,
  `schemas_are_valid_draft2020`, `fixture_records_validate`,
  `high_stakes_kinds_official_only`, `fx_rate_weather_normal_cheapest_fare_price_level_and_grid_ci_are_observations`,
  `series_keys_render_from_fixtures`, `verbatim_pointers_are_concrete_and_cover_provenance`,
  `prompts_treat_page_as_untrusted_and_pair_city_with_country`,
  `every_city_field_is_paired_with_a_country_field`,
  `schemas_reject_injected_extra_fields_and_oversized_strings`.
- `tests/loader.rs` (temporary files): wildcard expansion and each refusal above.

## Adding a kind

Add the schema, the prompt, the `kinds.toml` entry and a fixture in
`crates/vc-kinds/tests/fixtures/`. The registry test enforces all four.
