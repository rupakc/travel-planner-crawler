//! `state_dept_country_info`: the US State Department's country travel information
//! (`cadataapi.state.gov/api/CountryTravelInformation`): one JSON array with every country's
//! travel.state.gov page as HTML fragments (entry and exit requirements, embassy contacts,
//! local laws, health, safety), plus a "Last Updated" date.
//!
//! **Only the listed endpoint is ever requested.** The host's home page lists its `/api/…`
//! paths; any other path answers a Cloudflare block page, so the adapter refuses a leased URL
//! whose path is not [`PATH`] before sending anything, and never fans out. One request per run
//! (conditional when the API sends validators); the whole list is one body (~6 MB).
//!
//! The `tag` of each country is a **FIPS 10-4** code (`AS` = Australia, `AU` = Austria), never
//! an ISO code: [`FIPS_ISO2`] maps it. What [`records`](super::records) derives (pure, from the
//! stored body):
//! - `embassy`: the US embassy block (name, address, phones as written, email, web) for US
//!   travellers in that country;
//! - `visa_rule` (official, US passport holders): only when the entry text states the
//!   requirement plainly ([`visa`]); contradictory or conditional statements give `other`, no
//!   statement gives no rule; the deciding sentences are kept as `evidence`;
//! - `tip`: each titled paragraph of the local-laws, health and safety sections.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json, not_modified};
use super::{countries, html_text};

/// Adapter id.
pub const ID: &str = "state_dept_country_info";

/// The one endpoint this adapter requests (all countries).
pub const PATH: &str = "/api/CountryTravelInformation";

/// The body cap (the list is ~6 MB).
pub const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;

