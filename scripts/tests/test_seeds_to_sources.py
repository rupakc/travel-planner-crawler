"""Tests for scripts/seeds_to_sources.py (plan Task 0B, step 1)."""

from __future__ import annotations

import json
import re
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

# Refused VoyagerBot on the first full-crawl day (2026-11-19): off in production, out of every
# ramp. Non-government hosts are denylisted; government ones (and TravelDailyNews, which the
# owner keeps) stay off for a later re-test.
FULL_CRAWL_REFUSED_2026_11_19 = {
    "aphis-pet-travel",
    "blog-coasttocoast",
    "blog-frequentmiler",
    "blog-viewfromthewing",
    "city-bordeaux",
    "city-dresden",
    "city-salzburg",
    "city-winnipeg",
    "deals-fly4free",
    "gstc-certified-hotels",
    "guide-wanderlush",
    "news-traveldailynews",
    "official-octopus",
    "park-usfs",
    "pl-sentosa",
    "ref-iaato",
    "tb-angkor-enterprise",
    "tb-brunei",
    "tb-cook-islands",
    "tb-guadeloupe",
    "tb-iles-de-la-madeleine",
    "tb-kodiak",
    "tb-los-lagos",
    "tb-malta",
    "tb-meet-in-shanghai",
    "tb-sabah",
    "tb-tahiti",
    "tb-tuvalu",
    "tb-vermont",
    "tb-wesgro",
    "tr-hkia",
    "tr-massport",
    "tr-miaa-ph",
    "tr-vy",
    "guide-caravanistan",
    "visa-imigrasi-id",
    "np-port-cros",
    "tb-liechtenstein",
    "news-atlasobscura",
    "mag-capetownmagazine",
    "visa-bh-evisa",
    "tr-metro-cl",
    "tb-minas",
    "gov-symi",
    "tb-tourisme-ci",
    "city-lyon",
    "gov-pt-vistos",
    "tb-ktb-turkey",
    "tr-mwaa",
    "ev-nhb-sg",
    "ev-roots-sg",
    "city-ljubljana",
}
FULL_CRAWL_REFUSED_PREFIXES = ("challenge page at first full crawl 2026-11-19",
                               "robots.txt 403 at first full crawl 2026-11-19",
                               "robots.txt challenge at first full crawl 2026-11-19")


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
        # Personal/non-commercial *terms* the owner accepted (2026-11-18) are facts only.
        if ("non-commercial" in lic or "-nc" in lic) and "owner accepted" not in lic:
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
            if sid in BLOGS_WITHOUT_A_FEED:
                # The site has no working feed (2026-11-18): its post sitemap, posts only.
                assert spec["method"] == "sitemap" and spec["sitemaps"] and spec["include"], sid
                assert "sitemap is read" in " ".join(spec["notes"].split()), sid
            else:
                assert spec["method"] == "rss" and spec["feeds"], sid


BLOGS_WITHOUT_A_FEED = {"blog-thriftynomads", "blog-expertvagabond", "blog-bootsnall", "blog-outsideonline",
                        "blog-twoweekscr",  # robots.txt disallows */feed/ (2026-11-18)
                        "news-tourismupdate",  # no feed; article sitemap (2026-11-18)
                        "blog-insidekyoto"}  # post sitemap only (2026-11-18)


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
        assert generated[sid]["relevance"]["sections"], sid
        if sid == "news-traveldailynews":
            # Challenge page at the first full crawl (2026-11-19): kept, off until a re-test.
            notes = " ".join(generated[sid]["notes"].split())
            assert generated[sid]["enabled"] is False and "re-test later" in notes, sid
            assert not any(e["host"] == "traveldailynews.com" for e in DENYLIST)
            continue
        assert generated[sid]["enabled"] is True, sid


def test_disabled_source_may_name_a_denylisted_host(tmp_path, monkeypatch):
    monkeypatch.setitem(s2s.RELEVANCE, "mystery", (["tips"], "A perfectly long reason sentence."))
    monkeypatch.setitem(s2s.DISABLED, "mystery", "test")
    seeds = tmp_path / "seeds.yaml"
    seeds.write_text(yaml.safe_dump({"seeds": [_seed(url="https://www.booking.com/feed/")]}))
    s2s.generate(seeds, tmp_path / "out", ROOT / "policy" / "denylist.yaml")
    monkeypatch.delitem(s2s.DISABLED, "mystery")
    with pytest.raises(s2s.MappingError, match="denylisted"):
        s2s.generate(seeds, tmp_path / "out2", ROOT / "policy" / "denylist.yaml")


# --- batch 2 (research 2026-11-17) ------------------------------------------------------

BATCH2_DENIED = [
    "https://library.traveldoc.aero/", "https://infopages.traveldoc.aero/x", "https://timaticweb2.com/",
    "https://www.ica.gov.sg/enter-transit-depart", "https://www.henleyglobal.com/passport-index",
    "https://www.passportindex.org/", "https://bus-data.dft.gov.uk/", "https://www.transit.dot.gov/ntd",
    "https://www.mofa.go.jp/j_info/visit/visa/", "https://www.dha.gov.za/", "https://www.mpi.govt.nz/",
    "https://www.usembassy.gov/", "https://fr.usembassy.gov/", "https://www.ricksteves.com/forums",
    "https://www.welcomenepal.com/", "https://www.timeout.com/london", "https://guide.michelin.com/en",
    "https://www.theinfatuation.com/", "https://www.happycow.net/", "https://ra.co/events",
    "https://www.officeholidays.com/", "https://www.visitwales.com/", "https://www.visitsingapore.com/",
    "https://www.thisisbarcelona.com/", "https://www.visitbarcelona.com/", "https://www.afar.com/",
    "https://www.nationalgeographic.com/travel", "https://www.visitbritain.com/", "https://www.visitnorway.com/",
    "https://www.visittheusa.com/", "https://visitbrasil.com/", "https://www.visitmelbourne.com/",
    "https://www.sftravel.com/", "https://www.fiji.travel/", "https://experienceegypt.eg/",
    "https://www.discoverhongkong.com/", "https://www.roadsideamerica.com/", "https://www.spottedbylocals.com/",
    "https://www.visitlondon.com/", "https://magicalkenya.com/", "https://www.capetown.travel/",
    "https://www.embassypages.com/", "https://opencellid.org/", "https://www.opensignal.com/",
    "https://www.iata.org/en/", "https://www.mea.gov.in/", "https://www.exteriores.gob.es/",
    "https://download.gtfs.de/germany/fv_free/latest.zip", "https://static.data.gov.hk/td/x.zip",
    "https://www.ireland.ie/en/", "https://www.transportation.gov/", "https://whatson.cityofsydney.nsw.gov.au/",
    # scam-safety research 2026-11-17: 403 to an honest bot
    "https://www.worldpackers.com/articles/travel-scams", "https://www.moneycrashers.com/travel-scams/",
    "https://www.eurochange.co.uk/travel-money/guides", "https://travelersunited.org/",
    "https://consumer.ftc.gov/articles/travel-scams",
    # bot walls met in production, 2026-11-17
    "https://taxation-customs.ec.europa.eu/travelling_en", "https://www.mfat.govt.nz/en/embassies",
    "https://mfat.govt.nz/", "https://secretldn.com/feed/", "https://www.secretldn.com/",
    # bot walls met in production, 2026-11-18
    "https://philippines.travel/sitemap.xml", "https://www.dubaicustoms.gov.ae/en/",
    "https://chicagoonthecheap.com/feed/", "https://freetoursbyfoot.com/sitemaps.xml",
    "https://www.nycgovparks.org/xml/events_300_rss.xml",
    # alternatives research 2026-11-18
    "https://www.visitoslo.com/", "https://dados.gov.br/", "https://www.sanparks.org/",
    "https://www.biosecurity.govt.nz/", "https://www.data.govt.nz/", "https://www.dubai.ae/",
    "https://www.us.emb-japan.go.jp/", "https://intramuros.gov.ph/",
    "https://www.agenziadoganemonopoli.gov.it/", "https://www.newzealand.com/",
    "https://www.douane.gouv.fr/", "https://www.mfa.gov.sg/",
    # sustainable/eco travel research 2026-11-18: terms bans and bot walls
    "https://whc.unesco.org/en/list/xml/", "https://www.unesco.org/en/terms-use",
    "https://www.visitfinland.com/en/", "https://www.responsibletravel.com/", "https://www.greentraveller.co.uk/",
    "https://www.kindtraveler.com/", "https://ecobnb.com/", "https://ecobnb.it/", "https://www.interrail.eu/",
    "https://www.interrail.com/", "https://www.eurail.com/", "https://www.chronotrains.com/",
    "https://www.greendestinations.org/", "https://www.visitportugal.com/", "https://www.nationaltrust.org.uk/",
    "https://darksky.org/", "https://www.slovenia.info/", "https://www.thetravelfoundation.org.uk/",
    "https://www.earth-changers.com/", "https://www.ademe.fr/", "https://sustainablehotelnews.com/",
    "https://www.ecotourismkenya.org/", "https://lnt.org/why/7-principles/",
    # budget travel research 2026-11-18: terms bans and bot walls
    "https://thebrokebackpacker.com/", "https://wanderingearl.com/feed/", "https://secretflying.com/feed/",
    "https://going.com/", "https://upgradedpoints.com/", "https://budgettravelbuff.com/",
    "https://mochileandoporelmundo.com/", "https://www.getaway.co.za/", "https://12go.asia/",
    "https://www.homeexchange.com/", "https://www.refill.org.uk/", "https://www.which.co.uk/",
    "https://www.ryanair.com/", "https://www.flixbus.com/", "https://global.flixbus.com/",
    "https://www.flixbus.de/", "https://gtfs.gis.flix.tech/gtfs_generic_eu.zip", "https://www.blablacar.com/",
    "https://www.hostelz.com/", "https://www.yha.org.uk/", "https://madmonkeyhostels.com/feed/",
    "https://www.tripzilla.com/feed/", "https://talesofabackpacker.com/", "https://nomadicbackpacker.com/",
    "https://www.moneysavingexpert.com/", "https://moneysmart.gov.au/", "https://www.moneyhelper.org.uk/",
    "https://www.expatistan.com/", "https://www.si.edu/", "https://www.parismuseumpass.fr/",
    "https://www.ratp.fr/", "https://www.jreast.co.jp/", "https://www.nyc.gov/", "https://data.buenosaires.gob.ar/",
    "https://www.routesonline.com/",
    # North America research 2026-11-18: terms ban robots, scraping or AI use (bot walls seen
    # from the research IP are not denylisted: they may be regional)
    "https://www.visitflorida.com/", "https://www.enjoyillinois.com/", "https://visitseattle.org/",
    "https://www.denver.org/", "https://www.visitdenver.com/", "https://www.visitdallas.com/",
    "https://baltimore.org/", "https://www.visitnc.com/", "https://www.visitorlando.com/",
    "https://ottawatourism.ca/", "https://www.gotobermuda.com/", "https://www.soundtransit.org/",
    "https://www.mbta.com/fares", "https://www.parks.ca.gov/", "https://data.calgary.ca/",
    "https://www.calgary.ca/", "https://roadtrippers.com/", "https://www.eater.com/",
]

