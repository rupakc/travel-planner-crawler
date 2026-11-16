"""Tests for scripts/seeds_to_sources.py (plan Task 0B, step 1)."""

from __future__ import annotations

import json
import sys
from pathlib import Path

import jsonschema
import pytest
import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import seeds_to_sources as s2s  # noqa: E402

SCHEMA = json.loads((ROOT / "schemas" / "source.schema.json").read_text())
SEEDS = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
ALLOWED = [s for s in SEEDS if s["allowed"]]
DENYLIST = s2s.load_denylist(ROOT / "policy" / "denylist.yaml")


@pytest.fixture(scope="module")
def generated(tmp_path_factory):
    out = tmp_path_factory.mktemp("sources")
    s2s.generate(ROOT / "docs" / "research" / "seeds.yaml", out, ROOT / "policy" / "denylist.yaml")
    return {p.stem: yaml.safe_load(p.read_text()) for p in sorted(out.glob("*.yaml"))}


def _validator():
    jsonschema.Draft202012Validator.check_schema(SCHEMA)
    return jsonschema.Draft202012Validator(SCHEMA, format_checker=jsonschema.FormatChecker())


def test_every_allowed_seed_yields_a_valid_file(generated):
    assert sorted(generated) == sorted(s["id"] for s in ALLOWED)
    v = _validator()
    for sid, spec in generated.items():
        errors = sorted(v.iter_errors(spec), key=str)
        assert not errors, f"{sid}: {[e.message for e in errors]}"


def test_committed_sources_are_valid_and_match_generation(generated):
    v = _validator()
    committed = {p.stem: yaml.safe_load(p.read_text()) for p in (ROOT / "sources").glob("*.yaml")}
    for sid, spec in committed.items():
        assert spec["id"] == sid, f"file name must equal id: {sid}"
        errors = sorted(v.iter_errors(spec), key=str)
        assert not errors, f"{sid}: {[e.message for e in errors]}"
    # Every seed-derived file is committed. (Hand edits after P0 are allowed, so no byte compare.)
    assert set(generated) <= set(committed)


def test_disallowed_seeds_produce_no_file(generated):
    for s in SEEDS:
        if not s["allowed"]:
            assert s["id"] not in generated


def test_no_file_host_is_denylisted(generated):
    for sid, spec in generated.items():
        for url in spec["seeds"] + spec["sitemaps"] + spec["feeds"]:
            assert s2s.denied(url, DENYLIST) is None, f"{sid}: {url} is denylisted"


def test_every_disallowed_seed_is_denylisted():
    for s in SEEDS:
        if not s["allowed"]:
            assert s2s.denied(s["url"], DENYLIST) is not None, f"{s['id']} ({s['url']}) missing from denylist"


@pytest.mark.parametrize(
    "url",
    [
        "https://www.booking.com/hotel/x.html",
        "https://www.expedia.co.uk/",
        "https://tripadvisor.de/",
        "https://www.google.com/travel/flights?q=x",
        "https://m.reddit.com/r/travel",
        "https://www.lonelyplanet.com/thorntree/forums",
    ],
)
def test_denylist_matches_brand_domains_and_paths(url):
    assert s2s.denied(url, DENYLIST) is not None


@pytest.mark.parametrize(
    "url",
    [
        "https://insideairbnb.com/get-the-data/",
        "https://www.google.com/search",
        "https://www.lonelyplanet.com/sitemaps/google-news/sitemap.xml",
        "https://api.stackexchange.com/2.3/questions",
        "https://www.eventbriteapi.com/v3/",
    ],
)
def test_denylist_does_not_overmatch(url):
    assert s2s.denied(url, DENYLIST) is None


def test_wikivoyage_is_share_alike(generated):
    hits = [s for sid, s in generated.items() if sid.startswith("wikivoyage")]
    assert hits
    for spec in hits:
        assert spec["licence"]["class"] == "share_alike"


def test_fcdo_is_open_and_official(generated):
    hits = [s for sid, s in generated.items() if sid.startswith("fcdo")]
    assert len(hits) >= 2
    for spec in hits:
        assert spec["licence"]["class"] == "open"
        assert spec["source_class"] == "official"