/// FIPS 10-4 tags the API uses → ISO 3166-1 alpha-2. Tags without an ISO code (the French
/// West Indies cover several territories) are left out: those countries keep their name only.
pub const FIPS_ISO2: &[(&str, &str)] = &[
    ("A1", "BQ"),
    ("A2", "GF"),
    ("AC", "AG"),
    ("AE", "AE"),
    ("AF", "AF"),
    ("AG", "DZ"),
    ("AJ", "AZ"),
    ("AL", "AL"),
    ("AM", "AM"),
    ("AN", "AD"),
    ("AO", "AO"),
    ("AR", "AR"),
    ("AS", "AU"),
    ("AU", "AT"),
    ("AV", "AI"),
    ("AW", "AW"),
    ("AY", "AQ"),
    ("BA", "BH"),
    ("BB", "BB"),
    ("BC", "BW"),
    ("BD", "BM"),
    ("BE", "BE"),
    ("BF", "BS"),
    ("BG", "BD"),
    ("BH", "BZ"),
    ("BK", "BA"),
    ("BL", "BO"),
    ("BM", "MM"),
    ("BN", "BJ"),
    ("BO", "BY"),
    ("BP", "SB"),
    ("BR", "BR"),
    ("BT", "BT"),
    ("BU", "BG"),
    ("BX", "BN"),
    ("BY", "BI"),
    ("CA", "CA"),
    ("CB", "KH"),
    ("CD", "TD"),
    ("CE", "LK"),
    ("CF", "CG"),
    ("CG", "CD"),
    ("CH", "CN"),
    ("CI", "CL"),
    ("CJ", "KY"),
    ("CM", "CM"),
    ("CN", "KM"),
    ("CO", "CO"),
    ("CS", "CR"),
    ("CT", "CF"),
    ("CU", "CU"),
    ("CV", "CV"),
    ("CY", "CY"),
    ("DA", "DK"),
    ("DJ", "DJ"),
    ("DO", "DM"),
    ("DR", "DO"),
    ("EC", "EC"),
    ("EG", "EG"),
    ("EI", "IE"),
    ("EK", "GQ"),
    ("EN", "EE"),
    ("ER", "ER"),
    ("ES", "SV"),
    ("ET", "ET"),
    ("EZ", "CZ"),
    ("FI", "FI"),
    ("FJ", "FJ"),
    ("FM", "FM"),
    ("FP", "PF"),
    ("FR", "FR"),
    ("GA", "GM"),
    ("GB", "GA"),
    ("GG", "GE"),
    ("GH", "GH"),
    ("GJ", "GD"),
    ("GM", "DE"),
    ("GR", "GR"),
    ("GT", "GT"),
    ("GV", "GN"),
    ("GW", "GW"),
    ("GY", "GY"),
    ("HA", "HT"),
    ("HK", "HK"),
    ("HO", "HN"),
    ("HR", "HR"),
    ("HU", "HU"),
    ("IC", "IS"),
    ("ID", "ID"),
    ("IN", "IN"),
    ("IR", "IR"),
    ("IS", "IL"),
    ("IT", "IT"),
    ("IV", "CI"),
    ("IZ", "IQ"),
    ("JA", "JP"),
    ("JM", "JM"),
    ("JO", "JO"),
    ("KE", "KE"),
    ("KG", "KG"),
    ("KN", "KP"),
    ("KR", "KI"),
    ("KS", "KR"),
    ("KU", "KW"),
    ("KV", "XK"),
    ("KZ", "KZ"),
    ("LA", "LA"),
    ("LE", "LB"),
    ("LG", "LV"),
    ("LH", "LT"),
    ("LI", "LR"),
    ("LO", "SK"),
    ("LS", "LI"),
    ("LT", "LS"),
    ("LU", "LU"),
    ("LY", "LY"),
    ("MA", "MG"),
    ("MC", "MO"),
    ("MD", "MD"),
    ("MG", "MN"),
    ("MH", "MS"),
    ("MI", "MW"),
    ("MJ", "ME"),
    ("MK", "MK"),
    ("ML", "ML"),
    ("MN", "MC"),
    ("MO", "MA"),
    ("MP", "MU"),
    ("MR", "MR"),
    ("MT", "MT"),
    ("MU", "OM"),
    ("MV", "MV"),
    ("MX", "MX"),
    ("MY", "MY"),
    ("MZ", "MZ"),
    ("NC", "NC"),
    ("NG", "NE"),
    ("NH", "VU"),
    ("NI", "NG"),
    ("NL", "NL"),
    ("NN", "SX"),
    ("NO", "NO"),
    ("NP", "NP"),
    ("NR", "NR"),
    ("NS", "SR"),
    ("NU", "NI"),
    ("NZ", "NZ"),
    ("OD", "SS"),
    ("PA", "PY"),
    ("PE", "PE"),
    ("PK", "PK"),
    ("PL", "PL"),
    ("PM", "PA"),
    ("PO", "PT"),
    ("PP", "PG"),
    ("PS", "PW"),
    ("QA", "QA"),
    ("RI", "RS"),
    ("RM", "MH"),
    ("RO", "RO"),
    ("RP", "PH"),
    ("RS", "RU"),
    ("RW", "RW"),
    ("SA", "SA"),
    ("SC", "KN"),
    ("SE", "SC"),
    ("SF", "ZA"),
    ("SG", "SN"),
    ("SI", "SI"),
    ("SL", "SL"),
    ("SM", "SM"),
    ("SN", "SG"),
    ("SO", "SO"),
    ("SP", "ES"),
    ("SR", "CH"),
    ("ST", "LC"),
    ("SU", "SD"),
    ("SW", "SE"),
    ("SY", "SY"),
    ("TD", "TT"),
    ("TH", "TH"),
    ("TI", "TJ"),
    ("TK", "TC"),
    ("TN", "TO"),
    ("TO", "TG"),
    ("TP", "ST"),
    ("TS", "TN"),
    ("TT", "TL"),
    ("TU", "TR"),
    ("TV", "TV"),
    ("TW", "TW"),
    ("TX", "TM"),
    ("TZ", "TZ"),
    ("UC", "CW"),
    ("UG", "UG"),
    ("UK", "GB"),
    ("UP", "UA"),
    ("UV", "BF"),
    ("UY", "UY"),
    ("UZ", "UZ"),
    ("VC", "VC"),
    ("VE", "VE"),
    ("VI", "VG"),
    ("VM", "VN"),
    ("WA", "NA"),
    ("WS", "WS"),
    ("WZ", "SZ"),
    ("YM", "YE"),
    ("ZA", "ZM"),
    ("ZI", "ZW"),
];

/// The ISO 3166-1 alpha-2 code of a FIPS 10-4 tag, if it has one.
pub fn fips_iso2(tag: &str) -> Option<&'static str> {
    let t = tag.trim();
    FIPS_ISO2
        .iter()
        .find(|(f, _)| f.eq_ignore_ascii_case(t))
        .map(|(_, i)| *i)
}

/// `"SanMarino"` → `"San Marino"` (the API writes names without spaces).
pub fn spaced(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 8);
    let mut prev: Option<char> = None;
    for c in name.trim().chars() {
        if c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase()) {
            out.push(' ');
        }
        out.push(c);
        prev = Some(c);
    }
    out
}

