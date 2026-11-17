# Crate: vc-extract

**Purpose.** The extraction pipeline: structured data first (no LLM), then LLM extraction
under the budget, then schema validation with one repair, grounding checks, the source-class
gate, place resolution and stable record keys.

Path: `crates/vc-extract`. Dependencies: `jsonschema`, `regex`, `csv`, `flate2`. Data:
`crates/vc-extract/data/` (GeoNames subset, see `data/NOTICE`). Status: wired; the
processor role runs it on every committed capture (structured-only while the LLM is off).

## Modules and key items

| Module | Key items | What it does |
|---|---|---|
| `pipeline` | the pipeline entry points | orchestrates the steps below for one capture |
| `structured` | mapping tables | schema.org JSON-LD and microdata to kind fields, no LLM |
| `gate` | `Gate`, `source_gate`, `is_high_stakes`, `compare_with_peers`, `PeerRecord`, `PeerVerdict` | source-class gate; two-reference agreement for emergency numbers; conflict detection (rules below) |
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
   delimiters under a system prompt that calls it untrusted data. No tools. The delimiter
   nonce is a BLAKE3 hash of the page text, so a recapture of an unchanged page builds the
   identical request and hits `llm_cache`. Markers inside the page are defanged, so the
   nonce does not need to be secret.
3. **Validate**, with one repair attempt.
4. **Ground**: verbatim fields must appear in the source text; URLs must be on the source's
   own domain or in its links (own domain only for high-stakes kinds); non-HTTP schemes and
   credentials are dropped; re-validate after drops.
5. **Gate**: high-stakes kinds from non-official sources are refused before any model call.
   The floor holds even if a permissive registry is passed in: `visa_rule` and `advisory`
   are official-only in code, whatever `kinds.toml` says (it can only make them stricter).
6. **Resolve places** (min score 0.5; LLM disambiguation when the top two are within 0.15).
   A failed disambiguation call is charged its estimate unless the error never reached
   Vertex (LLM off, budget refused, breaker open, rate limited).
7. **Key** each record stably.

## Peer agreement rules

Used for `reference` records of [high-stakes kinds](Glossary), which wait (held) until a
second source agrees:

- A peer **agrees** only if it covers and equals every non-empty fact group of the record
  (for example `numbers` and the embassy phone). Agreeing on one group does not vouch for
  the rest.
- A group both carry with different values is a **conflict**; a group the peer lacks is
  neither.
- **Same place:** records with place ids match on a shared id. Records without place ids
  match only when both name the same country (country-level records such as national
  emergency numbers). Records with no location never agree or conflict, and a city-level
  record never matches a country-level one.

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
- Gate unit tests: `peer_agreeing_only_on_numbers_does_not_vouch_for_planted_embassy_phone`,
  `records_without_place_ids_never_agree`,
  `visa_rule_and_advisory_stay_official_only_whatever_kinds_toml_says`; `nonce_follows_content`.
- `tests/pipeline.rs`: `blog_cannot_produce_visa_rule`, `record_key_stable_across_runs`,
  `structured_data_needs_no_llm`, `use_policy_blocks_llm`,
  `llm_gates_unchanged_disabled_budget_restricted`, `long_text_is_chunked_and_capped`,
  `repair_fixes_invalid_record_once`, `too_many_records_are_capped`,
  `observation_kinds_get_a_series_point`, `free_price_is_grounded_by_the_word_free`.
- `tests/places.rs`: famous places and namesakes, unresolved places,
  `disambiguation_only_picks_a_candidate`, `disambiguation_needs_budget`,
  `disambiguation_errors_charge_the_estimate_unless_never_sent`, small attributed data files.

More: [Processing and Extraction](Processing-and-Extraction).
