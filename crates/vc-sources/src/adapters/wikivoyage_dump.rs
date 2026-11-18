//! `wikivoyage_dump`: a Wikivoyage `pages-articles` dump (CC BY-SA 4.0) of any language
//! edition, streamed and bzip2-decoded on the fly. Filter v2 keeps main-namespace pages
//! (articles and redirects) as JSON objects
//! `{page_id, title, ns, lang, rev_id, timestamp, url, redirect?, text?, listings?}`;
//! contributor names are not kept (attribution is "Wikivoyage contributors" + the page URL).
//!
//! The edition comes from the dump's URL (`/<lang>wikivoyage/…`, default `en`). Each edition
//! names its listing templates in its own language (`{{see}}` in English, `{{vCard}}` in German,
//! `{{voir}}` in French, `{{zobacz}}` in Polish, `{{לינה}}` in Hebrew…): [`EDITIONS`] maps every
//! edition's listing templates (read from the 2026-11-01 dumps on 2026-11-18) to one
//! language-neutral type (`see do buy eat drink sleep go other`), and [`PARAMS`] maps the local
//! parameter names (`nom`, `nombre`, `indirizzo`, `שם`, `عرض جغرافیایی`, …) to English ones. The
//! listings of each article are kept as structured JSON next to its text.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::bytes::Regex;
use serde_json::{Map, Value, json};
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::xml_unescape;
use super::dump::{DumpFilter, DumpSettings, OutputKind, Split, Unit, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "wikivoyage-ns0-v2";

/// Article URL prefix of the English edition.
pub const WIKI_BASE: &str = "https://en.wikivoyage.org/wiki/";

// ---------------------------------------------------------------------------------------------
// Editions
// ---------------------------------------------------------------------------------------------

/// The language-neutral type of a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingType {
    /// Sights (`see`).
    See,
    /// Activities (`do`).
    Do,
    /// Shopping (`buy`).
    Buy,
    /// Restaurants (`eat`).
    Eat,
    /// Bars and nightlife (`drink`).
    Drink,
    /// Accommodation (`sleep`).
    Sleep,
    /// Getting there and around (`go`).
    Go,
    /// Anything else (`listing` without a known type).
    Other,
}

impl ListingType {
    /// The English section name.
    pub fn as_str(self) -> &'static str {
        match self {
            ListingType::See => "see",
            ListingType::Do => "do",
            ListingType::Buy => "buy",
            ListingType::Eat => "eat",
            ListingType::Drink => "drink",
            ListingType::Sleep => "sleep",
            ListingType::Go => "go",
            ListingType::Other => "other",
        }
    }
}

use ListingType::{Buy, Do, Drink, Eat, Go, Other, See, Sleep};

/// How a listing template gives its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tpl {
    /// The template itself names the section (`{{see}}`, `{{voir}}`).
    Fixed(ListingType),
    /// A generic listing whose `type=` (or local alias) names it (`{{listing}}`, `{{vCard}}`);
    /// none, or one that is not a known section or point-of-interest type, is
    /// [`ListingType::Other`].
    Typed,
    /// A map marker with positional `name|lat|long` and a `typ=` (Polish `{{Znacznik}}`).
    Marker,
}

/// One Wikivoyage language edition.
#[derive(Debug)]
pub struct Edition {
    /// Language code (`de`, `pt`, …), also the dump's `<code>wikivoyage` prefix.
    pub code: &'static str,
    /// English name of the language (attribution).
    pub language: &'static str,
    /// Local listing templates (lower case, spaces for underscores), on top of the English
    /// ones every edition also uses ([`COMMON`]).
    pub listings: &'static [(&'static str, Tpl)],
    /// Page-level templates whose named `lat`/`long` (or a local alias) locate the article.
    pub page_geo: &'static [&'static str],
    /// Local names of `{{geo|lat|long}}` (the article's own coordinates).
    pub geo: &'static [&'static str],
    /// Local names of `{{mapframe|lat|long}}` (a map centre: used when the article states no
    /// coordinates of its own).
    pub maps: &'static [&'static str],
}

/// English listing templates, used by every edition.
pub const COMMON: &[(&str, Tpl)] = &[
    ("see", Tpl::Fixed(See)),
    ("do", Tpl::Fixed(Do)),
    ("buy", Tpl::Fixed(Buy)),
    ("eat", Tpl::Fixed(Eat)),
    ("drink", Tpl::Fixed(Drink)),
    ("sleep", Tpl::Fixed(Sleep)),
    ("go", Tpl::Fixed(Go)),
    ("listing", Tpl::Typed),
    ("vcard", Tpl::Typed),
];