#[derive(Deserialize)]
struct Raw {
    #[serde(default)]
    tag: String,
    #[serde(default)]
    geopoliticalarea: String,
    #[serde(default)]
    entry_exit_requirements: Option<String>,
    #[serde(default)]
    local_laws_and_special_circumstances: Option<String>,
    #[serde(default)]
    health: Option<String>,
    #[serde(default)]
    safety_and_security: Option<String>,
    #[serde(default, rename = "travel_embassyAndConsulate")]
    embassy: Option<String>,
    #[serde(default)]
    last_update_date: Option<String>,
}

/// One country's information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountryInfo {
    /// The API's FIPS 10-4 tag.
    pub tag: String,
    /// ISO 3166-1 alpha-2 (from the tag), if there is one.
    pub country_code: Option<&'static str>,
    /// English country name: the ISO short name, else the API's name with spaces restored.
    pub country: String,
    /// "Last Updated" as `YYYY-MM-DD`.
    pub updated: Option<String>,
    /// Entry and exit requirements (HTML).
    pub entry: String,
    /// Local laws and special circumstances (HTML).
    pub laws: String,
    /// Health (HTML).
    pub health: String,
    /// Safety and security (HTML).
    pub safety: String,
    /// The embassy block (HTML).
    pub embassy: String,
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// `"Last Updated: August 11, 2025"` → `2025-08-11`.
pub fn updated_date(s: &str) -> Option<String> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)([a-z]+)\s+(\d{1,2}),?\s+(\d{4})").expect("regex"));
    let c = RE.captures(s)?;
    let m = MONTHS.iter().position(|m| m.eq_ignore_ascii_case(&c[1]))? + 1;
    let d: u32 = c[2].parse().ok()?;
    let y: u32 = c[3].parse().ok()?;
    ((1..=31).contains(&d) && (1900..=2999).contains(&y)).then(|| format!("{y:04}-{m:02}-{d:02}"))
}