# Hosts the crawler uses (or will use) that no denylist entry may catch.
APPROVED = [
    "https://www.gov.uk/api/content/foreign-travel-advice", "https://www.gov.uk/duty-free-goods",
    "https://cadataapi.state.gov/api/TravelAdvisories", "https://data.international.gc.ca/travel-voyage/x.json",
    "https://www.canada.ca/en/immigration-refugees-citizenship/", "https://www.cbsa-asfc.gc.ca/travel-voyage/",
    "https://inspection.canada.ca/en/", "https://www.cbp.gov/travel/", "https://www.aphis.usda.gov/pet-travel",
    "https://home-affairs.ec.europa.eu/", "https://food.ec.europa.eu/",
    "https://travel-europe.europa.eu/en/etias", "https://www.immigration.govt.nz/", "https://www.customs.govt.nz/",
    "https://www.customs.go.jp/english/", "https://www.maff.go.jp/aqs/english/",
    # europa.eu: only taxation-customs.ec.europa.eu is denied (2026-11-17)
    "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml",
    "https://home-affairs.ec.europa.eu/policies/schengen/visa-policy_en",
    "https://food.ec.europa.eu/animals/movement-pets_en", "https://eur-lex.europa.eu/eli/reg/2018/1806",
    "https://ec.europa.eu/eurostat/api/dissemination/statistics/1.0/data/prc_ppp_ind",
    "https://www.nps.gov/sitemap.xml", "https://www.visitscotland.com/", "https://www.discoverireland.ie/",
    "https://whatson.melbourne.vic.gov.au/", "https://www.sydney.com/", "https://www.gotokyo.org/",
    "https://www.gaultmillau.com/", "https://data.gov.hk/en-data/", "https://gtfs.de/", "https://smartraveller.gov.au/",
    "https://wwwnc.cdc.gov/travel", "https://www.iatatravelcentre.com.example/", "https://chile.travel/",
    "https://www.spain.info/en/", "https://www.australia.com/en/", "https://freewalkingtour.com/",
    # scam-safety sources (2026-11-17) and ftc.gov outside consumer.ftc.gov
    "https://travelerset.com/blog/common-travel-scams-and-how-to-avoid-them",
    "https://avoidtravelscam.com/resources/", "https://avoidtravelscam.com/scams/",
    "https://twopassportspacked.com/common-travel-scams/",
    "https://www.smartraveller.gov.au/news-and-updates/stop-check-protect-and-keep-scammers-out-your-travel-plans",
    "https://www.ftc.gov/", "https://ftc.gov/", "https://reportfraud.ftc.gov/",
    # alternatives to bot-walled sites (2026-11-18)
    "https://www.safetravel.govt.nz/destinations/japan",
    "https://europa.eu/youreurope/citizens/travel/carry/index_en.htm",
    "https://www.revenue.ie/en/customs/individuals/info-travelling-duty-free/index.aspx",
    "https://icp.gov.ae/sitemap.xml", "https://egymonuments.gov.eg/en/sitemap.aspx",
    "https://kws.go.ke/wp-sitemap-posts-park-1.xml", "https://dirco.gov.za/page-sitemap.xml",
    "https://www.capetown.gov.za/sitemap.xml", "https://cadataapi.state.gov/api/CountryTravelInformation",
    "https://api.ratings.food.gov.uk/Establishments", "https://data.cityofnewyork.us/resource/43nn-pn8j.json",
    "https://catalogue.data.govt.nz/", "https://www.customs.gov.sg/",
    # neighbours of the sustainable/budget denylist entries (2026-11-18) that stay allowed
    "https://datahub.visitfinland.com/", "https://transport.data.gouv.fr/resources/11681",
    "https://transport-data-gouv-fr-resource-history-prod.cellar-c2.services.clever-cloud.com/11681/x.zip",
    "https://data.cityofnewyork.us/resource/fudw-fgrp.json",
    "https://turismo.buenosaires.gob.ar/en", "https://www.smithsonianmag.com/travel/",
    "https://feeds.feedburner.com/InstinctVoyageur", "https://www.korail.com/ebizbf/EbizBfKrPassAbout.do",
    "https://www.green-tourism.com/our-members/", "https://youth.europa.eu/discovereu_en",
    "https://www.doc.govt.nz/parks-and-recreation/places-to-go/", "https://www.nationalparks.nsw.gov.au/things-to-do",
    "https://travelifestaybetter.com/", "https://www.greenglobe.com/sitemap.xml",
    "https://www.parismusees.paris.fr/en/les-musees-de-la-ville-de-paris",
    # neighbours of the North America denylist entries (2026-11-18) that stay allowed
    "https://cdn.mbta.com/MBTA_GTFS.zip", "https://www.visitcalgary.com/events", "https://www.denvergov.org/",
    "https://parks.ny.gov/sitemap.xml", "https://parks.canada.ca/pn-np", "https://visitindiana.in.gov/sitemap.xml",
    "https://admin.tourismvictoria.com/sitemap.xml", "https://www.supernaturalbc.com/sitemap.xml",
    "https://data.cityofchicago.org/resource/vsdy-d8k7.json", "https://www.flysfo.com/sitemap.xml",
]


# Sustainable/eco and budget travel (research 2026-11-18). Every row is a source; these are
# off, each with its reason prefix. Every other row of the batch is enabled.
SUSTAIN_BUDGET_OFF = {
    "ontd-night-trains": "adapter pending", "eu-ecolabel-ecat": "adapter pending",
    "data-gouv-ecolabel-fr": "adapter pending", "bigmac-index": "adapter pending",
    "sg-hawker-centres": "access route", "flixbus-gtfs": "adapter pending",
    "doc-nz-api": "needs an API key", "greenseal-api": "needs an API key",
    "icao-carbon": "needs an API key", "seoul-wifi": "needs an API key",
    "blueflag-gr": "seed unresolved", "mavi-bayrak-tr": "seed unresolved",
    "botswana-ecocert": "seed unresolved", "cst-costa-rica": "seed unresolved",
    "nordic-swan-hotels": "seed unresolved", "umweltzeichen-tourism": "seed unresolved",
    "tourcert": "seed unresolved", "travelife-hotels": "seed unresolved",
    "earthcheck": "seed unresolved", "greenkey-global-ca": "seed unresolved",
    "ecotourism-kenya-ecorated": "bot wall", "lnt-org": "bot wall",
    "blog-thesmartlocal": "relevance",
    # bot walls met in production at ramp end, 2026-11-18
    "blog-chicagoonthecheap": "bot wall", "walking-freetoursbyfoot": "bot wall",
    "official-nycparks-events": "bot wall",
}
# Crawl-delay from robots.txt (or the europa.eu default) -> min_delay_s.
SUSTAIN_BUDGET_DELAYS = {
    "gstc-certified-hotels": 600, "official-prague-eu": 600, "blog-eurocheapo": 30,
    "ecotourism-au-greentravelguide": 10, "blog-vietnamcoracle": 10, "mag-mexicodesconocido": 10,
    "blog-chicagoonthecheap": 10, "blog-bootsnall": 10, "official-turismo-ba": 10,
    "official-discovereu": 10, "vakinn-is": 5,
}
# robots.txt blocks AI crawlers: title and link only, no LLM kinds.
SUSTAIN_BUDGET_AI_RESERVED = {
    "blog-indietraveller", "blog-gobackpacking", "blog-eurocheapo", "blog-tokyocheapo", "blog-japancheapo",
    "blog-vietnamcoracle", "mag-mexicodesconocido", "blog-frequentmiler", "ref-priceoftravel",
    "walking-freetoursbyfoot", "official-prague-eu", "official-parismusees",
}


def _sustain_budget_ids():
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    return _batch_ids(seeds, "gstc-certified-hotels", "tb-visitcornwall")


def _batch_ids(seeds, first, next_first=None):
    """Ids of one research batch: from its first seed up to the next batch's first seed."""
    ids = [s["id"] for s in seeds]
    i = ids.index(first)
    j = ids.index(next_first) if next_first in ids else len(ids)
    return ids[i:j]


def test_sustainable_and_budget_sources_2026_11_18(generated):
    ids = _sustain_budget_ids()
    assert len(ids) == 116 and set(SUSTAIN_BUDGET_OFF) <= set(ids)
    r2, r3 = set(_ramp("ramp-2.txt")), set(_ramp("ramp-3.txt"))
    refused = set(ids) & FULL_CRAWL_REFUSED_2026_11_19
    on = [sid for sid in ids if sid not in SUSTAIN_BUDGET_OFF and sid not in refused]
    # adeac-bandera-azul on since 2026-11-18 (personal-use terms, owner accepted); three bot
    # walls off at ramp end 2026-11-18; refusals of the first full crawl off 2026-11-19
    assert len(on) == 90 - len(refused)
    for sid in on:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is True, sid
        assert spec["method"] in ("html", "rss", "sitemap") and not spec["needs_key"], sid
        assert sid in r2 and sid in r3, sid
        assert spec["daily_budget"] <= 200 and spec["max_depth"] <= 2, sid
        assert spec["min_delay_s"] >= SUSTAIN_BUDGET_DELAYS.get(sid, 5), sid
        assert spec["cadence"] in ("h12", "daily", "weekly", "monthly", "quarterly"), sid
        assert "2026-11-18" in notes, sid
        # Terms not found -> facts only, by the owner's decision of 2026-11-18.
        if spec["licence"]["class"] != "open":
            assert spec["licence"]["class"] == "facts_only", sid
            assert "owner accepted for non-commercial use 2026-11-18" in notes or \
                "facts only" in notes or "reproduction" in notes, sid
        if spec["category"] in ("blogs", "news"):
            assert spec["method"] in ("rss", "sitemap") and spec["source_class"] == "editorial", sid
        if sid in SUSTAIN_BUDGET_AI_RESERVED:
            assert spec["llm_kinds"] == [], sid
    for sid, why in SUSTAIN_BUDGET_OFF.items():
        spec = generated[sid]
        reason = " ".join(spec["notes"].split()).split("Disabled: ", 1)[1]
        assert spec["enabled"] is False and reason.startswith(why), (sid, reason)
        assert sid not in r2 and sid not in r3, sid
        if spec["needs_key"]:
            assert spec["key_secret"] == f"crawler-src-{sid}-key" and "https://" in reason, sid
    # Bot walls met at onboarding are denylisted.
    for sid, host in (("ecotourism-kenya-ecorated", "ecotourismkenya.org"), ("lnt-org", "lnt.org")):
        assert any(e["host"] == host for e in DENYLIST), host
        assert host in " ".join(generated[sid]["notes"].split()), sid
    assert "personal" in generated["adeac-bandera-azul"]["notes"]
    # The FlixBus GTFS stays on the gtfs_static adapter, off until its download avoids the
    # denylisted origin.
    fx = generated["flixbus-gtfs"]
    assert fx["adapter"] == "gtfs_static" and fx["licence"]["spdx"] == "ODbL-1.0"
    assert "gtfs.gis.flix.tech" in fx["notes"] and "flixbus-gtfs" not in s2s.GTFS_ON


# North America corpus (research vetted 2026-11-18): 98 rows, these are off with their reason.
NA_OFF = {
    "od-la-events": "dataset retired", "blog-narcity": "relevance:", "blog-outsideonline": "relevance:",
    "tr-bart-feeds": "needs an API key", "od-msc-citypage": "adapter pending",
    "od-canada-ckan": "adapter pending", "od-ottawa": "adapter pending",
}
# Personal/non-commercial-only terms: on by the owner's standing decision of 2026-11-18.
NA_PERSONAL_USE = {"tb-alberta", "tb-novascotia", "city-montreal", "city-vancouver", "city-halifax",
                   "city-las-vegas", "tr-wmata", "tr-cta", "tr-ttc"}
NA_AI_RESERVED = {"city-charleston", "city-winnipeg", "park-ny-state", "blog-outsideonline"}
NA_DELAYS = {"tb-quebec": 10, "city-halifax": 10, "od-canada-ckan": 20, "od-ottawa": 60}
# Hosts whose terms ban robots/scraping/AI use (denylisted). Hosts that refused research
# requests from an EU IP (403/challenge) are not added and not denylisted (may be regional).
NA_TERMS_BANS = ["visitflorida.com", "enjoyillinois.com", "visitseattle.org", "denver.org", "visitdenver.com",
                 "visitdallas.com", "baltimore.org", "visitnc.com", "visitorlando.com", "ottawatourism.ca",
                 "gotobermuda.com", "soundtransit.org", "www.mbta.com", "parks.ca.gov", "calgary.ca",
                 "roadtrippers.com", "eater.com"]
NA_REGIONAL_REFUSALS = ["https://www.travelalaska.com/", "https://www.colorado.com/", "https://www.gohawaii.com/",
                        "https://visitnj.org/", "https://www.amtrak.com/", "https://traveloregon.com/",
                        "https://www.whistler.com/", "https://www.yvr.ca/"]


def _na_ids():
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    i = next(n for n, s in enumerate(seeds) if s["id"] == "tb-alabama")
    ids = []
    for s in seeds[i:]:
        ids.append(s["id"])
        if s["id"] == "tr-ttc":
            break
    return ids


