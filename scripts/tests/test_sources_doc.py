"""docs/SOURCES.md and docs/wiki/Sources.md are generated from sources/*.yaml and kept in
sync: the source list cannot grow (or change) without the registry showing it."""

from __future__ import annotations

import sys
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import sources_doc  # noqa: E402


def test_registry_is_up_to_date():
    assert sources_doc.main(["--check"]) == 0, "run: python3 scripts/sources_doc.py"


def test_registry_lists_every_source_under_its_sections():
    text = (ROOT / "docs" / "SOURCES.md").read_text()
    assert (ROOT / "docs" / "wiki" / "Sources.md").read_text() == text
    for p in (ROOT / "sources").glob("*.yaml"):
        spec = yaml.safe_load(p.read_text())
        assert f"| `{spec['id']}` |" in text, spec["id"]
        assert spec["relevance"]["why"] in " ".join(text.split()), spec["id"]
    assert "Missing relevance" not in text


def test_new_source_makes_the_registry_stale(tmp_path):
    src = tmp_path / "sources"
    src.mkdir()
    for p in (ROOT / "sources").glob("*.yaml"):
        (src / p.name).write_text(p.read_text())
    spec = yaml.safe_load((src / "met-norway.yaml").read_text())
    spec["id"] = "new-weather"
    (src / "new-weather.yaml").write_text(yaml.safe_dump(spec))
    assert sources_doc.main(["--check", "--sources", str(src)]) == 1


def test_section_titles_cover_every_section():
    sys.path.insert(0, str(ROOT / "scripts"))
    import seeds_to_sources as s2s

    assert list(sources_doc.SECTION_TITLES) == list(s2s.SECTIONS)


def test_needs_a_licence_or_key_section():
    text = (ROOT / "docs" / "SOURCES.md").read_text()
    assert "## Needs a licence or API key" in text
    section = text.split("## Needs a licence or API key", 1)[1]
    for e in sources_doc.load_licence_only():
        assert e["name"] in section, e["name"]
    for name in ("IATA Timatic", "TravelDoc", "Rome2Rio"):
        assert name in section, name
    for p in (ROOT / "sources").glob("*.yaml"):
        spec = yaml.safe_load(p.read_text())
        if spec.get("needs_key") and not spec.get("enabled", True):
            sid = spec["id"]
            assert f"| `{sid}` |" in section, spec["id"]