/// Every edition the adapter knows (English first). Template names were read from each
/// edition's 2026-11-01 `pages-articles` dump (template use counts in ns-0 articles).
pub const EDITIONS: &[Edition] = &[
    Edition {
        code: "en",
        language: "English",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "de",
        language: "German",
        listings: &[],
        page_geo: &["geodata"],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "fr",
        language: "French",
        listings: &[
            ("voir", Tpl::Fixed(See)),
            ("faire", Tpl::Fixed(Do)),
            ("acheter", Tpl::Fixed(Buy)),
            ("manger", Tpl::Fixed(Eat)),
            ("sortir", Tpl::Fixed(Drink)),
            ("se loger", Tpl::Fixed(Sleep)),
            ("aller", Tpl::Fixed(Go)),
            ("circuler", Tpl::Fixed(Go)),
        ],
        page_geo: &[
            "info ville",
            "info région",
            "info pays",
            "info parc",
            "info quartier",
            "bannière page",
        ],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "es",
        language: "Spanish",
        listings: &[
            ("ver", Tpl::Fixed(See)),
            ("hacer", Tpl::Fixed(Do)),
            ("comprar", Tpl::Fixed(Buy)),
            ("comer", Tpl::Fixed(Eat)),
            ("beber", Tpl::Fixed(Drink)),
            ("dormir", Tpl::Fixed(Sleep)),
            ("ir", Tpl::Fixed(Go)),
            ("listado", Tpl::Typed),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "it",
        language: "Italian",
        listings: &[],
        page_geo: &["quickbarcity", "quickbarregion", "quickbarcountry"],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "pt",
        language: "Portuguese",
        listings: &[
            ("veja", Tpl::Fixed(See)),
            ("faça", Tpl::Fixed(Do)),
            ("compre", Tpl::Fixed(Buy)),
            ("coma", Tpl::Fixed(Eat)),
            ("come", Tpl::Fixed(Eat)),
            ("beba", Tpl::Fixed(Drink)),
            ("durma", Tpl::Fixed(Sleep)),
            ("vá", Tpl::Fixed(Go)),
            ("outroitem", Tpl::Fixed(Other)),
            ("item", Tpl::Fixed(Other)),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "nl",
        language: "Dutch",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "pl",
        language: "Polish",
        listings: &[("zobacz", Tpl::Fixed(See)), ("znacznik", Tpl::Marker)],
        page_geo: &[],
        geo: &[],
        maps: &["mapa"],
    },
    Edition {
        code: "ru",
        language: "Russian",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "uk",
        language: "Ukrainian",
        listings: &[
            ("автобусом", Tpl::Fixed(Go)),
            ("автомобілем", Tpl::Fixed(Go)),
            ("потягом", Tpl::Fixed(Go)),
            ("літаком", Tpl::Fixed(Go)),
            ("кораблем", Tpl::Fixed(Go)),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "zh",
        language: "Chinese",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "ja",
        language: "Japanese",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "sv",
        language: "Swedish",
        listings: &[("att se", Tpl::Fixed(See)), ("boende", Tpl::Fixed(Sleep))],
        page_geo: &["quickbarcity", "quickbarregion", "quickbardistrict"],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "fi",
        language: "Finnish",
        listings: &[("tapahtuma", Tpl::Fixed(Do))],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "he",
        language: "Hebrew",
        listings: &[
            ("מוקדי", Tpl::Fixed(See)),
            ("פעילויות", Tpl::Fixed(Do)),
            ("קניות", Tpl::Fixed(Buy)),
            ("אוכל", Tpl::Fixed(Eat)),
            ("שתייה", Tpl::Fixed(Drink)),
            ("לינה", Tpl::Fixed(Sleep)),
            ("רשימה", Tpl::Typed),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "fa",
        language: "Persian",
        listings: &[
            ("دیدن", Tpl::Fixed(See)),
            ("انجام\u{200c}دادن", Tpl::Fixed(Do)),
            ("خریدن", Tpl::Fixed(Buy)),
            ("خوردن", Tpl::Fixed(Eat)),
            ("نوشیدن", Tpl::Fixed(Drink)),
            ("خوابیدن", Tpl::Fixed(Sleep)),
            ("فهرست\u{200c}بندی", Tpl::Typed),
        ],
        page_geo: &[],
        geo: &[],
        maps: &["قاب نقشه"],
    },
    Edition {
        code: "vi",
        language: "Vietnamese",
        listings: &[
            ("xem", Tpl::Fixed(See)),
            ("tham quan", Tpl::Fixed(See)),
            ("làm", Tpl::Fixed(Do)),
            ("mua", Tpl::Fixed(Buy)),
            ("ăn", Tpl::Fixed(Eat)),
            ("uống", Tpl::Fixed(Drink)),
            ("ngủ", Tpl::Fixed(Sleep)),
            ("nghỉ ngơi", Tpl::Fixed(Sleep)),
            ("địa điểm", Tpl::Typed),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "tr",
        language: "Turkish",
        listings: &[
            ("görün", Tpl::Fixed(See)),
            ("yapın", Tpl::Fixed(Do)),
            ("satın alın", Tpl::Fixed(Buy)),
            ("yiyin", Tpl::Fixed(Eat)),
            ("için", Tpl::Fixed(Drink)),
            ("uyuyun", Tpl::Fixed(Sleep)),
            ("listeleme", Tpl::Typed),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "el",
        language: "Greek",
        listings: &[
            ("οδικώς", Tpl::Fixed(Go)),
            ("αεροπορικώς", Tpl::Fixed(Go)),
            ("σιδηροδρομικώς", Tpl::Fixed(Go)),
            ("ακτοπλοϊκώς", Tpl::Fixed(Go)),
            ("λεωφορείο", Tpl::Fixed(Go)),
            ("μετρό", Tpl::Fixed(Go)),
            ("prosvasi", Tpl::Fixed(Go)),
        ],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "cs",
        language: "Czech",
        listings: &[("bod", Tpl::Typed)],
        page_geo: &[],
        geo: &[],
        maps: &["mapa"],
    },
    Edition {
        code: "id",
        language: "Indonesian",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &[],
    },
    Edition {
        code: "hi",
        language: "Hindi",
        listings: &[],
        page_geo: &[],
        geo: &[],
        maps: &["मैपफ्रेम"],
    },
    Edition {
        code: "bn",
        language: "Bengali",
        listings: &[
            ("দেখুন", Tpl::Fixed(See)),
            ("করুন", Tpl::Fixed(Do)),
            ("কিনুন", Tpl::Fixed(Buy)),
            ("আহার করুন", Tpl::Fixed(Eat)),
            ("খান", Tpl::Fixed(Eat)),
            ("পান করুন", Tpl::Fixed(Drink)),
            ("রাত্রিযাপন করুন", Tpl::Fixed(Sleep)),
            ("রাত্রিযাপন", Tpl::Fixed(Sleep)),
            ("যান", Tpl::Fixed(Go)),
            ("তালিকাভুক্তকরণ", Tpl::Typed),
            ("তালিকা", Tpl::Typed),
        ],
        page_geo: &[],
        geo: &["ভূ"],
        maps: &["মানচিত্রের কাঠামো"],
    },
];

/// The edition with this code.
pub fn edition(code: &str) -> Option<&'static Edition> {
    EDITIONS.iter().find(|e| e.code == code)
}

/// The edition of a dump URL (`…/<code>wikivoyage/…` or `<code>wikivoyage-…`); English when the
/// URL names none the adapter knows.
pub fn edition_of_url(url: &str) -> &'static Edition {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_default();
    path.split(['/', '-'])
        .filter_map(|seg| seg.strip_suffix("wikivoyage"))
        .find_map(edition)
        .unwrap_or(&EDITIONS[0])
}

/// The canonical name of a template or parameter (lower case, `_` → space, collapsed spaces).
fn norm(name: &str) -> String {
    name.replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl Edition {
    /// How `template` (any case) is a listing in this edition, if it is one.
    pub fn listing_template(&self, template: &str) -> Option<Tpl> {
        let n = norm(template);
        self.listings
            .iter()
            .chain(COMMON)
            .find(|(t, _)| *t == n)
            .map(|(_, k)| *k)
    }

    /// The article URL of `title`.
    pub fn page_url(&self, title: &str) -> String {
        format!(
            "https://{}.wikivoyage.org/wiki/{}",
            self.code,
            title.replace(' ', "_")
        )
    }

    /// The record-id prefix of the edition's pages (`enwikivoyage`).
    pub fn id_prefix(&self) -> String {
        format!("{}wikivoyage", self.code)
    }
}

/// Local parameter names → the English ones (one table for every edition: the local words do
/// not collide). The value is the English key kept in the listing JSON.
pub const PARAMS: &[(&str, &str)] = &[
    // name
    ("name", "name"),
    ("nom", "name"),
    ("nombre", "name"),
    ("nome", "name"),
    ("namn", "name"),
    ("nimi", "name"),
    ("naam", "name"),
    ("název", "name"),
    ("nazwa", "name"),
    ("adı", "name"),
    ("tên", "name"),
    ("שם", "name"),
    ("نام", "name"),
    ("নাম", "name"),
    ("название", "name"),
    ("назва", "name"),
    // alternative name
    ("alt", "alt"),
    ("שם חלופי", "alt"),
    ("نام دیگر", "alt"),
    ("tên khác", "alt"),
    ("diğer adı", "alt"),
    ("অন্য", "alt"),
    // coordinates
    ("lat", "lat"),
    ("latitude", "lat"),
    ("latitud", "lat"),
    ("enlem", "lat"),
    ("vĩ độ", "lat"),
    ("عرض جغرافیایی", "lat"),
    ("অক্ষাংশ", "lat"),
    ("zeměpisná šířka", "lat"),
    ("long", "long"),
    ("lon", "long"),
    ("lng", "long"),
    ("longitude", "long"),
    ("longitud", "long"),
    ("boylam", "long"),
    ("kinh độ", "long"),
    ("طول جغرافیایی", "long"),
    ("দ্রাঘিমাংশ", "long"),
    ("zeměpisná délka", "long"),
    // website
    ("url", "url"),
    ("sito", "url"),
    ("site", "url"),
    ("web", "url"),
    ("www", "url"),
    ("webbplats", "url"),
    ("האתר הרשמי", "url"),
    ("نشانی اینترنتی", "url"),
    ("trang web", "url"),
    ("ইউআরএল", "url"),
    // address
    ("address", "address"),
    ("adresse", "address"),
    ("dirección", "address"),
    ("indirizzo", "address"),
    ("endereço", "address"),
    ("adres", "address"),
    ("adress", "address"),
    ("adresa", "address"),
    ("כתובת", "address"),
    ("نشانی", "address"),
    ("địa chỉ", "address"),
    ("ঠিকানা", "address"),
    // phone
    ("phone", "phone"),
    ("téléphone", "phone"),
    ("tlf", "phone"),
    ("tel", "phone"),
    ("telefon", "phone"),
    ("telefono", "phone"),
    ("telefone", "phone"),
    ("טלפון", "phone"),
    ("تلفن", "phone"),
    ("điện thoại", "phone"),
    ("ফোন", "phone"),
    // opening hours
    ("hours", "hours"),
    ("horaire", "hours"),
    ("horario", "hours"),
    ("orari", "hours"),
    ("funcionamento", "hours"),
    ("godziny", "hours"),
    ("besökstid", "hours"),
    ("otevírací doba", "hours"),
    ("שעות", "hours"),
    ("ساعت\u{200c}ها", "hours"),
    ("giờ hoạt động", "hours"),
    ("সময়সূচী", "hours"),
    ("çalışma saatleri", "hours"),
    // price
    ("price", "price"),
    ("prix", "price"),
    ("precio", "price"),
    ("prezzo", "price"),
    ("preço", "price"),
    ("ceny", "price"),
    ("cena", "price"),
    ("pris", "price"),
    ("מחיר", "price"),
    ("بها", "price"),
    ("giá", "price"),
    ("মূল্য", "price"),
    ("ücret", "price"),
    // description
    ("content", "description"),
    ("description", "description"),
    ("descripción", "description"),
    ("descrizione", "description"),
    ("sobre", "description"),
    ("opis", "description"),
    ("beskrivning", "description"),
    ("popis", "description"),
    ("treść", "description"),
    ("תיאור", "description"),
    ("متن", "description"),
    ("nội dung", "description"),
    ("বিবরণ", "description"),
    ("içerik", "description"),
    // type (generic listings)
    ("type", "type"),
    ("typ", "type"),
    ("tipo", "type"),
    ("grupo", "type"),
    ("tip", "type"),
    ("loại", "type"),
    ("סוג", "type"),
    ("ধরন", "type"),
    // Wikidata item
    ("wikidata", "wikidata"),
];

fn param_key(local: &str) -> Option<&'static str> {
    let n = norm(local);
    PARAMS.iter().find(|(l, _)| *l == n).map(|(_, e)| *e)
}

/// Type values of generic listings (English section names, the local words, and the
/// fine-grained `{{vCard}}` types of de/ja/cs) → the listing type. `None`: not a point of
/// interest (a city, region or map area), so no listing.
pub fn type_of_value(v: &str) -> Option<ListingType> {
    let v = norm(v);
    Some(match v.as_str() {
        "see"
        | "voir"
        | "ver"
        | "veja"
        | "zobacz"
        | "xem"
        | "atrakcja"
        | "zabytek"
        | "natura"
        | "museum"
        | "church"
        | "castle"
        | "monument"
        | "palace"
        | "temple"
        | "shrine"
        | "mosque"
        | "synagogue"
        | "cathedral"
        | "monastery"
        | "building"
        | "ruins"
        | "ruin"
        | "archaeological site"
        | "gallery"
        | "viewpoint"
        | "landscape"
        | "park"
        | "garden"
        | "national park"
        | "nature reserve"
        | "beach"
        | "waterfall"
        | "mountain"
        | "lake"
        | "cave"
        | "square"
        | "tower"
        | "bridge"
        | "memorial"
        | "zoo"
        | "aquarium"
        | "landmark"
        | "artwork"
        | "theme park"
        | "fort"
        | "castle ruin"
        | "chapel"
        | "cemetery"
        | "historic site"
        | "heritage" => See,
        "do" | "faire" | "hacer" | "faça" | "làm" | "theatre" | "theater" | "cinema" | "sports"
        | "swimming pool" | "spa" | "tour" | "stadium" | "golf" | "ski" | "festival" | "event"
        | "learn" | "work" | "concert hall" => Do,
        "buy" | "acheter" | "comprar" | "compre" | "mua" | "zakupy" | "shop" | "market"
        | "mall" | "supermarket" | "shopping center" | "shopping centre" | "bookshop" => Buy,
        "eat" | "manger" | "comer" | "coma" | "ăn" | "jedzenie" | "restaurant" | "cafe"
        | "café" | "bakery" | "fast food" | "food" | "pizzeria" => Eat,
        "drink" | "sortir" | "beber" | "beba" | "uống" | "bar" | "pub" | "nightclub" | "club"
        | "wine bar" | "brewery" | "winery" => Drink,
        "sleep" | "se loger" | "dormir" | "durma" | "ngủ" | "nocleg" | "hotel" | "hostel"
        | "guest house" | "guesthouse" | "motel" | "campsite" | "camping" | "apartment"
        | "holiday flat" | "bed and breakfast" | "ryokan" | "inn" | "accommodation" | "resort"
        | "pension" | "capsule hotel" | "minshuku" => Sleep,
        "go" | "aller" | "circuler" | "ir" | "đi" | "transport" | "airport" | "station"
        | "train" | "bus" | "ferry" | "railway station" | "bus station" | "taxi" | "car rental"
        | "port" | "metro" | "tram" | "subway" => Go,
        "city" | "town" | "village" | "region" | "country" | "island" | "vicinity" | "district"
        | "around" | "ville" | "ciudad" | "città" => return None,
        _ => Other,
    })
}

// ---------------------------------------------------------------------------------------------
// Wikitext templates
// ---------------------------------------------------------------------------------------------

/// One template invocation: its name and parameters (`(Some(key), value)` or positional).
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    /// The name as written (trimmed).
    pub name: String,
    /// The parameters in order.
    pub params: Vec<(Option<String>, String)>,
}

impl Template {
    /// The value of the first non-empty parameter whose (local) name means `english`.
    pub fn named(&self, english: &str) -> Option<&str> {
        self.params.iter().find_map(|(k, v)| {
            let k = k.as_deref()?;
            (param_key(k) == Some(english) && !v.trim().is_empty()).then_some(v.trim())
        })
    }

    /// The `i`-th positional parameter.
    pub fn positional(&self, i: usize) -> Option<&str> {
        self.params
            .iter()
            .filter(|(k, _)| k.is_none())
            .nth(i)
            .map(|(_, v)| v.trim())
    }
}

/// The end (exclusive) of the template starting at `start` (`{{`), if it closes.
fn template_end(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    let (mut depth, mut i) = (0usize, start);
    while i + 1 < b.len() {
        match (b[i], b[i + 1]) {
            (b'{', b'{') => {
                depth += 1;
                i += 2;
            }
            (b'}', b'}') => {
                depth = depth.saturating_sub(1);
                i += 2;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Parses one `{{…}}` invocation (`s` includes the braces). Parameters split at `|` outside
/// nested templates and links.
pub fn parse_template(s: &str) -> Option<Template> {
    let inner = s.strip_prefix("{{")?.strip_suffix("}}")?;
    let mut parts = Vec::new();
    let (mut cur, mut braces, mut brackets) = (String::new(), 0usize, 0usize);
    let mut it = inner.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' if it.peek() == Some(&'{') => {
                it.next();
                braces += 1;
                cur.push_str("{{");
            }
            '}' if it.peek() == Some(&'}') => {
                it.next();
                braces = braces.saturating_sub(1);
                cur.push_str("}}");
            }
            '[' if it.peek() == Some(&'[') => {
                it.next();
                brackets += 1;
                cur.push_str("[[");
            }
            ']' if it.peek() == Some(&']') => {
                it.next();
                brackets = brackets.saturating_sub(1);
                cur.push_str("]]");
            }
            '|' if braces == 0 && brackets == 0 => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let mut parts = parts.into_iter();
    let name = parts.next()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let params = parts
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !k.contains(['{', '[', '<']) => {
                (Some(k.trim().to_string()), v.to_string())
            }
            _ => (None, p),
        })
        .collect();
    Some(Template { name, params })
}

static EXT_LINK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[(https?://[^\s\]]+)\s+([^\]]*)\]").expect("regex"));
static WIKI_LINK: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\[\[(?:[^|\]]*\|)?([^\]]*)\]\]").expect("regex"));
static INNER_TEMPLATE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\{\{[^{}]*\}\}").expect("regex"));
static HTML_TAG: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?s)<!--.*?-->|<[^>]*>").expect("regex"));

/// Wiki markup → plain text (links to their text, templates and tags dropped), capped.
fn plain(v: &str, max_chars: usize) -> String {
    let mut t = v.to_string();
    for _ in 0..4 {
        let n = INNER_TEMPLATE.replace_all(&t, "").into_owned();
        if n == t {
            break;
        }
        t = n;
    }
    let t = EXT_LINK.replace_all(&t, "$2");
    let t = WIKI_LINK.replace_all(&t, "$1");
    let t = HTML_TAG.replace_all(&t, "");
    let t = t
        .replace("'''", "")
        .replace("''", "")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&amp;", "&");
    t.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn coord(v: Option<&str>, limit: f64) -> Option<f64> {
    let x: f64 = v?.trim().parse().ok()?;
    (x.is_finite() && x.abs() <= limit && x != 0.0).then_some(x)
}

/// The listing of one template invocation, if it is a listing of `ed` (JSON, English keys).
pub fn listing(ed: &Edition, t: &Template) -> Option<Value> {
    let kind = ed.listing_template(&t.name)?;
    let raw_type = t.named("type").map(str::to_string);
    let ty = match kind {
        Tpl::Fixed(ty) => ty,
        Tpl::Typed | Tpl::Marker => match raw_type.as_deref() {
            Some(v) => type_of_value(v)?,
            None => Other,
        },
    };
    let mut name_raw = t.named("name").map(str::to_string);
    let (mut lat, mut lng) = (coord(t.named("lat"), 90.0), coord(t.named("long"), 180.0));
    if kind == Tpl::Marker {
        name_raw = name_raw.or_else(|| t.positional(0).map(str::to_string));
        lat = lat.or_else(|| coord(t.positional(1), 90.0));
        lng = lng.or_else(|| coord(t.positional(2), 180.0));
    }
    let name_raw = name_raw?;
    // `name=[https://site Name]` (Polish): the link is the website.
    let link_url = EXT_LINK.captures(&name_raw).map(|c| c[1].to_string());
    let name = plain(&name_raw, 160);
    if name.is_empty() {
        return None;
    }
    let mut m = Map::new();
    m.insert("type".into(), ty.as_str().into());
    if let Some(r) = raw_type.map(|r| plain(&r, 60)).filter(|r| !r.is_empty()) {
        m.insert("subtype".into(), r.into());
    }
    m.insert("name".into(), name.into());
    if let (Some(a), Some(o)) = (lat, lng) {
        m.insert("lat".into(), json!(a));
        m.insert("lng".into(), json!(o));
    }
    let url = t
        .named("url")
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .filter(|u| !u.contains(char::is_whitespace))
        .map(str::to_string)
        .or(link_url);
    if let Some(u) = url {
        m.insert("url".into(), u.into());
    }
    for (key, max) in [
        ("alt", 160),
        ("address", 200),
        ("phone", 80),
        ("hours", 200),
        ("price", 200),
        ("description", 600),
        ("wikidata", 20),
    ] {
        if let Some(v) = t
            .named(key)
            .map(|v| plain(v, max))
            .filter(|v| !v.is_empty())
        {
            m.insert(key.into(), v.into());
        }
    }
    Some(Value::Object(m))
}

/// The name of the template opening at `start` (`{{`), up to its first `|` or `}`.
fn head(text: &str, start: usize) -> &str {
    let rest = &text[start + 2..];
    &rest[..rest.find(['|', '}']).unwrap_or(rest.len())]
}

/// The listings of an article's wikitext, in order.
pub fn listings(ed: &Edition, text: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find("{{") {
        let start = i + off;
        if ed.listing_template(head(text, start)).is_some()
            && let Some(end) = template_end(text, start)
        {
            if let Some(l) = parse_template(&text[start..end]).and_then(|t| listing(ed, &t)) {
                out.push(l);
            }
            i = end;
        } else {
            i = start + 2;
        }
    }
    out
}

/// The article's own coordinates, best first: a positional `{{geo|lat|long}}` (or an edition's
/// local name for it), then named `lat`/`long` on one of the edition's page-level templates
/// (`{{GeoData}}`, `{{Info Ville}}`…), then the centre of the first `{{mapframe|lat|long}}`.
pub fn article_coords(ed: &Edition, text: &str) -> Option<(f64, f64)> {
    let mut named: Option<(f64, f64)> = None;
    let mut map: Option<(f64, f64)> = None;
    let mut i = 0;
    while let Some(off) = text[i..].find("{{") {
        let start = i + off;
        let name = norm(head(text, start));
        if ed.listing_template(&name).is_some() {
            // Listings carry their own coordinates, not the article's: skip their bodies.
            i = template_end(text, start).unwrap_or(start + 2);
            continue;
        }
        let geo = name == "geo" || ed.geo.contains(&name.as_str());
        let mapframe = name == "mapframe" || ed.maps.contains(&name.as_str());
        let page = ed.page_geo.contains(&name.as_str());
        if (geo || mapframe || page)
            && let Some(end) = template_end(text, start)
            && let Some(t) = parse_template(&text[start..end])
        {
            let pos = coord(t.positional(0), 90.0).zip(coord(t.positional(1), 180.0));
            if geo && pos.is_some() {
                return pos;
            }
            if mapframe && map.is_none() {
                map = pos;
            }
            if page && named.is_none() {
                named = coord(t.named("lat"), 90.0).zip(coord(t.named("long"), 180.0));
            }
        }
        i = start + 2;
    }
    named.or(map)
}

// ---------------------------------------------------------------------------------------------
// The dump filter
// ---------------------------------------------------------------------------------------------

static TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<title>(.*?)</title>").expect("regex"));
static NS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<ns>(-?\d+)</ns>").expect("regex"));
static PAGE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)</ns>\s*<id>(\d+)</id>").expect("regex"));
static REDIRECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<redirect title="([^"]*)"\s*/>"#).expect("regex"));
static REV_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<revision>\s*<id>(\d+)</id>").expect("regex"));
static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<timestamp>([^<]*)</timestamp>").expect("regex"));
static TEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<text\b[^>]*?(?:/>|>(.*?)</text>)").expect("regex"));

fn cap<'a>(re: &Regex, s: &'a [u8]) -> Option<&'a [u8]> {
    re.captures(s).and_then(|c| c.get(1)).map(|m| m.as_bytes())
}

