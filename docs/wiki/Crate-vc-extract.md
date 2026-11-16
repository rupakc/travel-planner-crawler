# Crate: vc-extract

**Purpose.** The extraction pipeline: structured data first (no LLM), then LLM extraction
under the budget, then schema validation with one repair, grounding checks, the source-class
gate, place resolution and stable record keys.

Path: `crates/vc-extract`. Dependencies: `jsonschema`, `regex`, `csv`, `flate2`. Data:
`crates/vc-extract/data/` (GeoNames subset, see `data/NOTICE`).

## Modules and key items

| Module | Key items | What it does |
|---|---|---|
| `pipeline` | the pipeline entry points | orchestrates the steps below for one capture |
| `structured` | mapping tables | schema.org JSON-LD and microdata to kind fields, no LLM |
| `gate` | `Gate`, `source_gate`, `is_high_stakes`, `compare_with_peers`, `PeerRecord`, `PeerVerdict` | source-class gate; two-reference agreement for emergency numbers; conflict detection |
| `grounding` | `ground`, `GroundOpts`, `GroundReport`, `GroundFailure`, `SourceEvidence`, `normalise_url` | verbatim and URL checks; drops fields or whole array elements |
| `places` | `FilePlaceResolver`, `LlmPlaceDisambiguator` | GeoNames resolution; LLM only to choose among close candidates, under budget |
| `record_key` | key functions | `@id` or anchor, else hash of (kind, name, date/geo), else per-page singleton |
| `fold`, `fold_table` | `fold` | accent and case folding for names |

## The steps

1. **Structured first.** Map JSON-LD and microdata to kind records with fixed tables; fit to
   the schema; drop if still invalid.
2. **LLM second**, only if the payload changed, the source wants the kind, UsePolicy allows
   LLM use, the source class is allowed, and the ledger reserves the cost. Input ≤ 1,200
   tokens × 3 chunks, output ≤ 400 tokens, at most 20 records per call. Page text sits in
   delimiters under a system prompt that calls it untrusted data. No tools.
3. **Validate**, with one repair attempt.
4. **Ground**: verbatim fields must appear in the source text; URLs must be on the source's
   own domain or in its links (own domain only for high-stakes kinds); non-HTTP schemes and
   credentials are dropped; re-validate after drops.
5. **Gate**: high-stakes kinds from non-official sources are refused before any model call.
   The floor holds even if a permissive registry is passed in.
6. **Resolve places** (min score 0.5; LLM disambiguation when the top two are within 0.15).
7. **Key** each record stably.

Contains data from GeoNames (geonames.org), CC BY 4.0.

## Config

`[extract]`: GeoNames file paths, repair attempts, excerpt length, minimum quality (0.3),
canary size (200), records per call (20), estimate prices, place score and margin. LLM limits
are in `[llm]`.

## Tests

- `tests/injection.rs`: **`injection_fixtures`** (fake emergency number, phishing URL,
  "ignore previous instructions"), `high_stakes_floor_ignores_permissive_registry`,
  `high_stakes_urls_must_be_own_domain_even_if_linked`,
  `reference_emergency_numbers_accepted_when_two_sources_agree`.
- `tests/pipeline.rs`: `blog_cannot_produce_visa_rule`, `record_key_stable_across_runs`,
  `structured_data_needs_no_llm`, `use_policy_blocks_llm`,
  `llm_gates_unchanged_disabled_budget_restricted`, `long_text_is_chunked_and_capped`,
  `repair_fixes_invalid_record_once`, `too_many_records_are_capped`,
  `observation_kinds_get_a_series_point`, `free_price_is_grounded_by_the_word_free`.
- `tests/places.rs`: famous places and namesakes, unresolved places,
  `disambiguation_only_picks_a_candidate`, `disambiguation_needs_budget`, small attributed
  data files.

More: [Processing and Extraction](Processing-and-Extraction).