def test_ticketmaster_is_api_terms_commercial_api(generated):
    spec = generated["ticketmaster"]
    assert spec["licence"]["class"] == "api_terms"
    assert spec["source_class"] == "commercial_api"
    assert spec["needs_key"] is True and spec["key_secret"]
    assert spec["display_requirements"]


def test_nager_date_is_api_terms(generated):
    assert generated["nager-date"]["licence"]["class"] == "api_terms"


def test_restricted_sources_are_disabled(generated):
    restricted = [sid for sid, s in generated.items() if s["licence"]["class"] == "restricted"]
    assert {"open-meteo", "meteostat"} <= set(restricted)
    for sid in restricted:
        assert generated[sid]["enabled"] is False, sid


def test_non_commercial_licences_are_restricted(generated):
    for s in ALLOWED:
        lic = s["license"].lower()
        if "non-commercial" in lic or "-nc" in lic:
            assert generated[s["id"]]["licence"]["class"] == "restricted", s["id"]


def test_open_meteo_disabled_met_norway_enabled(generated):
    assert generated["open-meteo"]["enabled"] is False
    met = generated["met-norway"]
    assert met["enabled"] is True
    assert met["licence"]["class"] == "open"
    assert "weather_normal" in met["kinds"]


def test_blogs_and_news_are_facts_only_editorial(generated):
    for sid, spec in generated.items():
        if sid.startswith(("blog-", "news-")):
            assert spec["licence"]["class"] == "facts_only", sid
            assert spec["source_class"] == "editorial", sid
            assert spec["method"] == "rss" and spec["feeds"], sid


def test_wikipedia_and_wikidata_are_reference_not_official(generated):
    for sid in ("enwiki-dump", "wikipedia-geosearch", "wikidata-aviation", "wikidata-dump"):
        assert generated[sid]["source_class"] == "reference", sid


def test_llm_kinds_subset_of_kinds_and_known(generated):
    for sid, spec in generated.items():
        assert set(spec["llm_kinds"]) <= set(spec["kinds"]), sid
        assert set(spec["kinds"]) <= s2s.KINDS, sid


def test_high_stakes_kinds_only_from_allowed_classes(generated):
    for sid, spec in generated.items():
        kinds = set(spec["kinds"])
        if kinds & {"visa_rule", "advisory"}:
            assert spec["source_class"] == "official", sid
        if "emergency_numbers" in kinds:
            assert spec["source_class"] in {"official", "reference"}, sid


def test_forums_v1_only_se_dump(generated):
    for sid, spec in generated.items():
        if spec["category"] == "forums":
            assert spec["enabled"] is (sid == "se-dump"), sid


def test_urls_have_no_templates(generated):
    for sid, spec in generated.items():
        for url in spec["seeds"] + spec["sitemaps"] + spec["feeds"]:
            assert "{" not in url and "}" not in url, sid


def test_generation_is_deterministic(tmp_path):
    a, b = tmp_path / "a", tmp_path / "b"
    for d in (a, b):
        s2s.generate(ROOT / "docs" / "research" / "seeds.yaml", d, ROOT / "policy" / "denylist.yaml")
    for p in a.glob("*.yaml"):
        assert p.read_bytes() == (b / p.name).read_bytes()


def test_unknown_licence_string_fails_loudly(tmp_path):
    seeds = tmp_path / "seeds.yaml"
    seeds.write_text(
        yaml.safe_dump(
            {
                "seeds": [
                    {
                        "id": "mystery",
                        "category": "blogs",
                        "name": "Mystery",
                        "url": "https://mystery.example/feed/",
                        "method": "rss",
                        "license": "Some new licence nobody mapped",
                        "cadence": "daily",
                        "needs_key": False,
                        "allowed": True,
                        "notes": "",
                    }
                ]
            }
        )
    )
    with pytest.raises(s2s.MappingError):
        s2s.generate(seeds, tmp_path / "out", ROOT / "policy" / "denylist.yaml")


@pytest.mark.parametrize(
    "sid,host",
    [
        ("wikidata-aviation", "query.wikidata.org"),
        ("mobility-database", "files.mobilitydatabase.org"),
        ("nager-date", "date.nager.at"),
    ],
)
def test_robots_disallowed_sources_are_off(generated, sid, host):
    # Their robots.txt disallows us (every agent): off, with the documented reason.
    src = generated[sid]
    assert src["enabled"] is False
    assert f"robots.txt of {host} disallows" in " ".join(src["notes"].split())