def test_north_america_sources_2026_11_18(generated):
    ids = _na_ids()
    assert len(ids) == 98 and set(NA_OFF) <= set(ids) and NA_PERSONAL_USE <= set(ids)
    r2, r3 = set(_ramp("ramp-2.txt")), set(_ramp("ramp-3.txt"))
    refused = set(ids) & FULL_CRAWL_REFUSED_2026_11_19
    on = [sid for sid in ids if sid not in NA_OFF and sid not in refused]
    assert len(on) == 91 - len(refused)
    for sid in on:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is True and not spec["needs_key"], sid
        assert spec["daily_budget"] <= 200 and spec["max_depth"] <= 2, sid
        assert spec["min_delay_s"] >= NA_DELAYS.get(sid, 5), sid
        assert spec["cadence"] in ("daily", "weekly", "monthly"), sid
        assert "2026-11-18" in notes, sid
        assert spec["licence"]["class"] in ("open", "facts_only"), sid
        if "terms not found" in notes.lower():
            assert "owner accepted for non-commercial use 2026-11-18" in notes, sid
        if spec["method"] == "html":
            assert spec["include"], f"{sid}: an html source pins its traveller sections"
        if sid == "od-chicago-events":
            assert spec["adapter"] == "json_events" and sid in r3 and sid not in r2
            assert spec["seeds"] == ["https://data.cityofchicago.org/resource/vsdy-d8k7.json"]
            assert spec["llm_kinds"] == [] and spec["kinds"] == ["event"]
        else:
            assert spec["method"] in ("html", "rss", "sitemap"), sid
            assert sid in r2 and sid in r3, sid
        if sid in NA_AI_RESERVED:
            assert spec["llm_kinds"] == [], sid
        # A source reads only its own hosts: every page include is pinned to a seed/sitemap/feed host.
        hosts = {u.split("/")[2] for u in spec["seeds"] + spec["sitemaps"] + spec["feeds"]}
        for r in spec["include"]:
            assert any(r.startswith((f"^https://{h.replace('.', chr(92) + '.')}/",
                                     f"^https?://{h.replace('.', chr(92) + '.')}/")) for h in hosts), (sid, r)
    for sid in NA_PERSONAL_USE:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is True and spec["licence"]["class"] == "facts_only", sid
        assert "terms restrict to personal/non-commercial use; owner accepted for non-commercial use 2026-11-18" in notes, sid
    for sid, why in NA_OFF.items():
        spec = generated[sid]
        reason = " ".join(spec["notes"].split()).split("Disabled: ", 1)[1]
        assert spec["enabled"] is False and reason.startswith(why), (sid, reason)
        assert sid not in r2 and sid not in r3, sid
    # Cross-host sitemaps were checked once with the VoyagerBot UA; the hosts that hold pages are listed.
    assert generated["tb-indiana"]["sitemaps"] == ["https://visitindiana.in.gov/sitemap.xml"]
    assert generated["tb-bc"]["sitemaps"] == ["https://www.supernaturalbc.com/sitemap.xml"]
    vic = generated["city-victoria"]
    assert vic["sitemaps"] == ["https://admin.tourismvictoria.com/sitemap.xml"]
    assert vic["seeds"] == ["https://www.tourismvictoria.com/victoria-events"]
    assert "travelnunavut.ca" in generated["tb-nunavut"]["sitemaps"][0]
    for host in NA_TERMS_BANS:
        assert any(e["host"] == host for e in DENYLIST), host
    for url in NA_REGIONAL_REFUSALS:
        assert s2s.denied(url, DENYLIST) is None, url


def test_unesco_whc_is_off_because_unesco_terms_ban_automated_access(generated):
    spec = generated["unesco-whc"]
    notes = " ".join(spec["notes"].split())
    assert spec["enabled"] is False
    assert "access, scrape, crawl, harvest or bulk-download Materials by any Automated System" in notes
    for url in spec["seeds"]:
        assert s2s.denied(url, DENYLIST) is not None, url
    for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
        assert "unesco-whc" not in _ramp(ramp)


@pytest.mark.parametrize("url", BATCH2_DENIED)
def test_batch2_denylist_entries(url):
    assert s2s.denied(url, DENYLIST) is not None, url


@pytest.mark.parametrize("url", APPROVED)
def test_approved_hosts_stay_allowed(url):
    assert s2s.denied(url, DENYLIST) is None, url


def test_every_enabled_source_is_allowed_by_the_denylist(generated):
    for sid, spec in generated.items():
        if spec["enabled"]:
            for u in spec["seeds"] + spec["sitemaps"] + spec["feeds"]:
                assert s2s.denied(u, DENYLIST) is None, (sid, u)


def test_production_bot_walls_2026_11_17_are_off_and_out_of_every_ramp(generated):
    for sid, host in (("eu-customs-travellers", "taxation-customs.ec.europa.eu"),
                      ("mfat-embassies", "mfat.govt.nz"), ("secretldn", "secretldn.com")):
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is False, sid
        assert "2026-11-17" in notes and "bot wall" in notes and host in notes, sid
        for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
            assert sid not in _ramp(ramp), (sid, ramp)
        assert any(e["host"] == host for e in DENYLIST), host


def test_production_bot_walls_2026_11_18_are_off_and_out_of_every_ramp(generated):
    for sid, host in (("philippines-travel", "philippines.travel"),
                      ("dubai-customs-passengers", "dubaicustoms.gov.ae")):
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is False, sid
        assert "2026-11-18" in notes and "bot wall" in notes and host in notes, sid
        for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
            assert sid not in _ramp(ramp), (sid, ramp)
        assert any(e["host"] == host for e in DENYLIST), host


def test_ramp_end_2026_11_18_refusals_are_off_and_out_of_every_ramp(generated):
    for sid, host in (("blog-chicagoonthecheap", "chicagoonthecheap.com"),
                      ("walking-freetoursbyfoot", "freetoursbyfoot.com"),
                      ("official-nycparks-events", "www.nycgovparks.org")):
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is False, sid
        assert "2026-11-18" in notes and "bot wall" in notes and host in notes, sid
        for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
            assert sid not in _ramp(ramp), (sid, ramp)
        assert any(host == e["host"] or host.endswith("." + e["host"]) for e in DENYLIST), host
    # too many 5xx at 5 s: off, slower, not denylisted
    rw = generated["visitrwanda"]
    assert rw["enabled"] is False and rw["min_delay_s"] == 20
    assert "5xx at 5 s" in " ".join(rw["notes"].split())
    assert not any(e["host"] == "visitrwanda.com" for e in DENYLIST)
    for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
        assert "visitrwanda" not in _ramp(ramp), ramp


# Alternatives to sites that refused or challenged VoyagerBot (vetted 2026-11-18).
ALTERNATIVES_ON = {"safetravel-nz", "eu-youreurope-travel", "revenue-ie-travellers"}
# Personal/non-commercial terms: on by the owner's standing rule of 2026-11-18 (facts only).
ALTERNATIVES_OWNER_ACCEPTED = {"dirco-sa", "capetown-gov"}


# Terms not found; the owner accepted them for non-commercial use on 2026-11-18.
ACCEPTED_2026_11_18 = ("icp-uae-customs", "egymonuments", "kws-parks")


def test_terms_unconfirmed_sources_accepted_2026_11_18(generated):
    for sid in ACCEPTED_2026_11_18:
        spec = generated[sid]
        assert spec["enabled"] is True and spec["licence"]["class"] == "facts_only", sid
        notes = " ".join(spec["notes"].split())
        assert "owner accepted for non-commercial use 2026-11-18" in notes, sid
        assert "Disabled:" not in notes, sid
        assert sid in _ramp("ramp-2.txt") and sid in _ramp("ramp-3.txt"), sid


def test_alternatives_2026_11_18(generated):
    for sid in ALTERNATIVES_ON:
        spec = generated[sid]
        assert spec["enabled"] is True and spec["source_class"] == "official", sid
        assert spec["licence"]["class"] == "open" and spec["licence"]["spdx"] == "CC-BY-4.0", sid
        assert spec["include"] and spec["daily_budget"] <= 300, sid
        assert "2026-11-18" in " ".join(spec["notes"].split()), sid
        assert sid in _ramp("ramp-2.txt") and sid in _ramp("ramp-3.txt"), sid
    assert generated["eu-youreurope-travel"]["min_delay_s"] >= 10  # europa.eu Crawl-delay 10
    for sid in ALTERNATIVES_OWNER_ACCEPTED | {"adeac-bandera-azul"}:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["enabled"] is True and spec["licence"]["class"] == "facts_only", sid
        assert "Terms restrict to personal/non-commercial use; owner accepted for non-commercial use 2026-11-18" in notes, sid
        assert sid in _ramp("ramp-2.txt") and sid in _ramp("ramp-3.txt"), sid


def test_terms_and_bot_wall_sources_are_off(generated):
    for sid in ("timeout-blog-rss", "timeout-news-sitemap"):
        notes = " ".join(generated[sid]["notes"].split())
        assert generated[sid]["enabled"] is False and "retrieval-augmented generation" in notes, sid
    usemb = generated["usembassy-list"]
    assert usemb["enabled"] is False and "answers 403" in " ".join(usemb["notes"].split())
    rs = generated["ricksteves-forum"]
    assert rs["enabled"] is False and "any robot, spider, crawler" in " ".join(rs["notes"].split())
    pi = generated["passport-index-dataset"]
    assert pi["enabled"] is False and pi["licence"]["class"] == "restricted"
    assert "Arton Capital" in " ".join(pi["notes"].split())


def test_batch2_off_sources_say_why(generated):
    prefixes = ("adapter pending", "needs an API key", "needs an account", "terms not yet confirmed",
                "licence:", "robots.txt", "access route", "owner decision", "dataset retired",
                "bot wall", "seed unresolved", "relevance:", "re-test from the crawler VM",
                "terms forbid storage/redistribution", "meta robots", "terms forbid automated access",
                "5xx at", "duplicate:", "gone:") + FULL_CRAWL_REFUSED_PREFIXES
    for sid in s2s.SOURCE_CLASS:
        spec = generated[sid]
        if spec["enabled"]:
            continue
        reason = " ".join(spec["notes"].split()).split("Disabled: ", 1)[1]
        assert reason.startswith(prefixes), (sid, reason)


# Batch-2 api/dump sources whose official/Wikipedia adapters shipped on 2026-11-18 (not in a
# ramp file yet).
ADAPTED_2026_11_18 = {
    "govuk-world-orgs": "govuk_world_orgs",
    "canada-offices-json": "canada_offices",
    "govuk-medical-lists": "govuk_medical_lists",
    "enwiki-visa-requirements": "enwiki_visa",
}

# US country information and the official food-hygiene registers: adapters shipped on
# 2026-11-18, listed in ramp-3.
COUNTRY_INFO_AND_FOOD = {
    "state-dept-country-info": "state_dept_country_info",
    "fsa-food-hygiene": "fsa_fhrs",
    "nyc-restaurant-inspections": "nyc_restaurant_inspections",
    "toronto-dinesafe": "toronto_dinesafe",
}


def test_country_info_and_food_registers_2026_11_18(generated):
    for sid, adapter in COUNTRY_INFO_AND_FOOD.items():
        spec = generated[sid]
        assert spec["enabled"] is True and spec["adapter"] == adapter, sid
        assert spec["source_class"] == "official" and spec["licence"]["class"] == "open", sid
        assert spec["llm_kinds"] == [] and spec["daily_budget"] <= 300, sid
        assert sid in _ramp("ramp-3.txt") and sid not in _ramp("ramp-2.txt"), sid
    sd = generated["state-dept-country-info"]
    assert sd["kinds"] == ["visa_rule", "embassy", "tip"] and sd["cadence"] == "daily"
    assert sd["seeds"] == ["https://cadataapi.state.gov/api/CountryTravelInformation"]
    for sid in ("fsa-food-hygiene", "nyc-restaurant-inspections", "toronto-dinesafe"):
        spec = generated[sid]
        assert spec["kinds"] == ["restaurant"] and spec["cadence"] == "weekly", sid
    assert generated["nyc-restaurant-inspections"]["daily_budget"] <= 200
    fsa = generated["fsa-food-hygiene"]["licence"]
    assert fsa["spdx"] == "OGL-UK-3.0"
    assert fsa["attribution"] == ("Contains public sector information licensed under the Open "
                                  "Government Licence v3.0 — Food Standards Agency")
    tor = generated["toronto-dinesafe"]["licence"]
    assert "Open Government Licence – Toronto" in tor["attribution"]
    assert tor["url"] == "https://open.toronto.ca/open-data-license/"
    assert "NYC Open Data" in generated["nyc-restaurant-inspections"]["licence"]["attribution"]