/// Parses the array.
pub fn parse(body: &[u8]) -> Result<Vec<CountryInfo>> {
    let raw: Vec<Raw> = json(body)?;
    if raw.is_empty() {
        return Err(Error::Invalid("state dept country info: empty list".into()));
    }
    Ok(raw
        .into_iter()
        .filter(|r| !r.tag.trim().is_empty() && !r.geopoliticalarea.trim().is_empty())
        .map(|r| {
            let code = fips_iso2(&r.tag);
            let country = code
                .and_then(countries::name)
                .map(str::to_string)
                .unwrap_or_else(|| spaced(&r.geopoliticalarea));
            CountryInfo {
                tag: r.tag.trim().to_string(),
                country_code: code,
                country,
                updated: r.last_update_date.as_deref().and_then(updated_date),
                entry: r.entry_exit_requirements.unwrap_or_default(),
                laws: r.local_laws_and_special_circumstances.unwrap_or_default(),
                health: r.health.unwrap_or_default(),
                safety: r.safety_and_security.unwrap_or_default(),
                embassy: r.embassy.unwrap_or_default(),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------------------------
// The embassy block
// ---------------------------------------------------------------------------------------------

/// The labelled fields of the embassy block, as written (text).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbassyBlock {
    /// `Embassy Name`.
    pub name: String,
    /// Notes written after the name (e.g. "OPERATIONS SUSPENDED - AUGUST 31, 2021").
    pub notes: Vec<String>,
    /// `Street Address` (lines joined with ", ").
    pub address: Option<String>,
    /// `Phone`.
    pub phone: Option<String>,
    /// `Emergency Phone`.
    pub emergency_phone: Option<String>,
    /// `Fax`.
    pub fax: Option<String>,
    /// `Email`.
    pub email: Option<String>,
    /// `Web`.
    pub web: Option<String>,
}

/// Reads the `<b>Label:</b> value <br/>` block.
pub fn embassy_block(html: &str) -> Option<EmbassyBlock> {
    static LABEL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<b>\s*([^<:]{2,40}?)\s*:\s*</b>").expect("regex"));
    static NOTE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<p\b[^>]*>(.*?)</p>").expect("regex"));
    static FIELD_END: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<br\s*/>\s*$").expect("regex"));
    let labels: Vec<_> = LABEL.captures_iter(html).collect();
    let mut b = EmbassyBlock::default();
    for (i, c) in labels.iter().enumerate() {
        let start = c.get(0).map_or(0, |m| m.end());
        let end = labels
            .get(i + 1)
            .and_then(|n| n.get(0))
            .map_or(html.len(), |m| m.start());
        let raw = FIELD_END.replace(html[start..end].trim(), "");
        let label = c[1].trim().to_lowercase();
        if label == "embassy name" {
            let notes: Vec<String> = NOTE
                .captures_iter(&raw)
                .map(|n| html_text::one_line(&n[1]))
                .filter(|n| !n.is_empty())
                .collect();
            let name = NOTE.replace_all(&raw, " ");
            b.name = html_text::one_line(&name);
            b.notes = notes;
            continue;
        }
        let mut value = String::new();
        for l in html_text::text(&raw)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
        {
            if !value.is_empty() {
                value.push_str(if value.ends_with(',') { " " } else { ", " });
            }
            value.push_str(l);
        }
        let value = (!value.is_empty()).then_some(value);
        match label.as_str() {
            "street address" | "address" => b.address = value,
            "phone" | "telephone" => b.phone = value,
            "emergency phone" | "emergency telephone" | "after hours phone" => {
                b.emergency_phone = value
            }
            "fax" => b.fax = value,
            "email" | "e-mail" => b.email = value,
            "web" | "website" => b.web = value,
            _ => {}
        }
    }
    (!b.name.is_empty()).then_some(b)
}

// ---------------------------------------------------------------------------------------------
// Entry requirements → one visa rule for US passport holders
// ---------------------------------------------------------------------------------------------

/// What the entry text states for a US tourist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisaFinding {
    /// `visa_rule.visa_type`.
    pub visa_type: &'static str,
    /// Stated maximum stay, when the deciding sentences give exactly one number of days.
    pub max_stay_days: Option<u32>,
    /// `tourism` when the deciding sentences speak of tourists or visitors, else `any`.
    pub purpose: &'static str,
    /// Required passport validity in months, when the text states exactly one.
    pub passport_validity_months: Option<u32>,
    /// The deciding sentences (≤ 3), as written.
    pub evidence: Vec<String>,
}

const ABBREV: &[(&str, &str)] = &[
    ("U.S.", "U\u{1}S\u{1}"),
    ("e.g.", "e\u{1}g\u{1}"),
    ("i.e.", "i\u{1}e\u{1}"),
    ("a.m.", "a\u{1}m\u{1}"),
    ("p.m.", "p\u{1}m\u{1}"),
    ("St.", "St\u{1}"),
    ("No.", "No\u{1}"),
];

/// The sentences of an HTML fragment, as written (abbreviations such as "U.S." do not end one).
pub fn sentences(html: &str) -> Vec<String> {
    static END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.!?](?:\s+|$)").expect("regex"));
    let mut out = Vec::new();
    for line in html_text::text(html).lines() {
        let mut l = line.to_string();
        for (a, p) in ABBREV {
            l = l.replace(a, p);
        }
        let mut start = 0;
        for m in END.find_iter(&l) {
            out.push(l[start..m.start() + 1].to_string());
            start = m.end();
        }
        if start < l.len() {
            out.push(l[start..].to_string());
        }
    }
    out.into_iter()
        .map(|s| {
            let mut s = s.trim().to_string();
            for (a, p) in ABBREV {
                s = s.replace(p, a);
            }
            s
        })
        .filter(|s| !s.is_empty())
        .collect()
}

macro_rules! re {
    ($name:ident, $pat:expr) => {
        static $name: LazyLock<Regex> = LazyLock::new(|| Regex::new($pat).expect("regex"));
    };
}

re!(
    FREE,
    r#"(?i)visa[- ]free|“visa free”|"visa free"|without (?:a|obtaining a|needing a) visa|(?:do|does|will) not (?:need|require) (?:a |an )?(?:\w+ )?visa|(?:is|are) not required to (?:have|obtain|get) (?:a |an )?(?:\w+ )?visa|visas? (?:is |are )?not (?:required|needed)|no visa (?:is )?(?:required|needed)|exempt(?:ed)? from (?:the )?visa|visa exemption"#
);
re!(
    STAY_CONTEXT,
    r"(?i)\b\d{1,3}[- ]days?\b|\btouris|\bbusiness\b|\bvisit(?:or|s|ing)?\b|\bstays?\b"
);
re!(
    EVISA,
    r"(?i)\be-?visas?\b|\belectronic visas?\b|\bon-?line visas?\b|\bvisas? online\b"
);
re!(
    ETA,
    r"\b(?:eTA|ETA|ETIAS)\b|(?i:electronic travel authori[sz]ation)"
);
re!(
    VOA,
    r"(?i)visas? (?:up)?on arrival|visas? (?:is |are |can be |may be )?(?:available|issued|granted|obtainable|obtained|purchased) (?:at [\w ]{1,40} )?(?:up)?on arrival|(?:obtain|get|receive|purchase|buy|apply for) (?:a |an )?(?:[\w-]+ ){0,4}visas? (?:up)?on arrival|(?:issued|available) (?:up)?on arrival"
);
re!(
    REQUIRED,
    r"(?i)visas? (?:is|are) required|requires? (?:a |an )?(?:valid )?(?:\w+ )?visa\b|\b(?:must|need|needs|will need|have to|required to)\b[^.]{0,60}?\bvisas?\b"
);
re!(
    ENTRY_CONTEXT,
    r"(?i)\bto enter\b|\bfor entry\b|\bentry visa\b|\bupon entry\b|\bbefore (?:you )?(?:travel|arriv|depart|leav)|\bprior to (?:travel|arriv|entry|departure|initiating)|\btouris|\bvisitors?\b|\bare required for all\b|\bto visit\b"
);
re!(
    NEGATED,
    r"(?i)\b(?:cannot|can ?not|not|no longer|unavailable|never)\b"
);
re!(
    VOA_NEGATED,
    r"(?i)\bno (?:visa )?issuance\b|\bno visas? (?:are |is )?(?:issued|available)\b"
);
re!(
    REQ_NEGATED,
    r"(?i)\b(?:do|does|will|are|is|was) not (?:need|require)|\bdon'?t (?:need|require)|\bnot required\b|\bno visa (?:is )?(?:required|needed)\b|\bcannot\b|\bnot (?:be )?(?:able|eligible)\b"
);
// A visa-free statement that is itself denied ("will not be able to … without a visa").
re!(
    REQ_NEGATED_FREE,
    r"(?i)\bnot (?:be )?(?:able|eligible|permitted|allowed)\b|\bcannot\b|\bno longer\b"
);
re!(VISA_PAGES, r"(?i)\bvisa pages?\b|\bblank pages?\b");
re!(
    ONLY_IF,
    r"(?i)\bonly (?:need|require)s? (?:a |an )?visa if\b|\bneed (?:a |an )?visa only if\b"
);
re!(
    CONDITIONAL,
    r"(?i)\bif you (?:plan|intend|wish|want)\b|\blonger than\b|\bmore than \d+ days\b|\bbeyond\b|\bother than\b|\ball other purposes\b|\bother purposes\b|\bemploy|\bwork\b|\bstud(?:y|ent)|\bresiden|\bborn in\b|\bdual nationals?\b|\bnationals of\b|\btemporary passport|\bdiplomatic|\bofficial passport|\bmilitary\b|\bSOFA\b|\bIsrael|\bjournalist|\bmissionar|\bvolunteer|\bextend|\bextension\b|\boverstay|\bif you (?:require|need|anticipate)\b|\bmay qualify\b|\bfor all other\b|\bother travel\b|\bre-?enter|\brefugee|\bemergency passport|\btravel document\b|\btransit|\bmainland\b|\bfrom (?:Hong Kong|Macau)\b|\bunder (?:the age of )?1[0-9]\b|\bolder than\b|\bminors?\b"
);
// A stated length of stay: "up to 90 days", "for 30 days", "less than 90 days", "a 30-day
// visa", "90 days or fewer" (not "within a 180-day period", "7 and 90 days before").
re!(
    DAYS,
    r"(?i)(?:\b(?:up to|for|of|less than|fewer than|under|maximum of|valid for)\s+(?:a\s+)?(\d{1,3})[- ](?:calendar[- ])?days?\b)|(?:\b(\d{1,3})[- ]days?(?:'s)? (?:[a-z]+ )?(?:stay|visa|visit|or (?:less|fewer))\b)"
);
re!(
    VALIDITY,
    r"(?i)passport[^.]{0,80}?\bvalid(?:ity)?\b[^.]{0,40}?\b(?:at least |for |of )?(one|two|three|six|1|2|3|6)[- ]months?\b|\b(one|two|three|six|1|2|3|6)[- ]months?'?s? (?:of )?(?:passport )?validity"
);
re!(TOURISM, r"(?i)\btouris|\bvisitors?\b|\bholiday|\bvacation");

fn months(w: &str) -> Option<u32> {
    match w.to_ascii_lowercase().as_str() {
        "one" | "1" => Some(1),
        "two" | "2" => Some(2),
        "three" | "3" => Some(3),
        "six" | "6" => Some(6),
        _ => None,
    }
}

fn cut_quote(s: &str) -> String {
    if s.chars().count() <= 300 {
        return s.to_string();
    }
    let mut t: String = s.chars().take(299).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

/// Classifies the entry requirements for a US passport holder visiting as a tourist.
/// `None` when no sentence states a requirement for that case.
pub fn visa(entry_html: &str) -> Option<VisaFinding> {
    let all = sentences(entry_html);
    let mut free = Vec::new();
    let mut evisa = Vec::new();
    let mut eta = Vec::new();
    let mut voa = Vec::new();
    let mut required = Vec::new();
    // "You only need a visa if: you stay more than 90 days…": every "must get a visa" that
    // follows is one of those conditions.
    let only_if = all
        .iter()
        .any(|s| ONLY_IF.is_match(s) && !CONDITIONAL.is_match(s));
    for s in &all {
        if CONDITIONAL.is_match(s) {
            continue;
        }
        if ONLY_IF.is_match(s) {
            free.push(s.clone());
            continue;
        }
        let negated = NEGATED.is_match(s);
        if FREE.is_match(s) && STAY_CONTEXT.is_match(s) && !REQ_NEGATED_FREE.is_match(s) {
            free.push(s.clone());
        } else if ETA.is_match(s) && !negated {
            eta.push(s.clone());
        } else if EVISA.is_match(s)
            && !negated
            && (REQUIRED.is_match(s) || ENTRY_CONTEXT.is_match(s))
        {
            evisa.push(s.clone());
        } else if VOA.is_match(s) && !negated && !VOA_NEGATED.is_match(s) {
            voa.push(s.clone());
        } else if REQUIRED.is_match(s)
            && !only_if
            && !REQ_NEGATED.is_match(s)
            && !VISA_PAGES.is_match(s)
            && ENTRY_CONTEXT.is_match(s)
        {
            required.push(s.clone());
        }
    }
    let (visa_type, deciding): (&'static str, Vec<String>) = match (
        !free.is_empty(),
        !eta.is_empty(),
        !evisa.is_empty(),
        !voa.is_empty(),
        !required.is_empty(),
    ) {
        (true, false, false, false, false) => ("visa_free", free),
        (true, true, false, false, false) => ("eta", [eta, free].concat()),
        (false, true, false, false, _) => ("eta", eta),
        (false, false, true, false, _) => ("e_visa", [evisa, required].concat()),
        (false, false, false, true, _) => ("visa_on_arrival", [voa, required].concat()),
        (false, false, false, false, true) => ("visa_required", required),
        (false, false, false, false, false) => return None,
        _ => ("other", [free, eta, evisa, voa, required].concat()),
    };
    let max_stay_days = if visa_type == "visa_required" || visa_type == "other" {
        None
    } else {
        let days: BTreeSet<u32> = deciding
            .iter()
            .flat_map(|s| {
                DAYS.captures_iter(s)
                    .filter_map(|c| c.get(1).or_else(|| c.get(2))?.as_str().parse().ok())
            })
            .filter(|d| (1..=365).contains(d))
            .collect();
        (days.len() == 1).then(|| *days.iter().next().unwrap_or(&0))
    };
    let validity: BTreeSet<u32> = all
        .iter()
        .flat_map(|s| {
            VALIDITY.captures_iter(s).filter_map(|c| {
                c.get(1)
                    .or_else(|| c.get(2))
                    .and_then(|m| months(m.as_str()))
            })
        })
        .collect();
    let purpose = if deciding.iter().any(|s| TOURISM.is_match(s)) {
        "tourism"
    } else {
        "any"
    };
    let mut evidence: Vec<String> = Vec::new();
    for s in &deciding {
        let q = cut_quote(s);
        if !evidence.contains(&q) {
            evidence.push(q);
        }
        if evidence.len() == 3 {
            break;
        }
    }
    Some(VisaFinding {
        visa_type,
        max_stay_days,
        purpose,
        passport_validity_months: (validity.len() == 1)
            .then(|| *validity.iter().next().unwrap_or(&0)),
        evidence,
    })
}

// ---------------------------------------------------------------------------------------------
// Titled paragraphs → tips
// ---------------------------------------------------------------------------------------------

/// One titled paragraph of a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipText {
    /// `tip.category`.
    pub category: &'static str,
    /// `tip.severity`.
    pub severity: &'static str,
    /// The paragraph's title as written.
    pub title: String,
    /// Its text (≤ 500 characters, cut at a sentence end where possible).
    pub body: String,
}

/// Which section a tip comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Local laws and special circumstances.
    Laws,
    /// Health.
    Health,
    /// Safety and security.
    Safety,
}

/// Titles that only introduce link lists or emergency numbers (left to other kinds).
const SKIP_TITLES: &[&str] = &[
    "resources",
    "emergency phone",
    "emergency number",
    "emergency services",
    "helpful",
    "for emergency",
    "dial",
];

fn tip_category(section: Section, title: &str) -> (&'static str, &'static str) {
    let t = title.to_lowercase();
    let has = |w: &[&str]| w.iter().any(|x| t.contains(x));
    if has(&["scam", "fraud"]) {
        return ("scam", "warning");
    }
    if has(&[
        "drug",
        "criminal",
        "penalt",
        "arrest",
        "prison",
        "detention",
        "prohibit",
        "illegal",
        "weapon",
        "firearm",
        "terror",
        "kidnap",
    ]) {
        return ("safety", "warning");
    }
    match section {
        Section::Health => ("health", "info"),
        Section::Safety => ("safety", "warning"),
        Section::Laws => {
            if has(&["currency", "money", "atm", "credit card", "banking", "cash"]) {
                ("money", "info")
            } else if has(&[
                "lgbt", "women", "faith", "religio", "dress", "culture", "custom",
            ]) {
                ("culture", "info")
            } else if has(&["traffic", "road", "driv", "taxi", "transport"]) {
                ("transport", "info")
            } else if has(&["phone", "internet", "cell", "mobile"]) {
                ("connectivity", "info")
            } else {
                ("other", "info")
            }
        }
    }
}

fn cut_body(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    if let Some(i) = head.rfind(". ").filter(|i| *i >= max / 2) {
        return head[..=i].trim_end().to_string();
    }
    let mut t = head.trim_end().to_string();
    t.push('…');
    t
}

/// The titled paragraphs (`<b>Title</b> text…`) of one section.
pub fn tips(section: Section, html: &str) -> Vec<TipText> {
    static TITLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?is)<p>\s*(?:<p>\s*)?<b>(.*?)</b>").expect("regex"));
    let marks: Vec<_> = TITLE.captures_iter(html).collect();
    let mut out = Vec::new();
    for (i, c) in marks.iter().enumerate() {
        let title = html_text::one_line(&c[1]);
        let title = title.trim().trim_end_matches(':').trim().to_string();
        let tl = title.to_lowercase();
        if title.chars().count() < 3
            || title.chars().count() > 120
            || !title.chars().any(char::is_alphabetic)
            || SKIP_TITLES.iter().any(|s| tl.contains(s))
        {
            continue;
        }
        let start = c.get(0).map_or(0, |m| m.end());
        let end = marks
            .get(i + 1)
            .and_then(|n| n.get(0))
            .map_or(html.len(), |m| m.start());
        let body = html_text::one_line(&html[start..end]);
        let body = body.trim_start_matches(':').trim().to_string();
        if body.chars().count() < 40 {
            continue;
        }
        let (category, severity) = tip_category(section, &title);
        out.push(TipText {
            category,
            severity,
            title,
            body: cut_body(&body, 500),
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The adapter
// ---------------------------------------------------------------------------------------------

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct StateDeptCountryInfo;

#[async_trait]
impl SourceAdapter for StateDeptCountryInfo {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        if url.as_url().path() != PATH || url.as_url().query().is_some() {
            // Only the endpoint the API lists: any other path on this host is a bot wall.
            return Err(Error::Invalid(format!(
                "{ID}: only {PATH} may be requested"
            )));
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            conditional: true,
            max_body_bytes: MAX_BODY_BYTES,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        if !not_modified(&ex) {
            check_status(&ex)?;
            parse(&ex.body)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fips_tags_map_to_known_iso_codes() {
        let mut seen = std::collections::HashSet::new();
        for (f, i) in FIPS_ISO2 {
            assert!(seen.insert(*f), "duplicate tag {f}");
            assert!(countries::name(i).is_some(), "{f} → unknown {i}");
        }
        assert_eq!(fips_iso2("AS"), Some("AU"), "FIPS AS is Australia");
        assert_eq!(fips_iso2("AU"), Some("AT"), "FIPS AU is Austria");
        assert_eq!(fips_iso2("UK"), Some("GB"));
        assert_eq!(fips_iso2("A3"), None);
    }

    #[test]
    fn names_and_dates() {
        assert_eq!(spaced("SanMarino"), "San Marino");
        assert_eq!(spaced("FrenchWestIndies"), "French West Indies");
        assert_eq!(
            updated_date("Last Updated: August 11, 2025").as_deref(),
            Some("2025-08-11")
        );
        assert_eq!(updated_date("Last Updated: "), None);
    }

    #[test]
    fn sentences_keep_abbreviations() {
        assert_eq!(
            sentences("<p>U.S. citizens may enter visa-free. Stay 90 days.</p>"),
            ["U.S. citizens may enter visa-free.", "Stay 90 days."]
        );
    }

    #[test]
    fn visa_statements_classify_conservatively() {
        let f = |h: &str| visa(h).map(|v| (v.visa_type, v.max_stay_days, v.purpose));
        assert_eq!(
            f(
                "<p>U.S. citizens may enter Slovakia for up to 90 days for tourist or business purposes without a visa.</p>"
            ),
            Some(("visa_free", Some(90), "tourism"))
        );
        assert_eq!(
            f(
                "<p>U.S. citizens must have a valid passport and Afghan visa to enter and exit Afghanistan.</p>"
            ),
            Some(("visa_required", None, "any"))
        );
        assert_eq!(
            f("<p>A no-cost visitor visa valid for 30 days is issued upon arrival.</p>"),
            Some(("visa_on_arrival", Some(30), "tourism"))
        );
        assert_eq!(
            f(
                "<p>You must apply and obtain an e-Visa through the Iraq e-Visa Portal prior to arrival.</p>"
            ),
            Some(("e_visa", None, "any"))
        );
        // Conditional statements (longer stays, study, dual nationals) decide nothing.
        assert_eq!(
            f(
                "<p>If you plan to stay longer than 90 days, you must obtain a visa before you travel.</p>"
            ),
            None
        );
        // A negated arrival visa is not a visa on arrival.
        assert_eq!(
            f("<p>Visas cannot be obtained upon arrival in Botswana.</p>"),
            None
        );
        // Contradictory plain statements → other.
        assert_eq!(
            f(
                "<p>You must have a visa to enter. Tourists may also obtain a visa on arrival.</p><p>Visit visa-free for up to 30 days.</p>"
            ),
            Some(("other", None, "tourism"))
        );
        // "Visit the embassy website for the most current visa information" states nothing.
        assert_eq!(
            f("<p>Visit the Embassy of Estonia website for the most current visa information.</p>"),
            None
        );
    }

    #[test]
    fn passport_validity_is_read_only_when_unambiguous() {
        let v = visa("<p>U.S. citizens may enter visa-free for up to 30 days. Your passport must be valid for at least six months.</p>").unwrap();
        assert_eq!(v.passport_validity_months, Some(6));
        let v = visa("<p>U.S. citizens may enter visa-free for up to 30 days. Your passport must be valid for at least six months. Minors need three months validity.</p>").unwrap();
        assert_eq!(v.passport_validity_months, None);
    }

    #[test]
    fn embassy_block_fields() {
        let h = "<b>Embassy Name:</b> U.S. Embassy Kabul <p>OPERATIONS SUSPENDED - AUGUST 31, 2021</p> \r\n <br/>\r\n <b>Street Address:</b>  1-10-5 Akasaka, Minato-ku,<br>Tokyo 107-8420 Japan\r\n <br/>\r\n <b>Phone:</b>  81-3-3224-5000\r\n <br/>\r\n <b>Emergency Phone:</b>  \r\n <br/>\r\n <b>Email:</b>  TokyoACS@state.gov\r\n <br/>\r\n <b>Web:</b>  https://jp.usembassy.gov/";
        let b = embassy_block(h).unwrap();
        assert_eq!(b.name, "U.S. Embassy Kabul");
        assert_eq!(b.notes, ["OPERATIONS SUSPENDED - AUGUST 31, 2021"]);
        assert_eq!(
            b.address.as_deref(),
            Some("1-10-5 Akasaka, Minato-ku, Tokyo 107-8420 Japan")
        );
        assert_eq!(b.phone.as_deref(), Some("81-3-3224-5000"));
        assert_eq!(b.emergency_phone, None);
        assert_eq!(b.email.as_deref(), Some("TokyoACS@state.gov"));
        assert_eq!(b.web.as_deref(), Some("https://jp.usembassy.gov/"));
    }

    #[test]
    fn titled_paragraphs_become_tips() {
        let h = "<p><p><b>Health Resources</b></p><ul><li>CDC</li></ul><p><b>Criminal Penalties:</b>&nbsp;U.S. citizens are subject to local laws and may be arrested for things legal at home.</p><p><b>Ambulance services<br></b>Ambulance services are widely available, but hospitals may refuse patients.</p>";
        let t = tips(Section::Laws, h);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[0].title, "Criminal Penalties");
        assert_eq!((t[0].category, t[0].severity), ("safety", "warning"));
        assert!(t[0].body.starts_with("U.S. citizens are subject"));
        assert_eq!(t[1].title, "Ambulance services");
        let h = tips(Section::Health, h);
        assert_eq!((h[1].category, h[1].severity), ("health", "info"));
    }
}
