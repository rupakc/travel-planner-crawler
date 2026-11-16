# Crate: vc-parse

**Purpose.** Content handlers, one per media type, behind `vc_core::parse::ContentHandler`.
They turn a captured body into links, main text and structured data. Handlers never fetch.

Path: `crates/vc-parse`. Dependencies: `scraper`, `dom_smoothie` (main-content extraction),
`quick-xml`, `feed-rs`, `whatlang` (language), `encoding_rs`, `csv`, `flate2`.

## Handlers

| Handler | Media | Output |
|---|---|---|
| `HtmlHandler` | `html` | title, main text, language, links with anchor text, JSON-LD + microdata, meta robots and TDMRep signals, simhash, `needs_render` |
| `FeedHandler` | `rss`, `atom` | title, language, one `feed_item` link per entry |
| `SitemapHandler` | `sitemap`, `sitemap_text` | `page` links (`<urlset>`) or `sitemap` links (`<sitemapindex>`), with `lastmod`; gzip accepted |
| `JsonHandler` | `json` | JSON-LD flattened, else the whole document as one value |
| `CsvHandler` | `csv` | rows as objects keyed by the header, up to 50,000 rows |

## Key functions

| Function | What it does |
|---|---|
| `registry()`, `registry_with(cfg)` | build the `HandlerRegistry` |
| `parse(...)` | dispatch a capture on its stored media type |
| `sniff(body)` | guess the media type from the bytes |
| `limits_from(cfg)` | build `Limits` from `[parse]` |
| `simhash64(text)`, `hamming(a, b)` | near-duplicate fingerprints |

HTML details: legacy charsets are decoded; language comes from the `lang` attribute or from
the text; a page with less than 200 characters of main text, or an SPA shell, gets
`needs_render`.

## Safety

- XML with a DTD is refused with `Error::Policy` (no entity expansion, no external
  entities).
- The DOM is pruned to `max_dom_depth` (256) and `max_nodes` (200,000) before anything walks
  it.
- Links, text, sitemap entries, structured objects and gzip output are capped.
- The CPU budget (5 s) is checked between stages; exceeding it is a permanent error.
- Handlers never log page content.

## Config

`[parse]`: DOM depth and nodes, text bytes, links, sitemap entries, CPU time, render
threshold.

## Tests

`tests/handlers.rs`: `jsonld_event_extracted`, `microdata_place_extracted`,
`sitemap_index_and_gz`, `feed_rss_and_atom`, `meta_noindex_signal`,
`xml_entity_expansion_rejected`, `dom_depth_capped`, `dom_node_count_capped`,
`html_links_text_lang_and_canonical`, `spa_shell_needs_render`, `main_text_capped_at_limit`,
`links_capped_at_limit`, `legacy_charset_decoded`, JSON and CSV handlers, registry dispatch
and sniffing, simhash properties.

Fuzz targets (separate workspace in `fuzz/`, nightly): `fuzz_html`, `fuzz_sitemap`,
`fuzz_feed`:

```bash
cd crates/vc-parse
for t in fuzz_html fuzz_sitemap fuzz_feed; do
  cargo +nightly fuzz run "$t" -- -max_total_time=60 -rss_limit_mb=2048 || exit 1
done
```

## Limits

No PDF handler in v1.
