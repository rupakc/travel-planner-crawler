# vc-parse

Content handlers for captured bodies (spec §5.7 step 1), behind `vc_core::parse::ContentHandler`:
HTML (main text via dom_smoothie, JSON-LD + microdata, links with anchor text, meta robots /
TDMRep signals, language, simhash, SPA detection), RSS/Atom, XML/text sitemaps and indexes
(gzip too), JSON and CSV. `vc_parse::registry()` builds the `HandlerRegistry`;
`vc_parse::parse()` dispatches a capture on its stored media type.

Handlers never fetch, refuse XML DTDs (`Error::Policy`), cap DOM depth and node count before
anything walks the tree, and cap links, text, sitemap entries and gzip output (`Limits`).

## Tests

```bash
CARGO_BUILD_JOBS=3 cargo test -p vc-parse
```

## Fuzzing

The cargo-fuzz targets live in `fuzz/`, a separate workspace (not a member of the root one).
They need a nightly toolchain and `cargo install cargo-fuzz`. Smoke run, about a minute per
target, from this directory:

```bash
for t in fuzz_html fuzz_sitemap fuzz_feed; do
  cargo +nightly fuzz run "$t" -- -max_total_time=60 -rss_limit_mb=2048 || exit 1
done
```

`fuzz/corpus/<target>/seed-*` are the committed seeds; libFuzzer adds its own corpus files
next to them (git-ignored). Crashes land in `fuzz/artifacts/<target>/`.