# Batch-2 api/dump sources that run on the open-data adapters (2026-11-18).
OPEN_DATA_ADAPTERS = {
    "openholidays": "openholidays",
    "nyc-parks-events": "json_events",
    "stockholm-open-api": "json_events",
    "eurostat-prices": "eurostat_jsonstat",
    "worldbank-wdi-bulk": "wdi_csv",
    "od-chicago-events": "json_events",  # North America, 2026-11-18
}


def test_batch2_api_and_dump_sources_run_only_on_an_adapter(generated):
    # An api/dump source is on only with a built-in adapter; every other one stays off.
    for sid in s2s.SOURCE_CLASS:
        spec = generated[sid]
        if sid in s2s.GTFS_FEEDS:
            continue
        if spec["method"] in ("api", "dump"):
            want = (OPEN_DATA_ADAPTERS.get(sid) or ADAPTED_2026_11_18.get(sid)
                    or COUNTRY_INFO_AND_FOOD.get(sid) or WIKIVOYAGE_EDITIONS.get(sid)
                    or OPEN_DATA_READERS.get(sid, (None,))[0] or CARBON_ADAPTERS.get(sid)
                    or ("factor_table" if sid in FACTOR_TABLE_SOURCES else None)
                    or CARBON_KEYED.get(sid, (None,))[0])
            assert spec["adapter"] == want, sid
            assert spec["enabled"] is (want is not None), sid
        if spec["needs_key"] and sid in CARBON_KEYED:
            assert spec["enabled"] and spec["key_secret"] == CARBON_KEYED[sid][1], sid
        elif spec["needs_key"]:
            assert spec["enabled"] is False and spec["key_secret"] == f"crawler-src-{sid}-key", sid


def test_batch2_enabled_sources_are_polite_and_scoped(generated):
    for sid in s2s.SOURCE_CLASS:
        spec = generated[sid]
        if not spec["enabled"] or sid in s2s.GTFS_FEEDS:
            continue
        assert spec["method"] in ("html", "rss", "sitemap") or sid in OPEN_DATA_ADAPTERS \
            or sid in ADAPTED_2026_11_18 or sid in COUNTRY_INFO_AND_FOOD or sid in OPEN_DATA_READERS \
            or sid in WIKIVOYAGE_EDITIONS or sid in CARBON_ADAPTERS or sid in FACTOR_TABLE_SOURCES \
            or sid in CARBON_KEYED, sid
        assert spec["daily_budget"] <= 500 and spec["max_depth"] <= 2 and spec["min_delay_s"] >= 5, sid
        assert spec["licence"]["class"] in ("open", "facts_only") or (
            (sid in ("openholidays", "enwiki-visa-requirements") + tuple(CORPUS_SHARE_ALIKE)
             or sid in WIKIVOYAGE_EDITIONS)
            and spec["licence"]["class"] == "share_alike"), sid
        assert spec["cadence"] in ("daily", "h12", "weekly", "monthly", "quarterly", "yearly"), sid
        # Every include regex is pinned to the source's own host.
        hosts = {u.split("/")[2] for u in spec["seeds"] + spec["sitemaps"] + spec["feeds"]}
        for r in spec["include"]:
            assert any(r.startswith(f"^https://{h.replace('.', chr(92) + '.')}/") or
                       r.startswith(f"^https?://{h.replace('.', chr(92) + '.')}/") for h in hosts), (sid, r)


def test_owner_accepted_terms_2026_11_17(generated):
    accepted = [sid for sid in s2s.SOURCE_CLASS
                if "owner accepted for non-commercial use 2026-11-17" in " ".join((generated[sid]["notes"] or "").split())]
    assert len(accepted) >= 40
    for sid in accepted:
        spec = generated[sid]
        # A bot wall met later (secretldn, 2026-11-17) turns an accepted source off.
        # (and too many 5xx, visitrwanda 2026-11-18) turn an accepted source off.
        n = " ".join(spec["notes"].split())
        bot_wall = "Disabled: bot wall" in n or "Disabled: 5xx at" in n
        assert spec["enabled"] is (not bot_wall), sid
        assert spec["licence"]["class"] == "facts_only", sid
    sg = generated["sg-customs-travellers"]
    assert sg["enabled"] is True and "Owner decision 2026-11-17" in " ".join(sg["notes"].split())
    # Not covered by the acceptance: a possible ban, a 403 page, the relevance gate.
    for sid in ("choosechicago", "colombia-travel", "tasteatlas", "parisjetaime", "au-homeaffairs-visas",
                "messynessychic", "nps-data-api", "myswitzerland-open-data", "amusingplanet"):
        assert generated[sid]["enabled"] is False, sid


def test_ai_reserved_sources_have_no_llm_kinds(generated):
    for sid in ("visitphilly", "healthsites-io"):
        assert generated[sid]["llm_kinds"] == [], sid


def _ramp(name):
    text = (ROOT / "sources" / name).read_text()
    return [ln.split("#")[0].strip() for ln in text.splitlines() if ln.split("#")[0].strip()]


def test_ramp_2_extends_ramp_1_with_enabled_runnable_sources():
    committed = {p.stem: yaml.safe_load(p.read_text()) for p in (ROOT / "sources").glob("*.yaml")}
    r1, r2 = _ramp("ramp-1.txt"), _ramp("ramp-2.txt")
    assert len(r2) == len(set(r2))
    assert r2[: len(r1)] == r1
    v = _validator()
    for sid in r2:
        assert sid in committed, sid
        spec = committed[sid]
        assert spec["enabled"] is True, sid
        assert not list(v.iter_errors(spec)), sid
    for sid in r2[len(r1):]:
        assert committed[sid]["method"] in ("html", "rss", "sitemap"), sid
    # Every html/rss/sitemap source this batch enables is in ramp-2 (the open-data adapter
    # sources come with ramp-3).
    on = ({sid for sid in s2s.SOURCE_CLASS if committed[sid]["enabled"]} - set(OPEN_DATA_ADAPTERS)
          - set(s2s.GTFS_FEEDS) - set(ADAPTED_2026_11_18) - set(COUNTRY_INFO_AND_FOOD)
          - set(OPEN_DATA_READERS) - set(WIKIVOYAGE_EDITIONS) - set(CARBON_ADAPTERS)
          - set(FACTOR_TABLE_SOURCES) - set(CARBON_KEYED))
    assert on <= set(r2), sorted(on - set(r2))


def test_scam_safety_sources(generated):
    for sid in ("travelerset-scams", "avoidtravelscam", "twopassportspacked-scams"):
        spec = generated[sid]
        assert spec["enabled"] and spec["method"] == "html" and spec["category"] == "tips", sid
        assert spec["licence"]["class"] == "facts_only" and spec["source_class"] == "editorial", sid
        assert spec["cadence"] == "monthly" and spec["daily_budget"] <= 50, sid
        assert spec["max_depth"] <= 1 and spec["relevance"]["sections"] == ["tips"], sid
        if spec["max_depth"] > 0:
            assert spec["include"], f"{sid}: a depth-1 source must pin its article section"
    assert generated["twopassportspacked-scams"]["max_depth"] == 0
    sr = generated["smartraveller"]
    assert any("scammers" in u for u in sr["seeds"]) and not sr["include"]
    assert "two networks 2026-11-17" in sr["notes"]


def test_ramp_3_is_ramp_2_plus_the_open_data_sources():
    committed = {p.stem: yaml.safe_load(p.read_text()) for p in (ROOT / "sources").glob("*.yaml")}
    r2, r3 = _ramp("ramp-2.txt"), _ramp("ramp-3.txt")
    assert len(r3) == len(set(r3))
    assert r3[: len(r2)] == r2
    assert set(r3[len(r2):]) == (set(OPEN_DATA_ADAPTERS) | s2s.GTFS_ON | set(COUNTRY_INFO_AND_FOOD)
                                 | set(OPEN_DATA_READERS) | set(WIKIVOYAGE_EDITIONS) | set(CARBON_ADAPTERS)
                                 | set(FACTOR_TABLE_SOURCES) | set(CARBON_KEYED))
    v = _validator()
    for sid in r3:
        spec = committed[sid]
        assert spec["enabled"] is True, sid
        assert not list(v.iter_errors(spec)), sid
    for sid, adapter in OPEN_DATA_ADAPTERS.items():
        assert committed[sid]["adapter"] == adapter, sid


def test_open_data_sources_2026_11_18(generated):
    oh = generated["openholidays"]
    assert oh["seeds"] == ["https://openholidaysapi.org/Countries"]
    assert oh["kinds"] == ["public_holiday"] and oh["cadence"] == "monthly"
    assert oh["licence"]["class"] == "share_alike" and oh["licence"]["spdx"] == "ODbL-1.0"
    for sid in ("eurostat-prices", "worldbank-wdi-bulk"):
        spec = generated[sid]
        assert spec["kinds"] == ["price_level"] and spec["cadence"] == "yearly", sid
        assert spec["source_class"] == "official" and spec["licence"]["spdx"] == "CC-BY-4.0", sid
    assert "na_item=PLI_EU27_2020" in generated["eurostat-prices"]["seeds"][0]
    assert generated["worldbank-wdi-bulk"]["method"] == "dump"
    sth = generated["stockholm-open-api"]
    assert sth["seeds"] == ["https://www.visitstockholm.com/api/public-v1/events/?format=json"]
    assert generated["nyc-parks-events"]["seeds"] == ["https://data.cityofnewyork.us/resource/fudw-fgrp.json"]
    # Still off, each with its reason.
    for sid, why in (("bcn-opendata-agenda", "disallows /data/api*"),
                     ("madrid-agenda", "disallows /api/"),
                     ("toronto-events", "Retired"),
                     ("taiwan-tourism-od", "answers 500"),
                     ("datagovhk", "www.lcsd.gov.hk")):
        spec = generated[sid]
        assert spec["enabled"] is False and why in " ".join(spec["notes"].split()), sid
    assert {"public_holiday", "price_level"} <= s2s.KINDS


def test_gtfs_feeds_run_on_the_gtfs_adapter(generated):
    assert s2s.GTFS_ON <= set(s2s.GTFS_FEEDS)
    for sid in s2s.GTFS_FEEDS:
        spec = generated[sid]
        assert spec["adapter"] == "gtfs_static", sid
        assert spec["method"] in ("api", "dump"), sid
        assert spec["kinds"] == ["transit_item"] and spec["llm_kinds"] == [], sid
        assert spec["enabled"] is (sid in s2s.GTFS_ON), sid
        assert spec["cadence"] == "weekly" and spec["min_delay_s"] >= 5, sid
        assert spec["daily_budget"] <= 500, sid
        assert spec["licence"]["attribution"], sid
        if spec["needs_key"]:
            assert spec["enabled"] is False and spec["key_secret"] == f"crawler-src-{sid}-key", sid
        if spec["enabled"]:
            notes = " ".join(spec["notes"].split())
            assert spec["licence"]["class"] in ("open", "api_terms") or (
                spec["licence"]["class"] == "facts_only" and "Terms not found" in notes
                and "owner decision 2026-11-17" in notes), sid
            assert not spec["needs_key"], sid
            assert all(u.startswith("https://") for u in spec["seeds"]), sid
    # robots.txt Crawl-Delay 10 on data.opentransportdata.swiss.
    assert generated["opentransportdata-swiss"]["min_delay_s"] >= 10
    for sid in ("gtfs-de", "hk-td-gtfs"):
        assert "403" in generated[sid]["notes"], sid
    assert "terms not yet confirmed" in generated["mta-gtfs"]["notes"]


