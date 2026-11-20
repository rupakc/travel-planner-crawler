//! `wiki_hooks`: place facts from the Wikipedia "Did you know", "On this day" and unusual-
//! articles pages, read as wikitext (`/wiki/<page>?action=raw`; robots.txt of every wiki:
//! `/wiki/` allowed for `*` (no VoyagerBot group, no AI-bot block), `/w/` and `/api/`
//! disallowed, so no `api.php`; each page path below was checked against its host's
//! robots.txt on 2026-11-20).
//!
//! Wikipedia text is CC BY-SA 4.0: a hook is kept **as written** (it is short), marked
//! `text_origin: source_hook`, `llm_ok: false` and licence `share_alike`, so it is never merged
//! into our own wording and never given to a model. A hook is a place fact only once it is
//! resolved to a place: each item lists the articles the hook links (`place.link_candidates`,
//! bold links first) and the read API serves it only when one of them is a sitelink of a
//! Wikidata place (`wikidata_facts`), whose QID and GeoNames id it then takes.
//!
//! Parsing (pure, [`parse`]): one hook per bullet line (`*`, `#`) or table row (`| … || …`);
//! comments, `<ref>`s and `<noinclude>` blocks are removed, templates only when they lead or
//! end the line (a hook with a template inside its text is dropped, since its words would be
//! lost); links become their text; namespaced links (files, categories) are not candidates.
//! "On this day" lines starting with a year give `date_ref`. Hooks over 280 characters, without
//! a link or shorter than 20 characters are dropped.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, on_origin};
use super::place_facts::{Fact, MAX_TEXT_CHARS, TextOrigin, short_hash, squash, strip_html};

/// Adapter id.
pub const ID: &str = "wiki_hooks";
/// Most hooks kept from one page.
pub const MAX_HOOKS: usize = 500;
/// Most link candidates per hook.
pub const MAX_CANDIDATES: usize = 8;

/// The kind of a hook page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPage {
    /// "Did you know": recent-article hooks (`fun`).
    Dyk,
    /// "On this day" / selected anniversaries (`historical`).
    Otd,
    /// Unusual articles (`unbelievable`: oddities).
    Unusual,
}

impl HookPage {
    /// `source_field` of its facts.
    pub fn field(self) -> &'static str {
        match self {
            HookPage::Dyk => "dyk",
            HookPage::Otd => "otd",
            HookPage::Unusual => "unusual",
        }
    }

    /// `fact_type` of its facts.
    pub fn fact_type(self) -> &'static str {
        match self {
            HookPage::Dyk => "fun",
            HookPage::Otd => "historical",
            HookPage::Unusual => "unbelievable",
        }
    }
}

/// One Wikipedia and its hook pages.
#[derive(Debug, Clone, Copy)]
pub struct WikiSite {
    /// Wikidata site id (`dewiki`).
    pub site: &'static str,
    /// Host.
    pub host: &'static str,
    /// Language (BCP 47).
    pub lang: &'static str,
    /// Hook page titles (URL form, `_` for spaces) and their kinds.
    pub pages: &'static [(&'static str, HookPage)],
}