fn utf8(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).map_err(|e| Error::Invalid(format!("wikivoyage: {e}")))
}

/// Filters one `<page>…</page>` element of the English edition (pure; exposed for tests and
/// re-processing).
pub fn filter_page(page: &[u8]) -> Result<Option<Value>> {
    filter_page_in(&EDITIONS[0], page)
}

/// Filters one `<page>…</page>` element of edition `ed`.
pub fn filter_page_in(ed: &Edition, page: &[u8]) -> Result<Option<Value>> {
    let ns: i64 = cap(&NS, page)
        .and_then(|b| std::str::from_utf8(b).ok()?.parse().ok())
        .ok_or_else(|| Error::Invalid("wikivoyage: page without <ns>".into()))?;
    if ns != 0 {
        return Ok(None);
    }
    let title = xml_unescape(utf8(
        cap(&TITLE, page).ok_or_else(|| Error::Invalid("wikivoyage: no title".into()))?,
    )?);
    let num =
        |re: &Regex| -> Option<u64> { std::str::from_utf8(cap(re, page)?).ok()?.parse().ok() };
    let mut obj = json!({
        "page_id": num(&PAGE_ID),
        "title": title,
        "ns": 0,
        "lang": ed.code,
        "rev_id": num(&REV_ID),
        "timestamp": cap(&TIMESTAMP, page).map(|b| String::from_utf8_lossy(b).into_owned()),
        "url": ed.page_url(&title),
    });
    if let Some(r) = cap(&REDIRECT, page) {
        obj["redirect"] = xml_unescape(utf8(r)?).into();
    } else if let Some(t) = cap(&TEXT, page) {
        let text = xml_unescape(utf8(t)?);
        let ls = listings(ed, &text);
        if !ls.is_empty() {
            obj["listings"] = Value::Array(ls);
        }
        obj["text"] = text.into();
    }
    Ok(Some(obj))
}

