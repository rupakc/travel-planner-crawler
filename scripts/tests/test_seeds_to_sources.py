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
        if not spec["enabled"]:
            continue  # disabled sources are never fetched (vc-sources validate: same rule)
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
        "https://cadataapi.state.gov/api/TravelAdvisories",
        "https://www.state.gov/",
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


def test_unreadable_robots_source_is_off(generated):
    # api.mobilitydatabase.org/robots.txt redirects to a login page: no permission, so off.
    src = generated["mobility-database-api"]
    assert src["enabled"] is False
    notes = " ".join(src["notes"].split())
    assert "robots.txt of api.mobilitydatabase.org is unreadable" in notes
    assert "When in doubt, don't crawl" in notes
    committed = yaml.safe_load((ROOT / "sources" / "mobility-database-api.yaml").read_text())
    assert committed["enabled"] is False


# --- relevance (owner rule 2026-11-17: only relevant and pertinent sites) ---------------

def test_every_spec_has_relevance_with_known_sections(generated):
    for sid, spec in generated.items():
        rel = spec["relevance"]
        assert rel["sections"], sid
        assert set(rel["sections"]) <= set(s2s.SECTIONS), sid
        assert len(rel["why"].strip()) >= s2s.MIN_WHY_CHARS, sid
        assert "\n" not in rel["why"], sid
    committed = {p.stem: yaml.safe_load(p.read_text()) for p in (ROOT / "sources").glob("*.yaml")}
    for sid, spec in committed.items():
        assert spec.get("relevance"), f"{sid}: relevance missing"


def test_sections_match_the_rust_enum():
    rust = (ROOT / "crates" / "vc-core" / "src" / "sources.rs").read_text()
    for s in s2s.SECTIONS:
        assert f'=> "{s}"' in rust, s
    enum = SCHEMA["properties"]["relevance"]["properties"]["sections"]["items"]["enum"]
    assert list(enum) == list(s2s.SECTIONS)


def test_relevance_table_has_no_stale_ids():
    assert set(s2s.RELEVANCE) == {s["id"] for s in ALLOWED}


def _seed(**kw):
    seed = {
        "id": "mystery", "category": "blogs", "name": "Mystery",
        "url": "https://mystery.example/travel/feed/", "method": "rss",
        "license": "copyright", "cadence": "daily", "needs_key": False,
        "allowed": True, "notes": "",
    }
    seed.update(kw)
    return seed


def test_seed_without_relevance_fails_loudly(tmp_path):
    seeds = tmp_path / "seeds.yaml"
    seeds.write_text(yaml.safe_dump({"seeds": [_seed()]}))
    with pytest.raises(s2s.MappingError, match="RELEVANCE"):
        s2s.generate(seeds, tmp_path / "out", ROOT / "policy" / "denylist.yaml")


@pytest.mark.parametrize(
    "rel",
    [([], "A perfectly long reason sentence."), (["nightlife"], "A perfectly long reason sentence."),
     (["tips"], "too short"), (["tips", "tips"], "A perfectly long reason sentence.")],
)
def test_bad_relevance_fails(monkeypatch, rel):
    monkeypatch.setitem(s2s.RELEVANCE, "mystery", rel)
    with pytest.raises(s2s.MappingError):
        s2s.build_spec(_seed())


def test_schema_requires_relevance(generated):
    v = _validator()
    spec = dict(generated["met-norway"])
    del spec["relevance"]
    assert list(v.iter_errors(spec)), "relevance is required"
    for bad in ({"sections": [], "why": "x" * 30}, {"sections": ["nightlife"], "why": "x" * 30},
                {"sections": ["weather"], "why": "short"}, {"sections": ["weather"], "why": "x" * 30, "extra": 1}):
        spec["relevance"] = bad
        assert list(v.iter_errors(spec)), bad


def test_travel_state_gov_sources_are_off_and_denylisted(generated):
    for sid in ("state-dept-advisories-rss", "state-dept-country-pages"):
        spec = generated[sid]
        assert spec["enabled"] is False, sid
        assert "travel.state.gov answers 403 to VoyagerBot's robots.txt (bot wall)" in " ".join(spec["notes"].split())
        for url in spec["seeds"] + spec["feeds"]:
            assert s2s.denied(url, DENYLIST) is not None, url
    # The advisories API is a sibling host, not a subdomain: still allowed and enabled.
    api = generated["state-dept-advisories-api"]
    assert api["enabled"] is True
    assert all(s2s.denied(u, DENYLIST) is None for u in api["seeds"])


VENUES = ("brokenships", "torvehallerne", "mathallen-oslo", "noma", "alchemist", "rub-og-stub")


def test_owner_added_sources_2026_11_17(generated):
    added = ("blog-thequantumexplorer", "liveriga", *VENUES)
    for sid in added:
        spec = generated[sid]
        assert spec["enabled"] is True, sid
        assert spec["licence"]["class"] == "facts_only", sid
        assert spec["min_delay_s"] >= 5, sid
        # Each stays on its own host: every URL it lists shares the seed host.
        urls = spec["seeds"] + spec["sitemaps"] + spec["feeds"]
        assert len({u.split("/")[2] for u in urls}) == 1, (sid, urls)
    for sid in VENUES:
        spec = generated[sid]
        assert spec["method"] == "html" and spec["max_depth"] == 1, sid
        assert spec["cadence"] == "monthly" and spec["daily_budget"] <= 100, sid
        assert spec["source_class"] == "editorial" and not spec["include"], sid
    riga = generated["liveriga"]
    assert riga["source_class"] == "official" and riga["max_depth"] == 2
    assert riga["include"] == [r"^https://www\.liveriga\.com/en/"]
    blog = generated["blog-thequantumexplorer"]
    assert blog["sitemaps"] == ["https://thequantumexplorer.org/sitemap.xml"]
    ct = generated["culturetrip"]
    assert ct["enabled"] is False and ct["licence"]["class"] == "facts_only"
    assert "effectively a bot wall" in " ".join(ct["notes"].split())
    assert s2s.denied(ct["sitemaps"][0], DENYLIST) is None, "no denylist entry for culturetrip"


def test_owner_kept_sources_stay_enabled(generated):
    # Owner decision 2026-11-17: these stay on, each with a relevance entry.
    for sid in ("common-crawl", "enwiki-dump", "inside-airbnb", "news-skift",
                "news-traveldailynews", "news-simpleflying"):
        assert generated[sid]["enabled"] is True, sid
        assert generated[sid]["relevance"]["sections"], sid


def test_disabled_source_may_name_a_denylisted_host(tmp_path, monkeypatch):
    monkeypatch.setitem(s2s.RELEVANCE, "mystery", (["tips"], "A perfectly long reason sentence."))
    monkeypatch.setitem(s2s.DISABLED, "mystery", "test")
    seeds = tmp_path / "seeds.yaml"
    seeds.write_text(yaml.safe_dump({"seeds": [_seed(url="https://www.booking.com/feed/")]}))
    s2s.generate(seeds, tmp_path / "out", ROOT / "policy" / "denylist.yaml")
    monkeypatch.delitem(s2s.DISABLED, "mystery")
    with pytest.raises(s2s.MappingError, match="denylisted"):
        s2s.generate(seeds, tmp_path / "out2", ROOT / "policy" / "denylist.yaml")