def test_adapted_sources_2026_11_18(generated):
    # Official embassy and medical lists, and Wikipedia visa tables, through their own adapters
    # and adapter-only kinds; Wikipedia stays reference/share-alike and never yields visa_rule.
    kinds = {
        "govuk-world-orgs": ["embassy"],
        "canada-offices-json": ["embassy"],
        "govuk-medical-lists": ["medical_facility", "article"],
        "enwiki-visa-requirements": ["visa_reference"],
    }
    for sid, k in kinds.items():
        spec = generated[sid]
        assert spec["kinds"] == k and spec["llm_kinds"] == [], sid
        assert spec["source_class"] == ("reference" if sid.startswith("enwiki") else "official"), sid
    wiki = generated["enwiki-visa-requirements"]
    assert wiki["licence"]["class"] == "share_alike"
    assert wiki["seeds"] == [
        "https://dumps.wikimedia.org/enwiki/latest/enwiki-latest-pages-articles-multistream-index.txt.bz2"
    ]


# --- Corpus expansion: Europe (research vetted 2026-11-18, corpus_europe.md) -------------
EUROPE_OFF = {
    **{sid: "adapter pending" for sid in (
        "od-data-gov-uk", "od-culture-data-gouv", "od-datos-gob-es", "od-opendata-swiss", "od-data-gv-at",
        "od-data-overheid-nl", "od-data-public-lu", "od-avoindata-fi", "od-dataportal-se", "od-podatki-gov-si",
        "od-andmed-eesti", "od-data-europa", "od-data-gov-cz", "od-dane-gov-pl", "tr-irail", "tr-entur",
        "tb-puglia")},
    "api-europeana": "needs an API key", "od-data-gov-gr": "needs an API key",
    "thelocal-news": "relevance",
    "ref-odysseus-culture-gr": "re-test from the crawler VM", "tr-intercity-pl": "re-test from the crawler VM",
}
EUROPE_DELAYS = {"city-toulouse": 20, "city-geneve": 10, "tb-gibraltar": 10, "blog-europeupclose": 30,
                 "city-esmadrid": 10}
EUROPE_AI_RESERVED = {
    "city-dresden", "city-esmadrid", "city-ljubljana", "city-nice", "ed-inyourpocket", "np-cairngorms",
    "tb-fjordnorway", "tb-galicia", "tb-moldova", "tb-sanmarino", "tb-sardegna", "tb-serbia", "tb-valdeloire",
    "tb-visithungary", "tb-visitluxembourg", "tr-vy",
}
# Personal/non-commercial terms, on by the owner's standing rule of 2026-11-18.
EUROPE_PERSONAL_TERMS = {
    "tr-eurostar",  # enabled 2026-11-18, owner rule
    "city-visitbath", "city-visitbelfast", "city-visitmanchester", "city-visitcambridge", "tb-visitisleofman",
    "tb-historicenvironment-scot", "tb-discovernorthernireland", "tb-bayern", "tr-cfr-calatori", "city-esmadrid",
    "city-visitvalencia", "tb-sardegna", "tb-visittrentino", "city-visitbruges", "city-visitbergen",
    "city-thisisathens", "tb-montenegro", "tr-heathrow", "tr-munich-airport",
}
EUROPE_PORTAL_REASON = "adapter pending: choose tourism datasets + dataset reader"


def _corpus_batch_check(generated, ids, off, delays, ai_reserved, personal):
    r2, r3 = set(_ramp("ramp-2.txt")), set(_ramp("ramp-3.txt"))
    assert set(off) <= set(ids) and set(personal) <= set(ids)
    for sid in ids:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert "2026-11-18" in notes, sid
        if sid in FULL_CRAWL_REFUSED_2026_11_19:
            continue  # test_full_crawl_refusals_2026_11_19
        if sid in off:
            reason = notes.split("Disabled: ", 1)[1]
            assert spec["enabled"] is False and reason.startswith(off[sid]), (sid, reason)
            assert sid not in r2 and sid not in r3, sid
            if spec["needs_key"]:
                assert spec["key_secret"] == f"crawler-src-{sid}-key" and "https://" in reason, sid
            continue
        assert spec["enabled"] is True, sid
        assert spec["method"] in ("html", "rss", "sitemap") and not spec["needs_key"], sid
        assert sid in r2 and sid in r3, sid
        assert spec["daily_budget"] <= 200 and spec["max_depth"] <= 2, sid
        assert spec["min_delay_s"] >= delays.get(sid, 5), sid
        assert spec["cadence"] in ("daily", "weekly", "monthly", "quarterly", "yearly"), sid
        if spec["licence"]["class"] != "open":
            assert spec["licence"]["class"] == "facts_only" or sid in CORPUS_SHARE_ALIKE, sid
        if spec["licence"]["class"] == "facts_only" and ("terms not found" in notes or "Terms unconfirmed" in notes):
            assert "owner accepted for non-commercial use 2026-11-18" in notes, sid
        if sid in personal:
            assert spec["licence"]["class"] == "facts_only", sid
            assert ("Terms restrict to personal/non-commercial use; owner accepted for non-commercial use "
                    "2026-11-18") in notes, sid
        if sid in ai_reserved:
            assert spec["llm_kinds"] == [] and "no LLM kinds" in notes, sid
        if spec["category"] in ("blogs", "news"):
            assert spec["source_class"] == "editorial", sid


def test_corpus_europe_2026_11_18(generated):
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    ids = _batch_ids(seeds, "tb-visitcornwall", "tb-alabama")
    assert len(ids) == 151
    _corpus_batch_check(generated, ids, EUROPE_OFF, EUROPE_DELAYS, EUROPE_AI_RESERVED, EUROPE_PERSONAL_TERMS)
    assert len([s for s in ids if s not in EUROPE_OFF]) == 129
    # National open-data portals wait for a dataset reader, not an HTML crawl.
    for sid in ids:
        if sid.startswith("od-") and sid not in ("od-musei-italiani", "od-data-gov-gr", "od-data-gov-cz"):
            assert EUROPE_PORTAL_REASON in generated[sid]["notes"].replace("\n", " "), sid
    # Denylisted or already-present hosts are not added again.
    assert "tb-nationaltrust" not in generated and "od-data-gouv-fr" not in generated
    for url in EUROPE_DENIED:
        assert s2s.denied(url, DENYLIST) is not None, url
    for url in EUROPE_STAY_ALLOWED:
        assert s2s.denied(url, DENYLIST) is None, url


# Terms ban robots, scraping, automated access or TDM (corpus Europe 2026-11-18).
EUROPE_DENIED = [
    "https://www.lakedistrict.gov.uk/", "https://www.nationalrail.co.uk/", "https://visitandorra.com/",
    "https://www.visitestonia.com/", "https://www.suedtirol.info/", "https://www.mvg.de/",
    "https://www.publictransport.com.mt/", "https://www.bolognawelcome.com/",
]
EUROPE_STAY_ALLOWED = [
    "https://visittallinn.ee/sitemap.xml", "https://www.tirol.at/sitemap.xml",
    "https://www.translink.co.uk/sitemap.xml", "https://www.museiitaliani.it/", "https://www.oebb.at/en/",
    "https://www.cairngorms.co.uk/",
]


# --- Corpus expansion: thematic, worldwide (research vetted 2026-11-18, corpus_thematic.md) ---
THEMATIC_OFF = {
    **{sid: "adapter pending" for sid in (
        "od-openskimap", "od-openbeta", "od-uk-bathing-water", "od-eea-bathing-water", "od-epa-beacon",
        "od-fr-merimee", "od-nl-rijksmonumenten", "od-imls-museums", "od-mapping-museums-uk", "od-iana-tzdb",
        "api-usno-astro", "api-equaldex", "od-gb-toiletmap")},
    "od-nasa-black-marble": "needs an account",
    "ref-museumsassociation": "relevance",
    **{sid: "re-test from the crawler VM" for sid in (
        "park-gbrmpa", "gov-agriculture-au-pets", "health-fitfortravel", "air-united", "air-klm")},
}
THEMATIC_DELAYS = {"ref-ilga-world": 10, "gov-ee-police-dnv": 10, "health-ecdc": 10, "ref-mountainproject": 60}
THEMATIC_AI_RESERVED = {"air-istanbul", "blog-ciaobambino", "blog-journeywoman", "food-tucson-gastronomy",
                        "ref-aarp-travel", "trail-camino-santiago-gal"}
THEMATIC_PERSONAL_TERMS = {
    "wine-portugal", "gov-catsa", "gov-afcd-hk-pets", "air-narita", "ref-mountainproject",
    "park-wa-dbca", "food-macao-gastronomy", "mkt-borough-market", "blog-journeywoman", "air-changi", "air-delhi",
    "air-qantas-travel-info",
}


def test_corpus_thematic_2026_11_18(generated):
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    ids = _batch_ids(seeds, "trail-camino-santiago-gal", "tb-caribe-mexicano")
    assert len(ids) == 84
    _corpus_batch_check(generated, ids, THEMATIC_OFF, THEMATIC_DELAYS, THEMATIC_AI_RESERVED, THEMATIC_PERSONAL_TERMS)
    assert len([s for s in ids if s not in THEMATIC_OFF]) == 64
    # Duplicates of existing sources, the denylisted Smithsonian API and Incheon (robots Disallow: /) are not added.
    for sid in ("park-doc-nz", "park-nsw-npws", "ev-festivalfinder-eu", "air-incheon", "air-munich",
                "api-smithsonian-oa", "air-sfo"):
        assert sid not in generated, sid
    assert generated["air-frankfurt"]["exclude"] and "(?!" not in "".join(generated["air-frankfurt"]["include"])
    for url in THEMATIC_DENIED:
        assert s2s.denied(url, DENYLIST) is not None, url
    for url in THEMATIC_STAY_ALLOWED:
        assert s2s.denied(url, DENYLIST) is None, url


# Terms ban robots, scraping, automated access or TDM (corpus thematic 2026-11-18).
THEMATIC_DENIED = [
    "https://www.nationaltrail.co.uk/", "https://appalachiantrail.org/", "https://www.euansguide.com/",
    "https://www.pikeplacemarket.org/", "https://www.nparks.gov.sg/", "https://sunrise-sunset.org/api",
    "https://www.sydneyairport.com.au/", "https://www.viennaairport.com/", "https://www.southwest.com/",
]
THEMATIC_STAY_ALLOWED = [
    "https://www.roots.gov.sg/sitemap-pages.xml", "https://www.nea.gov.sg/sitemap.xml",
    "https://www.nationalparks.nsw.gov.au/things-to-do", "https://aa.usno.navy.mil/api/rstt/oneday",
    "https://www.flughafen-zuerich.ch/en/passengers", "https://cadw.gov.wales/sitemap.xml",
]


# Corpus sources whose licence is share_alike (never merged with other licences).
CORPUS_SHARE_ALIKE = {"customs-sag-cl", "guide-caravanistan",  # caravanistan: CC BY-SA 4.0
                      "od-google-tim-github"}  # Travel Impact Model docs: CC BY-SA 4.0 (2026-11-19)


# --- Corpus expansion: Latin America and the Caribbean (research vetted 2026-11-18) -------
LATAM_OFF = {
    # Its terms ban robots and spiders (hidden gems research 2026-11-18): off, denylisted.
    "tb-visittci": "terms forbid automated access",
    **{sid: "adapter pending" for sid in (
        "od-datos-gov-co", "od-datos-gob-cl", "od-catalogo-uy", "od-data-rio", "fx-bcra")},
    "fx-banxico": "needs an API key",
    **{sid: "relevance" for sid in ("blog-bogotapost", "blog-ba-herald", "blog-riotimes")},
    # robots.txt gave no response from the research workstation; terms unchecked.
    **{sid: "re-test from the crawler VM" for sid in (
        "city-turismo-cdmx", "ev-cartelera-cdmx", "tr-metro-cdmx", "od-datos-cdmx", "od-datatur-mx",
        "od-datos-bogota", "park-sinac-cr", "visa-migracion-pa", "city-visit-rio", "tb-santacatarina",
        "tb-parana", "wx-inmet", "tb-saintmartin", "tb-stkitts")},
}
LATAM_DELAYS = {"park-pn-ar": 10, "visa-migraciones-ar": 10, "tr-trenes-ar": 10, "tb-anguilla": 10,
                "visa-pica-jm": 10, "tb-bonaire": 10}