struct PageFilter {
    edition: &'static Edition,
}

impl DumpFilter for PageFilter {
    fn version(&self) -> &'static str {
        FILTER_VERSION
    }

    fn split(&self) -> Split {
        Split::Element {
            open: b"<page>",
            close: b"</page>",
        }
    }

    fn output(&self) -> OutputKind {
        OutputKind::JsonArray
    }

    fn filter(&mut self, unit: &[u8]) -> Result<Unit> {
        Ok(match filter_page_in(self.edition, unit)? {
            Some(v) => Unit::Keep(serde_json::to_vec(&v)?),
            None => Unit::Drop,
        })
    }
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct WikivoyageDump {
    settings: DumpSettings,
}

impl WikivoyageDump {
    /// Default settings.
    pub const DEFAULT: WikivoyageDump = WikivoyageDump {
        settings: DumpSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        WikivoyageDump { settings }
    }
}

#[async_trait]
impl SourceAdapter for WikivoyageDump {
    fn id(&self) -> &'static str {
        "wikivoyage_dump"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        let mut f = PageFilter {
            edition: edition_of_url(url.as_str()),
        };
        run_dump(ctx, self.id(), url, &mut f, &self.settings).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One real listing per edition (from the 2026-11-01 dumps, CC BY-SA 4.0 Wikivoyage
    /// contributors): (edition, wikitext, type, name, lat).
    const CASES: &[(&str, &str, &str, &str, Option<f64>)] = &[
        (
            "en",
            "{{see| name=British Museum | url=https://www.britishmuseum.org/ | lat=51.5194 | long=-0.1269 | content=Huge.}}",
            "see",
            "British Museum",
            Some(51.5194),
        ),
        (
            "de",
            "{{vCard|type=building|name=Hoftorturm|lat=46.84780 | long=9.53393}}",
            "see",
            "Hoftorturm",
            Some(46.8478),
        ),
        (
            "fr",
            "{{Voir|nom=Funiculaire du mont Sarakura|type=circuler|latitude=33.85658 |longitude=130.79642 | wikipédia=Funiculaire du mont Sarakura}}",
            "see",
            "Funiculaire du mont Sarakura",
            Some(33.85658),
        ),
        (
            "fr",
            "{{Se loger\n| nom = Aton Palace Luxor\n| téléphone = +20 100 737 4116\n| prix = 45-65 €\n| latitude = 25.766040\n| longitude = 32.639390\n}}",
            "sleep",
            "Aton Palace Luxor",
            Some(25.76604),
        ),
        (
            "es",
            "{{Ver|nombre=Museo Arqueológico Numba Charava|dirección=Los Zorzales.|lat=-31.4217144|long=-64.5124376}}",
            "see",
            "Museo Arqueológico Numba Charava",
            Some(-31.4217144),
        ),
        (
            "es",
            "{{Listado|tipo=dormir|nombre=Hostal Sol|lat=40.4167|long=-3.7033}}",
            "sleep",
            "Hostal Sol",
            Some(40.4167),
        ),
        (
            "it",
            "{{Sleep\n| nome=Hotel Duomo | alt= | sito=https://www.hotelduomo.it/ | email=\n| indirizzo=Via San Raffaele 1 | lat=45.4652 | long=9.1915 | indicazioni=\n| tel=+39 02 8833 | orari= | prezzo=Da 150 €\n| descrizione=Vicino al Duomo.\n}}",
            "sleep",
            "Hotel Duomo",
            Some(45.4652),
        ),
        (
            "pt",
            "{{Durma|nome=Parque de Campismo de Santa Luzia|endereço=Rua do Parque de Campismo, Bairro de Santa Luzia |tel=+351 (273) 417 172|preço=36 €|lat=41.492341|long=-6.285654}}",
            "sleep",
            "Parque de Campismo de Santa Luzia",
            Some(41.492341),
        ),
        (
            "nl",
            "{{See\n|name = Burj Al Arab\n|lat=25.141|long=55.185\n|wikidata=Q62939\n}}",
            "see",
            "Burj Al Arab",
            Some(25.141),
        ),
        (
            "pl",
            "{{zobacz\n| name=[https://www.aalborgakvavit.dk/ Aalborg Akvavit] | alt= | url= | email=\n| adres= | lat=57.053999 | long=9.910703 | wskazówki=\n| opis=Destylarnia.\n}}",
            "see",
            "Aalborg Akvavit",
            Some(57.053999),
        ),
        (
            "pl",
            "{{Znacznik|Port lotniczy Aalborg|57.092778|9.849167|typ=transport}}",
            "go",
            "Port lotniczy Aalborg",
            Some(57.092778),
        ),
        (
            "ru",
            "{{Listing\n   | type = buy|name=MediaMarkt|lat=35.271753387325944|long=-2.93796925658175|lastedit=2024-12-05}}",
            "buy",
            "MediaMarkt",
            Some(35.271753387325944),
        ),
        (
            "uk",
            "{{Потягом|name=Залізничний вокзал|lat=49.8396|long=24.0109}}",
            "go",
            "Залізничний вокзал",
            Some(49.8396),
        ),
        (
            "zh",
            "{{See|name=東山聖方濟各堂|content=又稱東山天主堂。|address=越秀區新河浦路山河後街26號|wikidata=Q32172500|lat=23.120878|long=113.294516}}",
            "see",
            "東山聖方濟各堂",
            Some(23.120878),
        ),
        (
            "ja",
            "{{vCard|type=temple|name=曼荼羅寺|lat=34.2233323|long=133.7503177}}",
            "see",
            "曼荼羅寺",
            Some(34.2233323),
        ),
        (
            "sv",
            "{{Listing|typ=sleep|lat=52.4020|long=16.9117|namn=Hotel Poznań|2}}",
            "sleep",
            "Hotel Poznań",
            Some(52.402),
        ),
        (
            "fi",
            "{{Listing | type = buy |  name = Max Mara | address = Harju 6|lat=59.434623| long= 24.743505}}",
            "buy",
            "Max Mara",
            Some(59.434623),
        ),
        (
            "he",
            "{{לינה | שם =אתר קמפינג | lat = 64.2768 | long = -21.0948}}",
            "sleep",
            "אתר קמפינג",
            Some(64.2768),
        ),
        (
            "he",
            "{{מוקדי\n| שם=המצפה התת-ימי | האתר הרשמי= | lat=29.50401 | long=34.91921 | כתובת=\n| טלפון=08-6364200 | תיאור= אחת האטרקציות הפופולריות ביותר באילת.\n}}",
            "see",
            "המצפה התת-ימי",
            Some(29.50401),
        ),
        (
            "fa",
            "{{خوابیدن\n| نام=Doogles | نام دیگر= | نشانی اینترنتی= | پست الکترونیکی=\n| نشانی= | عرض جغرافیایی=-15.78397 | طول جغرافیایی=35.01488 | مسیرها=\n| متن=\n}}",
            "sleep",
            "Doogles",
            Some(-15.78397),
        ),
        (
            "fa",
            "{{انجام\u{200c}دادن|نام=Hammam|عرض جغرافیایی=35.7|طول جغرافیایی=51.4}}",
            "do",
            "Hammam",
            Some(35.7),
        ),
        (
            "vi",
            "{{Ngủ| tên=Uran Baigal| vĩ độ=47.74239| kinh độ=96.84699}}",
            "sleep",
            "Uran Baigal",
            Some(47.74239),
        ),
        (
            "tr",
            "{{Görün|adı=Deniz Feneri|enlem=41.0916|boylam=31.1198|içerik=Mendireğin ucundaki liman feneri.}}",
            "see",
            "Deniz Feneri",
            Some(41.0916),
        ),
        (
            "el",
            "{{Sleep|name=Rioja Hostal|address=Calle Ejercito Espanol 10|lat=35.293573142534115|long=-2.939047422471579|phone=+34 952 68 27 09}}",
            "sleep",
            "Rioja Hostal",
            Some(35.293573142534115),
        ),
        (
            "el",
            "{{Αεροπορικώς|name=Αερολιμένας Ακτίου|lat=38.9255|long=20.7653}}",
            "go",
            "Αερολιμένας Ακτίου",
            Some(38.9255),
        ),
        (
            "cs",
            "{{Bod|typ=see|název=Pražský hrad|zeměpisná šířka=50.0906|zeměpisná délka=14.4005}}",
            "see",
            "Pražský hrad",
            Some(50.0906),
        ),
        (
            "id",
            "{{Sleep|name=Raja Ampat dive resort|url=https://www.openstreetmap.org/node/5097319261|lat=-0.43933|long=130.74100|lastedit=2017-09-15}}",
            "sleep",
            "Raja Ampat dive resort",
            Some(-0.43933),
        ),
        (
            "hi",
            "{{See|name=पर्यटक कार्यालय|url=https://melillaturismo.com/|address=Plaza de las Culturas s/n|lat=35.293593261100156|long=-2.937322738536183}}",
            "see",
            "पर्यटक कार्यालय",
            Some(35.293593261100156),
        ),
        (
            "bn",
            "{{দেখুন\n| নাম=রামকৃষ্ণ মন্দির \n| ঠিকানা= | অক্ষাংশ=22.5843 | দ্রাঘিমাংশ=88.9225 \n| বিবরণ=\n}}",
            "see",
            "রামকৃষ্ণ মন্দির",
            Some(22.5843),
        ),
        (
            "bn",
            "{{রাত্রিযাপন করুন\n| নাম=হোটেল আইবিস ব্রাগা| ইউআরএল=https://all.accor.com/hotel/1802/index.en.shtml | অক্ষাংশ=41.5533 | দ্রাঘিমাংশ=-8.4259 | ফোন=+351 253 204 800\n}}",
            "sleep",
            "হোটেল আইবিস ব্রাগা",
            Some(41.5533),
        ),
    ];

    #[test]
    fn every_edition_maps_its_own_listing_templates() {
        let mut covered = std::collections::BTreeSet::new();
        for (code, text, ty, name, lat) in CASES {
            let ed = edition(code).unwrap();
            let ls = listings(ed, text);
            assert_eq!(ls.len(), 1, "{code}: {text}");
            let l = &ls[0];
            assert_eq!(l["type"], *ty, "{code}: {text}");
            assert_eq!(l["name"], *name, "{code}: {text}");
            assert_eq!(l["lat"].as_f64(), *lat, "{code}: {text}");
            assert!(l["lng"].is_number(), "{code}: {text}");
            covered.insert(*code);
        }
        // Every edition has a case, and every case reads its own edition's templates.
        let all: std::collections::BTreeSet<_> = EDITIONS.iter().map(|e| e.code).collect();
        assert_eq!(covered, all);
    }

    #[test]
    fn local_templates_belong_to_their_own_edition_only() {
        // `{{voir}}` is French; in English it is just some other template.
        let fr = "{{Voir|nom=Tour Eiffel|latitude=48.8584|longitude=2.2945}}";
        assert!(listings(edition("en").unwrap(), fr).is_empty());
        assert_eq!(listings(edition("fr").unwrap(), fr).len(), 1);
        // English templates work everywhere.
        let en = "{{eat|name=Café|lat=1.5|long=2.5}}";
        for e in EDITIONS {
            assert_eq!(listings(e, en).len(), 1, "{}", e.code);
        }
        // First letter any case, `_` = space.
        assert_eq!(
            edition("fr").unwrap().listing_template("Se_loger"),
            Some(Tpl::Fixed(ListingType::Sleep))
        );
        assert_eq!(
            edition("de").unwrap().listing_template("VCard"),
            Some(Tpl::Typed)
        );
    }

    #[test]
    fn listing_fields_are_cleaned_and_mapped_to_english_keys() {
        let ed = edition("pl").unwrap();
        let ls = listings(
            ed,
            "{{zobacz\n| name=[https://www.aalborgakvavit.dk/ Aalborg Akvavit] | adres=Ved Stranden 3 | telefon=+45 1234 | godziny=9–17 | ceny=50&nbsp;DKK | opis=Destylarnia [[wódka|wódki]] {{cytat}} &mdash; '''stara'''. | lat=57.05 | long=9.91\n}}",
        );
        let l = &ls[0];
        assert_eq!(
            l["url"], "https://www.aalborgakvavit.dk/",
            "name link is the website"
        );
        assert_eq!(l["address"], "Ved Stranden 3");
        assert_eq!(l["phone"], "+45 1234");
        assert_eq!(l["hours"], "9–17");
        assert_eq!(l["price"], "50 DKK");
        assert_eq!(l["description"], "Destylarnia wódki — stara.");
        // A generic listing with a city type is a destination, not a point of interest.
        let en = edition("en").unwrap();
        assert!(listings(en, "{{listing|type=city|name=Oslo|lat=59.9|long=10.7}}").is_empty());
        assert_eq!(
            listings(en, "{{listing|name=Kiosk}}")[0]["type"],
            "other",
            "no type: other"
        );
        // Bad coordinates are dropped, the listing kept.
        let l = &listings(en, "{{see|name=X|lat=95|long=abc}}")[0];
        assert!(l.get("lat").is_none() && l.get("lng").is_none());
        // No name, no listing.
        assert!(listings(en, "{{see|lat=1|long=2}}").is_empty());
    }

    #[test]
    fn nested_templates_and_links_do_not_split_parameters() {
        let t = parse_template(
            "{{see|name=A [[B|C]] D|hours={{Horaire|1|5|7|30}} daily|lat=1.5|long=2.5}}",
        )
        .unwrap();
        assert_eq!(t.named("name"), Some("A [[B|C]] D"));
        assert_eq!(t.named("hours"), Some("{{Horaire|1|5|7|30}} daily"));
        assert_eq!(t.params.len(), 4);
    }

    #[test]
    fn article_coordinates_per_edition() {
        let en = edition("en").unwrap();
        // The listing's coordinates are not the article's.
        let text = "{{see|name=X|lat=1|long=1}}\n{{geo|51.5|-0.12|zoom=10}}";
        assert_eq!(article_coords(en, text), Some((51.5, -0.12)));
        assert_eq!(article_coords(en, "{{see|name=X|lat=1|long=1}}"), None);
        assert_eq!(
            article_coords(en, "{{Mapframe|48.85|2.35|zoom=12}}"),
            Some((48.85, 2.35))
        );
        // German {{GeoData|lat=…|long=…}}, French banner latitude/longitude, Italian Lat/Long.
        let de = edition("de").unwrap();
        assert_eq!(
            article_coords(de, "{{GeoData|lat=52.52|long=13.405|radius=20}}"),
            Some((52.52, 13.405))
        );
        let fr = edition("fr").unwrap();
        assert_eq!(
            article_coords(
                fr,
                "{{Info Ville\n| nom local=Paris\n| latitude=48.8566\n| longitude=2.3522\n}}"
            ),
            Some((48.8566, 2.3522))
        );
        let it = edition("it").unwrap();
        assert_eq!(
            article_coords(it, "{{QuickbarCity|Stato=Italia|Lat=41.9|Long=12.5}}"),
            Some((41.9, 12.5))
        );
        // A page template of another edition does not count.
        assert_eq!(
            article_coords(en, "{{GeoData|lat=52.52|long=13.405}}"),
            None
        );
        // Polish {{mapa|lat|long}}, Bengali {{ভূ|lat|long}}.
        assert_eq!(
            article_coords(edition("pl").unwrap(), "{{Mapa|50.06|19.94|zoom=12}}"),
            Some((50.06, 19.94))
        );
        assert_eq!(
            article_coords(edition("bn").unwrap(), "{{ভূ|23.81|90.41}}"),
            Some((23.81, 90.41))
        );
    }

    #[test]
    fn edition_comes_from_the_dump_url() {
        let code = |u: &str| edition_of_url(u).code;
        assert_eq!(
            code(
                "https://dumps.wikimedia.org/dewikivoyage/latest/dewikivoyage-latest-pages-articles.xml.bz2"
            ),
            "de"
        );
        assert_eq!(
            code(
                "https://dumps.wikimedia.org/bnwikivoyage/latest/bnwikivoyage-latest-pages-articles.xml.bz2"
            ),
            "bn"
        );
        assert_eq!(
            code(
                "https://dumps.wikimedia.org/enwikivoyage/latest/enwikivoyage-latest-pages-articles.xml.bz2"
            ),
            "en"
        );
        // Unknown or no edition: English (the fixture URLs of the tests).
        assert_eq!(code("http://127.0.0.1:1234/wv.xml.bz2"), "en");
        assert_eq!(
            code("https://dumps.wikimedia.org/xxwikivoyage/latest/x.bz2"),
            "en"
        );
        assert_eq!(
            edition("de").unwrap().page_url("Köln Altstadt"),
            "https://de.wikivoyage.org/wiki/Köln_Altstadt"
        );
        assert_eq!(edition("de").unwrap().id_prefix(), "dewikivoyage");
        assert_eq!(EDITIONS.len(), 23);
    }
}
