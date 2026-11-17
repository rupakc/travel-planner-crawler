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

## The 20 v1 kinds

`activity`, `advisory`, `airport`, `article`, `attraction`, `cheapest_fare`,
`emergency_numbers`, `event`, `fx_rate`, `hotel`, `packing_hint`, `place`, `qa_thread`,
`route`, `sim_offer`, `tip`, `transport_option`, `visa_rule`, `walking_tour`,
`weather_normal`. `cheapest_fare` (series `fare:<ORIG>:<DEST>:<YYYY-MM>`, cached fares, not
live prices), `fx_rate` and `weather_normal` are observations; the rest are records.

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
  `high_stakes_kinds_official_only`, `fx_rate_and_weather_normal_are_observations`,
  `series_keys_render_from_fixtures`, `verbatim_pointers_are_concrete_and_cover_provenance`,
  `prompts_treat_page_as_untrusted_and_pair_city_with_country`,
  `every_city_field_is_paired_with_a_country_field`,
  `schemas_reject_injected_extra_fields_and_oversized_strings`.
- `tests/loader.rs` (temporary files): wildcard expansion and each refusal above.

## Adding a kind

Add the schema, the prompt, the `kinds.toml` entry and a fixture in
`crates/vc-kinds/tests/fixtures/`. The registry test enforces all four.