LATAM_AI_RESERVED = {"blog-wander-argentina", "tb-guadeloupe", "tb-mendoza", "tb-yucatan", "tr-metro-cl",
                     "tr-navimag", "tr-transmilenio"}
# OWNER-DECISION rows of the LatAm research, on by the owner's decision of 2026-11-18.
LATAM_PERSONAL_TERMS = {
    "official-sectur-mx", "visa-inm-mx", "park-conanp-mx", "ev-cervantino", "city-medellin-travel",
    "visa-migracion-colombia", "tr-transmilenio", "tr-perurail", "tb-visitjamaica", "tb-bonaire", "tb-visittci",
    "tb-visitloscabos",
}


def test_corpus_latam_2026_11_18(generated):
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    ids = _batch_ids(seeds, "tb-caribe-mexicano", "tb-visitghana")
    assert len(ids) == 108
    _corpus_batch_check(generated, ids, LATAM_OFF, LATAM_DELAYS, LATAM_AI_RESERVED, LATAM_PERSONAL_TERMS)
    assert len([s for s in ids if s not in LATAM_OFF]) == 84
    # PeruRail's robots.txt disallows /feed/: HTML pages only.
    pr = generated["tr-perurail"]
    assert pr["method"] == "html" and not pr["feeds"]
    # INAH stays on under the same rule (gob.mx personal-use terms).
    inah = generated["official-inah"]
    assert inah["enabled"] is True and "solamente para su uso personal" in " ".join(inah["notes"].split())
    sag = generated["customs-sag-cl"]
    assert sag["licence"]["class"] == "share_alike" and sag["enabled"] is True
    for sid in ("park-galapagos", "city-turismo-ba", "tb-turismo-br"):
        assert sid not in generated, sid
    for url in LATAM_DENIED:
        assert s2s.denied(url, DENYLIST) is not None, url
    for url in LATAM_STAY_ALLOWED:
        assert s2s.denied(url, DENYLIST) is None, url


LATAM_DENIED = ["https://www.aruba.com/", "https://www.chilango.com/", "https://serviciomigraciones.cl/"]
LATAM_STAY_ALLOWED = ["https://www.curacao.com/en/", "https://www.gob.mx/sectur",
                      "https://www.conaf.cl/sitemap_index.xml"]

# Generic, mapping-driven open-data readers (2026-11-18): on, official, adapter-only kinds,
# listed in ramp-3.
OPEN_DATA_READERS = {
    "od-chicago-food-inspections": ("socrata", ["restaurant"], "open"),
    "od-austin-food-inspections": ("socrata", ["restaurant"], "open"),
    "od-catalonia-tourism-register": ("socrata", ["hotel"], "facts_only"),
    "od-nyc-public-restrooms": ("socrata", ["amenity"], "open"),
    "od-fr-atout-france-classes": ("udata_resource", ["hotel"], "open"),
    "od-wien-ogd": ("wfs_geojson", ["attraction", "amenity"], "open"),
    "od-zuerich-ogd": ("wfs_geojson", ["amenity"], "open"),
}
OPEN_DATA_READERS_OFF = {
    "od-sf-restaurant-scores": "dataset retired",
    "od-lacounty-restaurant-inspections": "access route",
    "od-nz-doc": "access route",
    "od-hk-fehd-licensed-restaurants": "adapter pending",
    "sg-hawker-centres": "access route",
}


def test_open_data_readers_2026_11_18(generated):
    for sid, (adapter, kinds, licence) in OPEN_DATA_READERS.items():
        spec = generated[sid]
        assert spec["enabled"] is True and spec["adapter"] == adapter, sid
        assert spec["kinds"] == kinds and spec["llm_kinds"] == [], sid
        assert spec["source_class"] == "official", sid
        assert spec["licence"]["class"] == licence and spec["licence"]["attribution"], sid
        assert spec["daily_budget"] <= 200 and spec["min_delay_s"] >= 5, sid
        assert sid in _ramp("ramp-3.txt") and sid not in _ramp("ramp-2.txt"), sid
    # Atout France: the dataset API names the file; the file's host is a seed so the source
    # owns it. Never a /datasets/r/ redirect or a static.data.gouv.fr/resources file.
    atout = generated["od-fr-atout-france-classes"]["seeds"]
    assert atout[0] == "https://www.data.gouv.fr/api/1/datasets/hebergements-touristiques-classes-en-france/"
    assert atout[1].startswith("https://data.classement.atout-france.fr/static/exportHebergementsClasses/")
    assert not any("/datasets/r/" in u or "static.data.gouv.fr" in u for u in atout)
    assert len(generated["od-wien-ogd"]["seeds"]) == 4
    assert len(generated["od-zuerich-ogd"]["seeds"]) == 2
    for sid, why in OPEN_DATA_READERS_OFF.items():
        spec = generated[sid]
        reason = " ".join(spec["notes"].split()).split("Disabled: ", 1)[1]
        assert spec["enabled"] is False and reason.startswith(why), (sid, reason)
        for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
            assert sid not in _ramp(ramp), (sid, ramp)
    # No ArcGIS feature-service host is ever a seed (services*.arcgis.com answer 403 on robots).
    for spec in generated.values():
        assert not any(".arcgis.com/" in u and "services" in u.split("/")[2] for u in spec["seeds"])


# --- Corpus expansion: Africa and the Middle East (research vetted 2026-11-18) ------------
AFRICA_OFF = {
    **{sid: "adapter pending" for sid in (
        "tr-digitalmatatus", "od-data-gov-qa", "od-data-gov-bh", "od-izmir-acikveri", "od-data-gov-ma",
        "od-data-gov-tn", "od-data-gov-om", "od-data-abudhabi", "od-bayanat-ae")},
    **{sid: "relevance" for sid in ("blog-yabangee", "news-whatson-ae", "lepetitjournal-news", "news-capetownetc")},
    "tb-visitaqaba": "re-test from the crawler VM",
}
AFRICA_DELAYS = {"tb-discovertunisia": 10, "tr-rta-dubai": 10, "tr-bakubus": 60, "wx-meteo-sc": 10}
AFRICA_AI_RESERVED = {"mag-theguideistanbul", "tb-fujairah", "tb-visitizmir", "tr-ttc-tbilisi"}
AFRICA_PERSONAL_TERMS = {
    "gov-westerncape", "visa-etakenya", "wx-met-oman", "tb-wesgro", "np-nwr-namibia", "tb-visitrasalkhaimah",
    "tb-dct-abudhabi", "tr-admobility", "wx-ncm-sa", "ev-sharjahart", "customs-sars", "news-thenational-travel",
    "mag-travelafrica", "blog-istanbulclues", "ref-zambiatourism",
}


def test_corpus_africa_me_2026_11_18(generated):
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    ids = _batch_ids(seeds, "tb-visitghana", "tb-osaka-info")
    assert len(ids) == 138
    _corpus_batch_check(generated, ids, AFRICA_OFF, AFRICA_DELAYS, AFRICA_AI_RESERVED, AFRICA_PERSONAL_TERMS)
    assert len([s for s in ids if s not in AFRICA_OFF]) == 124
    # KwaZulu-Natal: the sitemap lives on visitkzn-sa.com, whose robots.txt was read first.
    kzn = generated["tb-zulu-kzn"]
    assert kzn["sitemaps"] == ["https://visitkzn-sa.com/sitemap.xml"]
    assert "robots.txt of visitkzn-sa.com" in " ".join(kzn["notes"].split())
    assert generated["ev-riyadhseason"]["render"] == "auto"
    # MyCiTi is the existing official-myciti; Botswana Tourism's host already has a source.
    assert "tb-botswanatourism" not in generated and generated["official-myciti"]["enabled"] is True
    for url in AFRICA_DENIED:
        assert s2s.denied(url, DENYLIST) is not None, url
    for url in AFRICA_STAY_ALLOWED:
        assert s2s.denied(url, DENYLIST) is None, url


AFRICA_DENIED = ["https://georgia.travel/", "https://www.isimangaliso.com/", "https://www.thesouthafrican.com/",
                 "https://www.casatramway.ma/", "https://mdlbeast.com/", "https://www.geo.fr/",
                 "https://www.routard.com/", "https://visitethiopia.travel/"]
AFRICA_STAY_ALLOWED = ["https://visitkzn-sa.com/sitemap.xml",
                       "https://www.oncf-voyages.ma/", "https://www.railway.ge/en/sitemap_index.xml"]


# --- Corpus expansion: Asia-Pacific (research vetted 2026-11-18, corpus_asia_pacific.md) ---
AP_OFF = {
    "api-cwa-taiwan": "needs an API key", "api-metlink-nz": "needs an API key",
    "api-tdx-taiwan": "needs an API key", "gov-kemenparekraf": "re-test from the crawler VM",
    "gov-mct-china": "re-test from the crawler VM", "gov-mot-cambodia": "adapter pending",
    "gov-tourism-mm": "re-test from the crawler VM", "np-knps-korea": "re-test from the crawler VM",
    "od-data-gov-mo": "adapter pending", "od-data-gov-sg": "adapter pending",
    "od-data-qld": "adapter pending", "tb-discoverkyrgyzstan": "re-test from the crawler VM",
    "tb-hanoi-tourism": "re-test from the crawler VM", "tb-himachal": "re-test from the crawler VM",
    "tb-jktourism": "re-test from the crawler VM", "tb-kyoto-travel": "re-test from the crawler VM",
    "tb-mongolia-travel": "adapter pending", "tb-puducherry": "re-test from the crawler VM",
    "tb-punjab-pk": "re-test from the crawler VM", "tb-rajasthan": "re-test from the crawler VM",
    "tb-sikkim": "re-test from the crawler VM", "tb-tamilnadu": "re-test from the crawler VM",
    "tb-tonga": "re-test from the crawler VM", "tb-visitjogja": "re-test from the crawler VM",
    "tb-visitkl": "re-test from the crawler VM", "tb-vnat": "adapter pending",
    "tb-wbtourism": "re-test from the crawler VM", "tr-delhi-metro": "adapter pending",
    "tr-indianrailways": "re-test from the crawler VM", "tr-krl-jakarta": "re-test from the crawler VM",
    "tr-myrapid": "re-test from the crawler VM", "tr-railway-lk": "re-test from the crawler VM",
    "tr-seoul-metro": "re-test from the crawler VM", "tr-srt-thailand": "re-test from the crawler VM",
    "tr-td-hk": "re-test from the crawler VM", "tr-transperth": "re-test from the crawler VM",
    "visa-evisa-kh": "re-test from the crawler VM", "visa-evisa-uz": "adapter pending",
    "visa-immigration-ph": "re-test from the crawler VM", "visa-laoevisa": "re-test from the crawler VM",
    "wx-jma": "adapter pending", "wx-metservice-nz": "adapter pending",
}
AP_DELAYS = {"blog-insidekyoto": 10, "gov-nepal-motca": 10, "np-dnpwc-nepal": 10, "tr-great-journeys-nz": 60}
AP_AI_RESERVED = {
    "blog-insidekyoto", "ev-broadsheet", "mag-hongkongcheapo", "tb-kazakhstan", "tb-sabah",
    "tb-south-australia", "tb-tahiti", "tb-tourism-malaysia-corp", "tr-mrta",
}
AP_PERSONAL_TERMS = {
    "ev-broadsheet", "ev-esplanade-sg", "ev-vivid-sydney", "np-wa-exploreparks", "ref-japan-guide",
    "tb-aucklandnz", "tb-macao-tourism", "tb-nelsontasman", "tb-rotoruanz", "tb-sltda", "tb-taiwan-tourism",
    "tb-tasmania", "tb-visitcanberra", "tb-visitnsw", "wx-nea-sg",
}