/// The wikis and pages listed ADD in the place-facts research (2026-11-20), every path
/// robots-checked. Archive subpages (`<page>/<sub>`) belong to their page.
pub static SITES: &[WikiSite] = &[
    WikiSite {
        site: "zhwiki",
        host: "zh.wikipedia.org",
        lang: "zh",
        pages: &[
            ("Wikipedia:新条目推荐", HookPage::Dyk),
            ("Wikipedia:历史上的今天", HookPage::Otd),
            ("Wikipedia:維基誌異", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "jawiki",
        host: "ja.wikipedia.org",
        lang: "ja",
        pages: &[
            ("Wikipedia:月間新記事賞", HookPage::Dyk),
            ("Wikipedia:今日は何の日", HookPage::Otd),
            ("Wikipedia:珍項目", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "kowiki",
        host: "ko.wikipedia.org",
        lang: "ko",
        pages: &[
            ("위키백과:알고_계십니까", HookPage::Dyk),
            ("위키백과:오늘의_역사", HookPage::Otd),
            ("위키백과:별난_문서", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "arwiki",
        host: "ar.wikipedia.org",
        lang: "ar",
        pages: &[
            ("ويكيبيديا:هل_تعلم", HookPage::Dyk),
            ("ويكيبيديا:في_هذا_اليوم", HookPage::Otd),
            ("ويكيبيديا:فريق_مقالات_غريبة", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "bgwiki",
        host: "bg.wikipedia.org",
        lang: "bg",
        pages: &[
            ("Уикипедия:Последни_попълнения", HookPage::Dyk),
            ("Уикипедия:Избрани_годишнини", HookPage::Otd),
            ("Уикипедия:Нестандартни_статии", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "bnwiki",
        host: "bn.wikipedia.org",
        lang: "bn",
        pages: &[
            ("উইকিপিডিয়া:আপনি_জানেন_কি", HookPage::Dyk),
            ("উইকিপিডিয়া:নির্বাচিত_বার্ষিকী", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "cawiki",
        host: "ca.wikipedia.org",
        lang: "ca",
        pages: &[
            ("Viquipèdia:Efemèrides_seleccionades", HookPage::Otd),
            ("Viquipèdia:Articles_inusuals", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "cswiki",
        host: "cs.wikipedia.org",
        lang: "cs",
        pages: &[
            ("Wikipedie:Zajímavosti", HookPage::Dyk),
            ("Wikipedie:Vybraná_výročí_dne", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "dawiki",
        host: "da.wikipedia.org",
        lang: "da",
        pages: &[
            ("Wikipedia:Vidste_du_at...", HookPage::Dyk),
            ("Wikipedia:Årets_dage_kort", HookPage::Otd),
            ("Wikipedia:Spøjse_artikler", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "dewiki",
        host: "de.wikipedia.org",
        lang: "de",
        pages: &[
            ("Wikipedia:Hauptseite/Schon_gewusst", HookPage::Dyk),
            ("Wikipedia:Hauptseite/Jahrestage", HookPage::Otd),
            ("Wikipedia:Kuriositätenkabinett", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "eswiki",
        host: "es.wikipedia.org",
        lang: "es",
        pages: &[
            ("Wikipedia:¿Sabías_que...", HookPage::Dyk),
            ("Wikipedia:Efemérides", HookPage::Otd),
            ("Wikipedia:Artículos_peculiares", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "etwiki",
        host: "et.wikipedia.org",
        lang: "et",
        pages: &[
            ("Vikipeedia:Kas_teadsid", HookPage::Dyk),
            ("Vikipeedia:Valitud_sündmused", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "fawiki",
        host: "fa.wikipedia.org",
        lang: "fa",
        pages: &[
            (
                "ویکی\u{200c}پدیا:آیا_می\u{200c}دانستید_که...؟",
                HookPage::Dyk,
            ),
            ("ویکی\u{200c}پدیا:یادبودهای_برگزیده", HookPage::Otd),
            ("ویکی\u{200c}پدیا:مقالات_عجیب", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "fiwiki",
        host: "fi.wikipedia.org",
        lang: "fi",
        pages: &[
            ("Wikipedia:Tiesitkö,_että...", HookPage::Dyk),
            ("Wikipedia:Päivämalline", HookPage::Otd),
            ("Wikipedia:Erikoiset_artikkelit", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "frwiki",
        host: "fr.wikipedia.org",
        lang: "fr",
        pages: &[
            ("Wikipédia:Le_saviez-vous_?", HookPage::Dyk),
            ("Wikipédia:Éphéméride", HookPage::Otd),
            ("Wikipédia:Articles_insolites", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "hewiki",
        host: "he.wikipedia.org",
        lang: "he",
        pages: &[
            ("ויקיפדיה:הידעת?", HookPage::Dyk),
            ("ויקיפדיה:היום_בהיסטוריה", HookPage::Otd),
            ("ויקיפדיה:ערכים_יוצאי_דופן", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "hiwiki",
        host: "hi.wikipedia.org",
        lang: "hi",
        pages: &[("विकिपीडिया:क्या_आप_जानते_हैं", HookPage::Dyk)],
    },
    WikiSite {
        site: "huwiki",
        host: "hu.wikipedia.org",
        lang: "hu",
        pages: &[
            ("Wikipédia:Évfordulók", HookPage::Otd),
            ("Wikipédia:Az_élet_könnyedebb_oldala", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "idwiki",
        host: "id.wikipedia.org",
        lang: "id",
        pages: &[
            ("Wikipedia:Tahukah_Anda", HookPage::Dyk),
            ("Wikipedia:Hari_Ini_Dalam_Sejarah", HookPage::Otd),
            (
                "Wikipedia:Artikel-artikel_yang_tidak_lazim",
                HookPage::Unusual,
            ),
        ],
    },
    WikiSite {
        site: "itwiki",
        host: "it.wikipedia.org",
        lang: "it",
        pages: &[
            ("Wikipedia:Lo_sapevi_che", HookPage::Dyk),
            ("Wikipedia:Voci_insolite", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "mswiki",
        host: "ms.wikipedia.org",
        lang: "ms",
        pages: &[
            ("Wikipedia:Tahukah_anda", HookPage::Dyk),
            ("Wikipedia:Hari_ulang_tahun_pilihan", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "nlwiki",
        host: "nl.wikipedia.org",
        lang: "nl",
        pages: &[
            ("Wikipedia:Wist_je_dat", HookPage::Dyk),
            ("Wikipedia:Dag_uit_de_geschiedenis", HookPage::Otd),
            (
                "Wikipedia:Humor_en_onzin/Opmerkelijke_pagina's",
                HookPage::Unusual,
            ),
        ],
    },
    WikiSite {
        site: "nowiki",
        host: "no.wikipedia.org",
        lang: "nb",
        pages: &[
            ("Wikipedia:Visste_du_at", HookPage::Dyk),
            ("Wikipedia:Hendelser", HookPage::Otd),
            (
                "Wikipedia:Liste_over_merkverdige_artikler",
                HookPage::Unusual,
            ),
        ],
    },
    WikiSite {
        site: "plwiki",
        host: "pl.wikipedia.org",
        lang: "pl",
        pages: &[
            ("Wikiprojekt:Czy_wiesz", HookPage::Dyk),
            ("Wikiprojekt:Rocznice", HookPage::Otd),
            ("Wikipedia:Nietypowe_artykuły", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "ptwiki",
        host: "pt.wikipedia.org",
        lang: "pt",
        pages: &[
            ("Wikipédia:Sabia_que", HookPage::Dyk),
            ("Wikipédia:Efemérides", HookPage::Otd),
            ("Wikipédia:Artigos_peculiares", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "rowiki",
        host: "ro.wikipedia.org",
        lang: "ro",
        pages: &[
            ("Wikipedia:Știați_că", HookPage::Dyk),
            ("Format:Aniversările_zilei", HookPage::Otd),
            ("Wikipedia:Articole_neobișnuite", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "ruwiki",
        host: "ru.wikipedia.org",
        lang: "ru",
        pages: &[
            ("Проект:Знаете_ли_вы", HookPage::Dyk),
            ("Проект:События_дня", HookPage::Otd),
            ("Википедия:Необычные_статьи", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "simplewiki",
        host: "simple.wikipedia.org",
        lang: "en",
        pages: &[("Wikipedia:Did_you_know", HookPage::Dyk)],
    },
    WikiSite {
        site: "skwiki",
        host: "sk.wikipedia.org",
        lang: "sk",
        pages: &[("Wikipédia:Zaujímavosti", HookPage::Dyk)],
    },
    WikiSite {
        site: "slwiki",
        host: "sl.wikipedia.org",
        lang: "sl",
        pages: &[
            ("Wikipedija:Ste_vedeli", HookPage::Dyk),
            ("Wikipedija:Izbrane_obletnice", HookPage::Otd),
            ("Wikipedija:Nenavadni_članki", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "srwiki",
        host: "sr.wikipedia.org",
        lang: "sr",
        pages: &[
            ("Википедија:Да_ли_сте_знали", HookPage::Dyk),
            ("Википедија:Изабрани_догађаји", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "svwiki",
        host: "sv.wikipedia.org",
        lang: "sv",
        pages: &[
            ("Wikipedia:Visste_du_att...", HookPage::Dyk),
            ("Wikipedia:Dagens_händelser", HookPage::Otd),
            ("Wikipedia:Kuriosaartiklar", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "tawiki",
        host: "ta.wikipedia.org",
        lang: "ta",
        pages: &[
            ("விக்கிப்பீடியா:உங்களுக்குத்_தெரியுமா", HookPage::Dyk),
            ("விக்கிப்பீடியா:ஆண்டு_நிறைவுகள்", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "thwiki",
        host: "th.wikipedia.org",
        lang: "th",
        pages: &[
            ("วิกิพีเดีย:รู้ไหมว่า", HookPage::Dyk),
            ("วิกิพีเดีย:วันนี้ในอดีต", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "trwiki",
        host: "tr.wikipedia.org",
        lang: "tr",
        pages: &[
            ("Vikipedi:Biliyor_muydunuz?", HookPage::Dyk),
            ("Vikipedi:Tarihte_bugün", HookPage::Otd),
        ],
    },
    WikiSite {
        site: "ukwiki",
        host: "uk.wikipedia.org",
        lang: "uk",
        pages: &[
            ("Вікіпедія:Проєкт:Чи_ви_знаєте", HookPage::Dyk),
            ("Вікіпедія:Проєкт:Цей_день_в_історії", HookPage::Otd),
            ("Вікіпедія:Неочікувані_статті", HookPage::Unusual),
        ],
    },
    WikiSite {
        site: "viwiki",
        host: "vi.wikipedia.org",
        lang: "vi",
        pages: &[
            ("Wikipedia:Bạn_có_biết", HookPage::Dyk),
            ("Wikipedia:Ngày_này_năm_xưa", HookPage::Otd),
        ],
    },
];

/// The site of a host.
pub fn site_by_host(host: &str) -> Option<&'static WikiSite> {
    SITES.iter().find(|s| s.host.eq_ignore_ascii_case(host))
}

fn pct_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).and_then(|d| u8::try_from(d).ok());
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A title in its compared form: decoded, `_` → space, trimmed.
fn norm_title(t: &str) -> String {
    squash(&pct_decode(t).replace('_', " "))
}

/// The page kind of `title` on `site` (a listed page or one of its subpages).
pub fn page_kind(site: &WikiSite, title: &str) -> Option<HookPage> {
    let t = norm_title(title);
    site.pages
        .iter()
        .filter(|(p, _)| {
            let p = norm_title(p);
            t == p || t.starts_with(&format!("{p}/"))
        })
        .max_by_key(|(p, _)| p.len())
        .map(|(_, k)| *k)
}

/// The site, page title and kind of a hook-page URL (`/wiki/<title>`, any query).
pub fn locate(url: &CanonicalUrl) -> Option<(&'static WikiSite, String, HookPage)> {
    let u = url.as_url();
    let title = u.path().strip_prefix("/wiki/")?;
    let host = u.host_str()?;
    let site = match site_by_host(host) {
        Some(s) => s,
        // Not a wiki host (a test server): the first site that lists the path.
        None => SITES.iter().find(|s| page_kind(s, title).is_some())?,
    };
    let kind = page_kind(site, title)?;
    Some((site, norm_title(title), kind))
}

/// One hook.
#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    /// Plain text.
    pub text: String,
    /// Linked article titles (bold first), normalised.
    pub links: Vec<String>,
    /// Leading year of an "On this day" line.
    pub year: Option<i64>,
}

static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("re"));
static REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<ref\b[^>]*/>|<ref\b[^>]*>.*?</ref\s*>|<noinclude>.*?</noinclude>")
        .expect("re")
});
static LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\[\]|]+)(?:\|([^\[\]]*))?\]\]").expect("re"));
static EXT_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(?:https?:)?//[^\s\]]+(?:\s+([^\]]*))?\]").expect("re"));
static YEAR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:\[\[)?(\d{3,4})(?:\]\])?\s*(?:年|[–—:\-]|\s-\s)").expect("re")
});
static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{[^{}]*\}\}").expect("re"));

/// Whether a link target is in a namespace (`File:`, `Kategorie:`, …) or another wiki.
fn namespaced(target: &str) -> bool {
    target
        .split_once(':')
        .is_some_and(|(ns, _)| !ns.is_empty() && !ns.contains(' ') && ns.chars().count() <= 20)
}

/// A link target as an article title: `None` for namespaced, interwiki or section-only links.
fn article(target: &str) -> Option<String> {
    let t = target.trim();
    if t.is_empty() || t.starts_with(':') || t.starts_with('#') {
        return None;
    }
    let t = t.split('#').next().unwrap_or(t).trim();
    if namespaced(t) {
        return None;
    }
    let t = norm_title(t);
    // Year and date articles ("1244", "27. Oktober") are never places.
    if t.chars().all(|c| !c.is_alphabetic()) {
        return None;
    }
    let mut c = t.chars();
    let first = c.next()?;
    Some(first.to_uppercase().chain(c).collect())
}

/// Strips templates that lead or end `s`; `None` when one is left inside the text.
fn strip_edge_templates(s: &str) -> Option<String> {
    let mut s = s.trim().to_string();
    loop {
        let before = s.clone();
        if s.starts_with("{{")
            && let Some(m) = TEMPLATE.find(&s)
            && m.start() == 0
        {
            s = s[m.end()..].trim_start().to_string();
        }
        if s.ends_with("}}")
            && let Some(m) = TEMPLATE.find_iter(&s).last()
            && m.end() == s.len()
        {
            s = s[..m.start()].trim_end().to_string();
        }
        if s == before {
            break;
        }
    }
    (!s.contains("{{") && !s.contains("}}")).then_some(s)
}

/// Text and links of one hook line (bullets and table bars already removed).
fn hook_of(line: &str, year_page: bool) -> Option<Hook> {
    let line = strip_edge_templates(line)?;
    let year = if year_page {
        YEAR.captures(&line)
            .and_then(|c| c[1].parse::<i64>().ok())
            .filter(|y| (100..=2100).contains(y))
    } else {
        None
    };
    // Bold spans: odd segments between `'''` markers.
    let mut bold_links = Vec::new();
    let mut other_links = Vec::new();
    for (i, seg) in line.split("'''").enumerate() {
        for c in LINK.captures_iter(seg) {
            if let Some(a) = article(&c[1]) {
                let list = if i % 2 == 1 {
                    &mut bold_links
                } else {
                    &mut other_links
                };
                if !list.contains(&a) {
                    list.push(a);
                }
            }
        }
    }
    let mut links = bold_links;
    for l in other_links {
        if !links.contains(&l) {
            links.push(l);
        }
    }
    links.truncate(MAX_CANDIDATES);
    if links.is_empty() {
        return None;
    }
    let text = LINK.replace_all(&line, |c: &regex::Captures<'_>| {
        let target = c[1].trim();
        let shown = c.get(2).map(|d| d.as_str().to_string());
        if !target.starts_with(':') && namespaced(target) {
            // Files, categories and other namespaces: no text.
            return String::new();
        }
        shown.unwrap_or_else(|| target.trim_start_matches(':').to_string())
    });
    let text = EXT_LINK.replace_all(&text, |c: &regex::Captures<'_>| {
        c.get(1).map_or(String::new(), |d| d.as_str().to_string())
    });
    let text = strip_html(&text.replace("'''", "").replace("''", ""));
    let text = squash(&text);
    let n = text.chars().count();
    if !(20..=MAX_TEXT_CHARS).contains(&n) {
        return None;
    }
    Some(Hook { text, links, year })
}

/// The hooks of one hook page's wikitext. Pure.
pub fn parse(wikitext: &str, kind: HookPage) -> Vec<Hook> {
    let clean = COMMENT.replace_all(wikitext, "");
    let clean = REF.replace_all(&clean, "");
    let mut out: Vec<Hook> = Vec::new();
    for raw in clean.lines() {
        let l = raw.trim();
        let body: String = if let Some(b) = l.strip_prefix(['*', '#']) {
            b.trim_start_matches(['*', '#', ':']).trim().to_string()
        } else if l.starts_with('|')
            && !l.starts_with("|-")
            && !l.starts_with("|}")
            && !l.starts_with("|+")
        {
            // A table row: its cells, joined.
            l.trim_start_matches('|')
                .split("||")
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(" \u{2014} ")
        } else {
            continue;
        };
        if let Some(h) = hook_of(&body, kind == HookPage::Otd)
            && !out.iter().any(|o| o.text == h.text)
        {
            out.push(h);
            if out.len() >= MAX_HOOKS {
                break;
            }
        }
    }
    out
}

/// The `place_fact` items of a stored hook page (`url` is the page's `?action=raw` URL). Pure.
pub fn items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<(String, Value)>> {
    let (site, title, kind) =
        locate(url).ok_or_else(|| Error::Invalid("wiki_hooks: not a listed hook page".into()))?;
    let text = std::str::from_utf8(body).map_err(|e| Error::Invalid(format!("wiki_hooks: {e}")))?;
    let page_url = format!("https://{}/wiki/{}", site.host, title.replace(' ', "_"));
    let mut out = Vec::new();
    for h in parse(text, kind) {
        let mut place = Map::new();
        place.insert(
            "link_candidates".into(),
            Value::Array(
                h.links
                    .iter()
                    .map(|l| json!(format!("{}:{l}", site.site)))
                    .collect(),
            ),
        );
        let fact = Fact {
            id: format!("{}:{}:{}", site.site, kind.field(), short_hash(&h.text)),
            place,
            fact_type: kind.fact_type(),
            text: h.text,
            lang: site.lang.to_string(),
            origin: TextOrigin::SourceHook,
            date_ref: h.year.map(|y| format!("{y:04}")),
            quantity: None,
            source_field: kind.field().to_string(),
            source: (
                "Wikipedia",
                "Wikipedia contributors (Wikimedia Foundation)",
                if kind == HookPage::Dyk {
                    "yes"
                } else {
                    "partial"
                },
            ),
            licence: (
                "share_alike",
                "CC-BY-SA-4.0",
                format!("Wikipedia contributors ({}), CC BY-SA 4.0", site.host),
            ),
            confidence: "extracted",
            source_url: Some(page_url.clone()),
            as_of: None,
        };
        out.extend(fact.into_data());
    }
    Ok(out)
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct WikiHooks;

#[async_trait]
impl SourceAdapter for WikiHooks {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        if locate(&url).is_none() {
            return Err(Error::Invalid("wiki_hooks: not a listed hook page".into()));
        }
        let raw = on_origin(&url, &format!("{}?action=raw", url.as_url().path()))?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("text/x-wiki, text/plain"),
            hint: Some(MediaType::Text),
            max_body_bytes: 4 * 1024 * 1024,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, raw, &opts, &mut rep).await?;
        check_status(&ex)?;
        std::str::from_utf8(&ex.body).map_err(|e| Error::Invalid(format!("wiki_hooks: {e}")))?;
        rep.complete = true;
        Ok(rep)
    }
}