def test_corpus_asia_pacific_2026_11_18(generated):
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    ids = _batch_ids(seeds, "tb-osaka-info", "prague-pid-gtfs")
    assert len(ids) == 183
    _corpus_batch_check(generated, ids, AP_OFF, AP_DELAYS, AP_AI_RESERVED, AP_PERSONAL_TERMS)
    assert len([s for s in ids if s not in AP_OFF]) == 141
    # Wellington's sitemap lives on admin.wellingtonnz.com, whose robots.txt was read first.
    wl = generated["tb-wellington"]
    assert wl["sitemaps"] == ["https://admin.wellingtonnz.com/sitemap.xml"]
    assert "robots.txt of admin.wellingtonnz.com" in " ".join(wl["notes"].split())
    # robots.txt Disallow: / hosts, denylisted hosts and duplicates are not added.
    for sid in ("np-nparks-sg", "tr-narita-airport", "tr-changi", "np-doc-nz", "np-nsw-parks"):
        assert sid not in generated or generated[sid]["enabled"] is False, sid
    for url in AP_DENIED:
        assert s2s.denied(url, DENYLIST) is not None, url
    for url in AP_STAY_ALLOWED:
        assert s2s.denied(url, DENYLIST) is None, url


AP_DENIED = ["https://www.visit-fsm.org/", "https://www.visitkiribati.travel/", "https://saigoneer.com/"]
AP_STAY_ALLOWED = ["https://www.roots.gov.sg/sitemap-pages.xml", "https://www.wellingtonnz.com/",
                   "https://admin.wellingtonnz.com/sitemap.xml", "https://visithcmc.vn/feed/"]


# Open data at scale (research 2026-11-18): GTFS feeds, Wikivoyage editions, JMA normals.
WIKIVOYAGE_EDITIONS = {
    f"wikivoyage-dump-{c}": "wikivoyage_dump"
    for c in ("de fr es it pt nl pl ru uk zh ja sv fi he fa vi tr el cs id hi bn").split()
}
GTFS_OPEN_DATA_ON = {
    "prague-pid-gtfs", "budapest-bkk-gtfs", "mbta-gtfs", "cta-gtfs", "king-county-metro-gtfs",
    "ireland-nta-gtfs", "lithuania-gtfs", "luxembourg-gtfs",
}


def test_open_data_gtfs_feeds_2026_11_18(generated):
    assert GTFS_OPEN_DATA_ON <= s2s.GTFS_ON
    for sid in GTFS_OPEN_DATA_ON:
        spec = generated[sid]
        assert spec["enabled"] and spec["adapter"] == "gtfs_static", sid
        assert spec["source_class"] == "official" and len(spec["seeds"]) == 1, sid
        assert sid in _ramp("ramp-3.txt") and sid not in _ramp("ramp-2.txt"), sid
    # One merged national feed for Lithuania instead of 61 municipal zips.
    assert generated["lithuania-gtfs"]["seeds"] == ["https://www.visimarsrutai.lt/gtfs/gtfs_all.zip"]
    # Luxembourg's zip name changes weekly: the seed is the udata index the adapter resolves.
    lu = generated["luxembourg-gtfs"]
    assert lu["seeds"][0].startswith("https://data.public.lu/api/1/datasets/")
    assert lu["licence"]["spdx"] == "CC-BY-4.0"
    assert generated["ireland-nta-gtfs"]["licence"]["attribution"].startswith("National Transport Authority")
    for sid in ("mbta-gtfs", "cta-gtfs", "king-county-metro-gtfs"):
        assert generated[sid]["licence"]["class"] == "api_terms" and generated[sid]["licence"]["url"], sid
    for sid in ("mbta-gtfs", "cta-gtfs"):
        assert generated[sid]["display_requirements"], sid
    # STM's zip redirects into a robots-disallowed path: off with the reason.
    stm = generated["stm-montreal-gtfs"]
    assert stm["enabled"] is False and "stm-montreal-gtfs" not in s2s.GTFS_ON
    assert "Disallow: /*getcontentasset" in " ".join(stm["notes"].split())


def test_wikivoyage_editions_2026_11_18(generated):
    assert len(WIKIVOYAGE_EDITIONS) == 22
    for sid in WIKIVOYAGE_EDITIONS:
        code = sid.rsplit("-", 1)[1]
        spec = generated[sid]
        assert spec["enabled"] and spec["adapter"] == "wikivoyage_dump", sid
        assert spec["licence"]["class"] == "share_alike" and spec["licence"]["spdx"] == "CC-BY-SA-4.0", sid
        assert f"{code}.wikivoyage.org" in spec["licence"]["attribution"], sid
        assert spec["seeds"] == [
            f"https://dumps.wikimedia.org/{code}wikivoyage/latest/{code}wikivoyage-latest-pages-articles.xml.bz2"
        ], sid
        assert spec["cadence"] == "monthly" and spec["method"] == "dump", sid
        assert spec["kinds"] == ["place", "attraction", "hotel"] and spec["llm_kinds"] == [], sid
        assert sid in _ramp("ramp-3.txt") and sid not in _ramp("ramp-2.txt"), sid


def test_jma_normals_and_bathing_water_2026_11_18(generated):
    jma = generated["jma-climate-normals"]
    assert jma["enabled"] and jma["method"] == "html" and jma["max_depth"] == 0
    assert len(jma["seeds"]) == 21 and all("/stats/etrn/view/nml_sfc_ym.php?" in u for u in jma["seeds"])
    assert jma["kinds"] == ["weather_normal"] and jma["source_class"] == "official"
    assert jma["licence"]["spdx"] == "CC-BY-4.0" and jma["cadence"] == "yearly"
    assert "jma-climate-normals" in _ramp("ramp-2.txt") and "jma-climate-normals" in _ramp("ramp-3.txt")
    bw = generated["ea-bathing-water-profiles"]
    reason = " ".join(bw["notes"].split()).split("Disabled: ", 1)[1]
    assert bw["enabled"] is False and reason.startswith("robots.txt") and "/doc/" in reason
    for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
        assert "ea-bathing-water-profiles" not in _ramp(ramp)


# Hidden gems, small islands and remote places (research 2026-11-18, vetted rows, owner
# rules of 2026-11-18). Every row of the batch has notes starting "Research 2026-11-18 (hidden
# gems". Off rows carry one of these reason prefixes; everything else is on, in ramp-2 and
# ramp-3, facts only unless an open licence was quoted.
# 9 refused at the first full crawl, 2026-11-19
HIDDEN_GEMS_COUNTS = {'total': 220, 'on': 185, 'off': 35}
HIDDEN_GEMS_OFF_PREFIXES = ("re-test from the crawler VM", "terms forbid storage/redistribution",
                            "meta robots", "seed unresolved", "adapter pending") + FULL_CRAWL_REFUSED_PREFIXES
# Parked or hijacked official domains: denylisted, never a source.
HIDDEN_GEMS_DENIED = [
    "https://www.gotobermuda.com/", "https://www.visitgozo.com/", "https://www.iledere.com/",
    "https://www.visittci.com/", "https://www.les-plus-beaux-villages-de-france.org/",
    "https://www.bradtguides.com/", "https://www.parconazionalepantelleria.it/", "https://www.menorca.es/",
    "https://www.iatlanticas.es/", "https://www.ats.aq/", "https://www.visituganda.com/",
    "https://www.tz-vis.hr/", "https://www.lastovo-tz.net/", "https://www.socotraisland.org/",
    "https://www.visitamazonas.com.br/", "https://www.timsnepal.com/", "https://www.visitrajaampat.com/",
    "https://www.ethiopia.travel/", "https://www.visitnaxos.gr/", "https://visitfolegandros.gr/",
    "https://www.tongaholiday.com/", "https://www.visitmarshallislands.com/lander",
    "https://www.discovernauru.com/", "https://www.visitcuracao.com/", "https://www.kosrae.com/",
]
# Hosts next to the new entries that stay allowed.
HIDDEN_GEMS_ALLOWED = [
    "https://visithvar.hr/", "https://visitkorcula.eu/", "https://www.visitmadeira.com/en",
    "https://www.experiencealula.com/", "https://www.myoutislands.com/", "https://www.visit-micronesia.fm/",
    "https://www.visitsolomons.com.sb/", "https://www.uzbekistan.travel/", "https://www.visitsaaremaa.ee/",
]


def _hidden_gems_ids():
    return [s["id"] for s in SEEDS
            if (s.get("notes") or "").startswith("Research 2026-11-18 (hidden gems")]


@pytest.mark.parametrize("url", HIDDEN_GEMS_DENIED)
def test_hidden_gems_denylist_entries(url):
    assert s2s.denied(url, DENYLIST) is not None, url


@pytest.mark.parametrize("url", HIDDEN_GEMS_ALLOWED)
def test_hidden_gems_neighbours_stay_allowed(url):
    assert s2s.denied(url, DENYLIST) is None, url


def test_hidden_gems_sources_2026_11_18(generated):
    import re

    ids = _hidden_gems_ids()
    assert len(ids) == len(set(ids)) == HIDDEN_GEMS_COUNTS["total"]
    r2, r3 = _ramp("ramp-2.txt"), _ramp("ramp-3.txt")
    on, off = [], []
    for sid in ids:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        (on if spec["enabled"] else off).append(sid)
        assert spec["max_depth"] <= 2 and spec["daily_budget"] <= 100, sid
        for m in re.finditer(r"Crawl-delay (\d+)", notes):
            assert spec["min_delay_s"] >= int(m.group(1)), sid
        if "reserves content from AI crawlers" in notes:
            assert spec["llm_kinds"] == [], sid
        if not spec["enabled"]:
            reason = notes.split("Disabled: ", 1)[1]
            assert reason.startswith(HIDDEN_GEMS_OFF_PREFIXES), (sid, reason)
            assert sid not in r2 and sid not in r3, sid
            continue
        assert spec["method"] in ("html", "rss", "sitemap") and not spec["needs_key"], sid
        assert sid in r2 and sid in r3, sid
        assert spec["cadence"] in ("daily", "weekly", "monthly"), sid
        if spec["method"] == "rss":
            assert spec["cadence"] == "daily" and spec["daily_budget"] <= 50, sid
        if spec["licence"]["class"] not in ("open", "share_alike"):
            assert spec["licence"]["class"] == "facts_only" and "facts only" in notes, sid
        if "Terms unconfirmed" in notes:
            assert "owner accepted for non-commercial use 2026-11-18" in notes, sid
        if "Terms restrict to personal/non-commercial use" in notes:
            assert "owner accepted for non-commercial use 2026-11-18" in notes, sid
        if spec["category"] in ("blogs", "news"):
            assert spec["source_class"] == "editorial", sid
        for u in spec["seeds"] + spec["sitemaps"] + spec["feeds"]:
            assert s2s.denied(u, DENYLIST) is None, (sid, u)
    assert len(on) == HIDDEN_GEMS_COUNTS["on"] and len(off) == HIDDEN_GEMS_COUNTS["off"], (len(on), len(off))
    # Parked domains found at onboarding are denylisted and no source reads them.
    parked = ("visitmarshallislands.com", "discovernauru.com", "visitcuracao.com", "kosrae.com")
    for sid, spec in generated.items():
        for u in spec["seeds"] + spec["sitemaps"] + spec["feeds"]:
            assert not any(h in u for h in parked), (sid, u)


def test_hidden_gems_part_1_asia_pacific_africa(generated):
    def notes(sid):
        return " ".join(generated[sid]["notes"].split())

    # A web-firewall page on robots.txt at onboarding: off, re-tested from the VM, not denylisted.
    for sid, host in (("tb-jeju", "www.visitjeju.net"), ("gov-ulleung", "www.ulleung.go.kr")):
        assert not generated[sid]["enabled"] and "Disabled: re-test from the crawler VM" in notes(sid), sid
        assert s2s.denied(f"https://{host}/", DENYLIST) is None, host
    ki = generated["tb-king-island"]
    assert not ki["enabled"] and "terms forbid storage/redistribution" in notes("tb-king-island")
    # Terms limited to personal/non-commercial use: on, facts only (owner rule 2026-11-18).
    for sid in ("tb-arunachal", "tb-amami", "gov-aad", "tb-eswatini"):
        spec = generated[sid]
        assert spec["enabled"] and spec["licence"]["class"] == "facts_only", sid
        assert "personal/non-commercial use; owner accepted for non-commercial use 2026-11-18" in notes(sid), sid
    assert generated["gov-torres-strait"]["licence"]["spdx"] == "CC-BY-4.0"
    # Seeds on redirect hosts use the final host.
    assert generated["np-rapa-nui"]["sitemaps"] == ["https://www.rapanuinationalpark.com/sitemap.xml"]
    assert generated["tb-japan-beautiful-villages"]["feeds"] == ["https://japan-utsukushii-mura.com/feed/"]
    # Advisory-risk destinations are added normally and flagged.
    for sid in ("tb-pakistan", "gov-gilgit-baltistan", "tb-kamchatka"):
        assert generated[sid]["enabled"] and "Advisory-risk destination" in notes(sid), sid


def test_hidden_gems_part_2_europe_atlantic_americas(generated):
    def notes(sid):
        return " ".join(generated[sid]["notes"].split())

    tr = generated["gov-tristan"]
    assert not tr["enabled"] and "Disabled: seed unresolved" in notes("gov-tristan")
    for sid, clause in (("mag-polarjournal", "in databases"), ("od-wdpa", "redistribute"),
                        ("guide-visitikaria", "storage")):
        assert not generated[sid]["enabled"] and clause in notes(sid), sid
        assert "terms forbid storage/redistribution" in notes(sid), sid
    # Terms limited to personal/non-commercial use: on, facts only (owner rule 2026-11-18).
    for sid in ("tb-shetland", "tb-orkney", "tb-jersey", "tb-borkum", "tb-statia", "tb-ouessant"):
        spec = generated[sid]
        assert spec["enabled"] and spec["licence"]["class"] == "facts_only", sid
        assert "personal/non-commercial use; owner accepted for non-commercial use 2026-11-18" in notes(sid), sid
    for sid in ("tb-ouessant", "tb-shetland", "tr-orkney-ferries"):
        assert generated[sid]["llm_kinds"] == [], sid
    assert generated["tb-shetland"]["min_delay_s"] >= 5 and generated["tb-french-guiana"]["min_delay_s"] >= 20
    vh = generated["gov-usgs-volcanoes"]
    assert vh["seeds"] == ["https://www.usgs.gov/programs/VHP"] and vh["licence"]["class"] == "open"
    assert vh["sitemaps"] == [] and vh["include"]
    for sid in ("od-natural-earth", "od-ramsar-rsis"):
        assert "Disabled: adapter pending" in notes(sid), sid
    # Seeds on redirect hosts use the final host.
    assert generated["tb-lofoten"]["sitemaps"] == ["https://visitlofoten.com/en/sitemap_index.xml"]
    assert generated["tb-bornholm"]["sitemaps"] == ["https://www.bornholminfo.dk/sitemap.xml"]
    assert generated["tb-american-samoa"]["seeds"] == ["https://www.visitamericansamoa.org/"]


def test_full_crawl_refusals_2026_11_19(generated):
    gov = {"aphis-pet-travel", "city-dresden", "park-usfs", "tb-angkor-enterprise", "tr-miaa-ph",
           "visa-imigrasi-id", "visa-bh-evisa", "gov-symi", "tb-tourisme-ci", "gov-pt-vistos",
           "tb-ktb-turkey", "ev-nhb-sg", "ev-roots-sg"}
    kept = {"news-traveldailynews", "tr-mwaa"}  # off for a re-test, not denylisted
    assert len(FULL_CRAWL_REFUSED_2026_11_19) == 52
    for sid in FULL_CRAWL_REFUSED_2026_11_19:
        spec = generated[sid]
        reason = " ".join(spec["notes"].split()).split("Disabled: ", 1)[1]
        assert spec["enabled"] is False and reason.startswith(FULL_CRAWL_REFUSED_PREFIXES), (sid, reason)
        for ramp in ("ramp-1.txt", "ramp-2.txt", "ramp-3.txt"):
            assert sid not in _ramp(ramp), (sid, ramp)
        urls = spec["seeds"] + spec["sitemaps"] + spec["feeds"]
        hit = any(s2s.denied(u if isinstance(u, str) else u["url"], DENYLIST) for u in urls)
        if sid in gov or sid in kept:
            assert "re-test later" in reason and not hit, sid
        else:
            assert "host denylisted" in reason and hit, sid


# --- Carbon footprint corpus (research vetted 2026-11-19) ---------------------------------
# New app section `carbon`. Every row has notes starting "Research 2026-11-19 (carbon footprint
# corpus". ADD rows on html/rss/sitemap are on (ramp-2 and ramp-3); Eurostat datasets run on the
# generic eurostat_jsonstat reader (ramp-3 only); dataset rows wait for a reader, API rows for a
# key, owner-decision rows for the owner.
CARBON_ADAPTERS = {
    "od-eurostat-avia-paoc": "eurostat_jsonstat",
    "od-eurostat-avia-par": "eurostat_jsonstat",
    "od-eurostat-air-emissions": "eurostat_jsonstat",
}
# factor_table (2026-11-19): emission-factor and grid-intensity files, ramp-3 only.
FACTOR_TABLE_SOURCES = {
    "od-desnz-ghg-factors": "emission_factor",
    "od-owid-food-ghg": "emission_factor",
    "carbon-owid-grapher-ci": "grid_carbon_intensity",
}
# Keyed carbon APIs on with the owner's key (2026-11-19): adapter, Secret Manager name. ramp-3 only.
CARBON_KEYED = {"carbon-ember-api": ("ember_api", "crawler-src-ember-key")}
# Owner decisions of 2026-11-19 turned on (html, facts only).
CARBON_OWNER_ON = ("tr-flix-methodology", "carbon-breeam-projects", "carbon-greenbooklive")
CARBON_COUNTS = {"total": 310, "on": 150, "adapter pending": 130, "needs an API key": 25, "robots.txt": 3,
                 "duplicate": 1, "gone": 1}
CARBON_DENIED = ["https://www.climatiq.io/terms", "https://www.goldstandard.org/", "https://registry.verra.org/",
                 "https://media.amtrak.com/terms-of-use/", "https://ghgprotocol.org/terms-use",
                 "https://www.hestia.earth/explorer"]
CARBON_STAY_ALLOWED = ["https://api.hestia.earth/", "https://www.amtrak.com/", "https://www.myclimate.org/en/"]


def _carbon_ids(seeds):
    return [s["id"] for s in seeds if (s.get("notes") or "").startswith("Research 2026-11-19 (carbon footprint corpus")]


def test_carbon_section_is_in_every_closed_list():
    assert "carbon" in s2s.SECTIONS
    enum = SCHEMA["properties"]["relevance"]["properties"]["sections"]["items"]["enum"]
    assert enum[-1] == "carbon"


def test_carbon_corpus_2026_11_19(generated):
    seeds = yaml.safe_load((ROOT / "docs" / "research" / "seeds.yaml").read_text())["seeds"]
    ids = _carbon_ids(seeds)
    assert len(ids) == CARBON_COUNTS["total"] == len(set(ids))
    r2, r3 = set(_ramp("ramp-2.txt")), set(_ramp("ramp-3.txt"))
    on, reasons = [], {}
    for sid in ids:
        spec = generated[sid]
        notes = " ".join(spec["notes"].split())
        assert spec["category"] == "carbon" and spec["relevance"]["sections"][0] == "carbon", sid
        assert spec["source_class"] in ("official", "reference"), sid
        if not spec["enabled"]:
            reason = notes.split("Disabled: ", 1)[1]
            prefix = next(p for p in ("adapter pending", "needs an API key", "owner decision", "robots.txt",
                                      "duplicate", "gone") if reason.startswith(p))
            reasons[prefix] = reasons.get(prefix, 0) + 1
            assert sid not in r2 and sid not in r3, sid
            assert spec["needs_key"] is (prefix == "needs an API key"), sid
            if spec["needs_key"]:
                assert spec["key_secret"] == f"crawler-src-{sid}-key" and "https://" in reason, sid
                assert "(free" in reason or "(paid or partner access" in reason, sid
            continue
        on.append(sid)
        if sid in CARBON_KEYED:
            adapter, secret = CARBON_KEYED[sid]
            assert spec["adapter"] == adapter and spec["method"] == "api", sid
            assert spec["needs_key"] and spec["key_secret"] == secret, sid
            assert spec["kinds"] == ["grid_carbon_intensity"] and spec["llm_kinds"] == [], sid
            assert spec["cadence"] == "weekly" and spec["daily_budget"] <= 10 and spec["max_depth"] == 0, sid
            assert all("api_key" not in u for u in spec["seeds"]), sid
            assert sid in r3 and sid not in r2, sid
            continue
        assert not spec["needs_key"] and spec["daily_budget"] <= 100 and spec["max_depth"] <= 1, sid
        assert spec["cadence"] in ("monthly", "quarterly", "yearly"), sid
        if sid in CARBON_ADAPTERS:
            assert spec["adapter"] == CARBON_ADAPTERS[sid] and spec["method"] == "api", sid
            assert spec["seeds"][0].startswith("https://ec.europa.eu/eurostat/api/dissemination/"), sid
            assert sid in r3 and sid not in r2, sid
            continue
        if sid in FACTOR_TABLE_SOURCES:
            assert spec["adapter"] == "factor_table" and spec["method"] == "api", sid
            assert spec["kinds"] == [FACTOR_TABLE_SOURCES[sid]] and spec["llm_kinds"] == [], sid
            assert len(spec["seeds"]) == 1 and spec["max_depth"] == 0, sid
            assert sid in r3 and sid not in r2, sid
            continue
        assert spec["method"] in ("html", "rss", "sitemap") and spec["adapter"] is None, sid
        assert sid in r2 and sid in r3, sid
        lic = spec["licence"]["class"]
        assert lic in ("open", "facts_only") or sid in CORPUS_SHARE_ALIKE, sid
        if lic == "facts_only":
            assert ("owner accepted for non-commercial use 2026-11-18" in notes
                    or (sid in CARBON_OWNER_ON and "owner accepted 2026-11-19" in notes)), sid
        if "AI-reserved robots" in notes:
            assert spec["llm_kinds"] == [], sid
        # Data files are never html seeds; PDFs are (raw capture).
        for u in spec["seeds"]:
            assert not re.search(r"\.(csv|zip|xlsx?|ods|parquet|json)(\?|$)", u), (sid, u)
        if spec["method"] == "sitemap" and spec["sitemaps"][0].split("/")[2] in (
                "www.carbontrust.com", "www.untourism.int", "uic.org", "www.transportenvironment.org"):
            assert spec["include"], sid  # big multi-topic sites: carbon paths only
    assert len(on) == CARBON_COUNTS["on"]
    assert reasons == {k: v for k, v in CARBON_COUNTS.items() if k not in ("total", "on")}
    # Duplicates and merged rows: one source per site.
    assert "carbon-emsd-hk" not in generated and "impactco2" in generated and "api-impactco2" not in generated
    assert generated["gov-uba-emissionsdaten"]["include"]
    # Google TIM: docs on GitHub (share-alike, depth 0), the API waits for a key, google.com stays denied.
    tim = generated["od-google-tim-github"]
    assert tim["enabled"] and tim["licence"]["class"] == "share_alike" and tim["max_depth"] == 0
    assert generated["api-google-tim"]["enabled"] is False and generated["api-google-tim"]["needs_key"]
    assert generated["od-ghgp-transport-tool"]["enabled"] is False
    # Owner decisions (2026-11-19): on where robots.txt allows; Crawl-delay 10 honoured.
    for sid in CARBON_OWNER_ON:
        assert generated[sid]["enabled"] and generated[sid]["licence"]["class"] == "facts_only", sid
        assert generated[sid]["include"], sid
    assert generated["carbon-breeam-projects"]["min_delay_s"] >= 10
    assert s2s.denied("https://corporate.flix.com/flix-sustainability/", DENYLIST) is None
    assert generated["carbon-electricitymaps-api"]["needs_key"] is True
    for sid in ("carbon-climatiq-oefdb", "od-fr-base-carbone-v17", "carbon-ember-grid", "od-sncf-info-ges"):
        assert generated[sid]["enabled"] is False, sid
    for url in CARBON_DENIED:
        assert s2s.denied(url, DENYLIST) is not None, url
    for url in CARBON_STAY_ALLOWED:
        assert s2s.denied(url, DENYLIST) is None, url
