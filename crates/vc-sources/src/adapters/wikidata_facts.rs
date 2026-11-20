//! `wikidata_facts`: place facts from Wikidata (CC0), with no LLM: typed statements turned
//! into short sentences of our own (`place_fact`, `confidence: structured`).
//!
//! Two routes (the leased row says which):
//! - **EntityData** (`/wiki/Special:EntityData/Q<n>.json`, robots: `Allow:
//!   /wiki/Special:EntityData/*.`): the item is fetched, then up to [`MAX_REFERENCED`] items it
//!   points to (its current country, events, highest/lowest points, namesakes, twin towns,
//!   neighbours, heritage designations), for their labels only. Every response is spooled raw
//!   as a `subresource` capture; then one **bundle** (`resource`, JSON) holding the slimmed item
//!   and the labels is the leased row's primary capture, and that bundle is what
//!   [`facts`] maps. (`/w/api.php` and `query.wikidata.org/sparql` are robots-disallowed, and
//!   `Special:ItemByTitle` is too; none is used.)
//! - **Dump** (`…/wikidatawiki/entities/…json.gz|bz2`, the existing `wikidata-dump` source,
//!   off because the full dump is too large): streamed, one entity per line; items with a
//!   GeoNames id (P1566) or an ISO 3166-1 code (P297) are slimmed on the fly (filter
//!   [`FILTER_VERSION`]). A dump carries no labels of the items an entity points to, so facts
//!   that need one (twin towns, events, …) are left out on this route.
//!
//! Properties ([`PROPERTIES`]) and their fact types: P571 inception, P576 dissolved, P793 +
//! P585 significant events, P1448 official names with an end date and P1435 heritage
//! designations → `historical`; P1448 current official name, P1449 nicknames, P1451 motto,
//! P1082 + P585 population, P2044 elevation, P2046 area, P757 World Heritage → `popular`;
//! P610 / P1589 highest and lowest points (+ P2044) → `unbelievable`; P138 named after, P190
//! twin towns, P47 borders → `little_known`. Deprecated statements are ignored; a preferred
//! one wins over normal ones. The place is keyed by its GeoNames id (P1566), its QID and its
//! country (P297, or the current P17's P297).

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Map, Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, append_resource_with_status, check_status, fetch_raw, on_origin};
use super::dump::{DumpFilter, DumpSettings, OutputKind, Split, Unit, run_dump};
use super::place_facts::{Fact, TextOrigin, and_list, date_phrase, number, thousands};

/// Adapter id.
pub const ID: &str = "wikidata_facts";
/// Dump filter version (recorded in the manifest; bump on any change).
pub const FILTER_VERSION: &str = "wikidata-facts-v1";
/// The `format` of a bundle (the primary capture of an EntityData row).
pub const BUNDLE_FORMAT: &str = "voyager-wikidata-facts-bundle-v1";
/// The fact properties.
pub const PROPERTIES: &[&str] = &[
    "P571", "P576", "P793", "P1448", "P1449", "P1451", "P610", "P1589", "P2044", "P2046", "P1082",
    "P138", "P190", "P47", "P1435", "P757",
];
/// Kept besides [`PROPERTIES`]: the GeoNames id, the country and the ISO codes.
const KEY_PROPERTIES: &[&str] = &["P1566", "P17", "P297", "P300"];
/// Most items fetched for labels per EntityData row.
pub const MAX_REFERENCED: usize = 12;
/// Cap on one EntityData response (big items, e.g. countries, are a few MB).
const MAX_ENTITY_BYTES: u64 = 16 * 1024 * 1024;
/// Most values of one multi-valued property listed in one sentence.
const MAX_LISTED: usize = 5;
/// Most events (P793) per place.
const MAX_EVENTS: usize = 5;

const SOURCE: (&str, &str) = ("Wikidata", "Wikidata contributors (Wikimedia Foundation)");

/// The QID of an EntityData URL (`/wiki/Special:EntityData/Q64.json`).
pub fn entity_data_qid(url: &CanonicalUrl) -> Option<String> {
    let p = url.as_url().path();
    let rest = p
        .strip_prefix("/wiki/Special:EntityData/")
        .or_else(|| p.strip_prefix("/wiki/Special%3AEntityData/"))?;
    let q = rest.strip_suffix(".json")?;
    is_qid(q).then(|| q.to_string())
}

fn is_qid(q: &str) -> bool {
    q.len() >= 2
        && q.len() <= 16
        && q.starts_with('Q')
        && q[1..].bytes().all(|b| b.is_ascii_digit())
        && !q[1..].starts_with('0')
}

fn is_dump(url: &CanonicalUrl) -> bool {
    let p = url.as_url().path();
    p.contains("/entities/") && (p.ends_with(".json.gz") || p.ends_with(".json.bz2"))
}

// ---------------------------------------------------------------------------------------------
// Slimming (dump lines and EntityData bodies alike)

/// The languages whose labels are kept: the sentences are English, so only English labels
/// and Wikidata's language-independent `mul` labels (names that are the same in every
/// language written in Latin script).
const LABEL_LANGS: &[&str] = &["en", "mul"];

fn slim_statement(st: &Value) -> Option<Value> {
    let rank = st.get("rank").and_then(Value::as_str).unwrap_or("normal");
    if rank == "deprecated" {
        return None;
    }
    let ms = st.get("mainsnak")?;
    if ms.get("snaktype").and_then(Value::as_str) != Some("value") {
        return None;
    }
    let value = ms.get("datavalue")?.get("value")?.clone();
    let mut q = Map::new();
    if let Some(qs) = st.get("qualifiers").and_then(Value::as_object) {
        for p in ["P585", "P580", "P582", "P2044"] {
            let vals: Vec<Value> = qs
                .get(p)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.get("datavalue")?.get("value").cloned())
                        .collect()
                })
                .unwrap_or_default();
            if !vals.is_empty() {
                q.insert(p.to_string(), Value::Array(vals));
            }
        }
    }
    let refs = st
        .get("references")
        .and_then(Value::as_array)
        .is_some_and(|r| !r.is_empty());
    Some(json!({"rank": rank, "value": value, "q": q, "refs": refs}))
}

/// The slim form of one entity (dump line or EntityData item): its id, `modified`, labels in
/// [`LABEL_LANGS`] and the fact and key
/// properties. `None` when it is not a place (no P1566 and no P297) or not an item.
pub fn slim(entity: &Value) -> Option<Value> {
    let id = entity.get("id").and_then(Value::as_str)?;
    if !is_qid(id) {
        return None;
    }
    let claims = entity.get("claims").and_then(Value::as_object)?;
    if !claims.contains_key("P1566") && !claims.contains_key("P297") {
        return None;
    }
    Some(slim_any(entity, id, claims, true))
}

fn slim_any(entity: &Value, id: &str, claims: &Map<String, Value>, full: bool) -> Value {
    let mut labels = Map::new();
    if let Some(l) = entity.get("labels").and_then(Value::as_object) {
        for lang in LABEL_LANGS.iter().copied() {
            if let Some(v) = l
                .get(lang)
                .and_then(|x| x.get("value"))
                .and_then(Value::as_str)
            {
                labels.insert(lang.to_string(), json!(v));
            }
        }
    }
    let props: &[&str] = if full { PROPERTIES } else { &[] };
    let mut kept = Map::new();
    for p in props.iter().chain(KEY_PROPERTIES) {
        if let Some(a) = claims.get(*p).and_then(Value::as_array) {
            let v: Vec<Value> = a.iter().filter_map(slim_statement).collect();
            if !v.is_empty() {
                kept.insert((*p).to_string(), Value::Array(v));
            }
        }
    }
    let mut out = json!({"id": id, "labels": labels, "claims": kept});
    if full && let Some(m) = entity.get("modified").and_then(Value::as_str) {
        out["modified"] = json!(m);
    }
    out
}

/// The slim entity of an EntityData body (`{"entities": {"Q64": {…}}}`; the first item when
/// the id redirected).
pub fn entity_of(body: &[u8], qid: &str) -> Result<Option<Value>> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("wikidata: {e}")))?;
    let ents = v
        .get("entities")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::Invalid("wikidata: no entities".into()))?;
    let e = ents.get(qid).or_else(|| ents.values().next());
    Ok(e.and_then(slim))
}

/// What the bundle keeps of a referenced item: its labels, and its ISO 3166-1 code and
/// elevation when it has them.
fn label_entry(body: &[u8], qid: &str) -> Result<Option<Value>> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("wikidata: {e}")))?;
    let Some(e) = v
        .get("entities")
        .and_then(Value::as_object)
        .and_then(|m| m.get(qid).or_else(|| m.values().next()))
    else {
        return Ok(None);
    };
    let Some(claims) = e.get("claims").and_then(Value::as_object) else {
        return Ok(None);
    };
    let s = slim_any(e, qid, claims, false);
    let mut out = json!({"labels": s["labels"]});
    if let Some(iso) = best(&s, "P297").and_then(|st| st["value"].as_str().map(str::to_string)) {
        out["iso2"] = json!(iso);
    }
    Ok(Some(out))
}

// ---------------------------------------------------------------------------------------------
// Facts

fn statements<'a>(e: &'a Value, p: &str) -> Vec<&'a Value> {
    let all: Vec<&Value> = e["claims"][p]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    if all.iter().any(|s| s["rank"] == "preferred") {
        all.into_iter()
            .filter(|s| s["rank"] == "preferred")
            .collect()
    } else {
        all
    }
}

/// Every non-deprecated statement of a multi-valued property, preferred ones first.
fn all_statements<'a>(e: &'a Value, p: &str) -> Vec<&'a Value> {
    let mut all: Vec<&Value> = e["claims"][p]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    all.sort_by_key(|s| s["rank"] != "preferred");
    all
}

fn best<'a>(e: &'a Value, p: &str) -> Option<&'a Value> {
    statements(e, p).into_iter().next()
}

fn item_id(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str).filter(|q| is_qid(q))
}

/// `(year, month, day, precision)` of a Wikidata time value.
fn time_parts(v: &Value) -> Option<(i64, u32, u32, u8)> {
    let t = v.get("time")?.as_str()?;
    let prec = u8::try_from(v.get("precision")?.as_u64()?).ok()?;
    let (neg, rest) = match t.as_bytes().first()? {
        b'-' => (true, &t[1..]),
        b'+' => (false, &t[1..]),
        _ => (false, t),
    };
    let mut it = rest.split(['-', 'T']);
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: u32 = it.next()?.parse().ok()?;
    Some((if neg { -y } else { y }, m, d, prec))
}

fn date_of(v: &Value) -> Option<(String, String)> {
    let (y, m, d, p) = time_parts(v)?;
    date_phrase(y, m, d, p)
}

fn year_of(v: &Value) -> Option<i64> {
    time_parts(v).filter(|t| t.3 >= 9).map(|t| t.0)
}

fn qual<'a>(st: &'a Value, p: &str) -> Option<&'a Value> {
    st["q"][p].as_array().and_then(|a| a.first())
}

/// `amount` of a quantity value, in the unit given by its unit QID.
fn amount(v: &Value) -> Option<(f64, &str)> {
    let a: f64 = v
        .get("amount")?
        .as_str()?
        .trim_start_matches('+')
        .parse()
        .ok()?;
    let unit = v.get("unit")?.as_str()?;
    let u = unit.rsplit('/').next().unwrap_or(unit);
    let unit = match u {
        "Q11573" => "m",
        "Q712226" => "km2",
        "1" => "1",
        _ => return None,
    };
    a.is_finite().then_some((a, unit))
}

/// The English (else `mul`) label of `qid`; `None` without one, so that an English sentence
/// never carries a label in another language.
fn label<'a>(labels: &'a Map<String, Value>, qid: &str) -> Option<&'a str> {
    let l = labels.get(qid)?.get("labels")?.as_object()?;
    LABEL_LANGS
        .iter()
        .find_map(|k| l.get(*k))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.chars().count() <= 120)
}

fn sourced(sts: &[&Value]) -> &'static str {
    let with = sts.iter().filter(|s| s["refs"] == true).count();
    match with {
        0 => "no",
        n if n == sts.len() => "yes",
        _ => "partial",
    }
}

/// The QIDs an entity's facts need labels for, most useful first (country first), deduplicated.
pub fn referenced(e: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |q: Option<&str>| {
        if let Some(q) = q
            && !out.iter().any(|x| x == q)
        {
            out.push(q.to_string());
        }
    };
    if e["claims"].get("P297").is_none() {
        push(current_country(e));
    }
    for p in ["P610", "P1589"] {
        for st in statements(e, p).into_iter().take(1) {
            push(item_id(&st["value"]));
        }
    }
    for p in ["P138", "P1435"] {
        for st in all_statements(e, p).into_iter().take(2) {
            push(item_id(&st["value"]));
        }
    }
    for st in all_statements(e, "P793").into_iter().take(MAX_EVENTS) {
        push(item_id(&st["value"]));
    }
    for p in ["P190", "P47"] {
        for st in all_statements(e, p).into_iter().take(MAX_LISTED) {
            push(item_id(&st["value"]));
        }
    }
    out
}

/// The current country item (P17 without an end time, preferred first).
fn current_country(e: &Value) -> Option<&str> {
    let all: Vec<&Value> = e["claims"]["P17"]
        .as_array()
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let open = |s: &&Value| qual(s, "P582").is_none();
    all.iter()
        .copied()
        .filter(open)
        .find(|s| s["rank"] == "preferred")
        .or_else(|| all.iter().copied().find(open))
        .and_then(|s| item_id(&s["value"]))
}

struct Place<'a> {
    qid: &'a str,
    name: String,
    obj: Map<String, Value>,
}

fn place<'a>(e: &'a Value, labels: &Map<String, Value>) -> Option<Place<'a>> {
    let qid = e["id"].as_str()?;
    let l = e["labels"].as_object()?;
    // English sentences: an English (else `mul`) name, never one in another language.
    let (lang, name) = LABEL_LANGS
        .iter()
        .find_map(|k| l.get(*k).map(|v| (*k, v)))?;
    let name = name.as_str()?.to_string();
    let mut obj = Map::new();
    if let Some(g) = best(e, "P1566")
        .and_then(|s| s["value"].as_str())
        .and_then(|g| g.parse::<u64>().ok())
        .filter(|g| *g > 0)
    {
        obj.insert("geonames_id".into(), json!(g));
    }
    obj.insert("wikidata_qid".into(), json!(qid));
    let own_iso = best(e, "P297").and_then(|s| s["value"].as_str());
    let country = own_iso.map(str::to_string).or_else(|| {
        current_country(e)
            .and_then(|c| labels.get(c))
            .and_then(|c| c.get("iso2"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    if own_iso.is_some() {
        obj.insert("admin_level".into(), json!("country"));
    } else if best(e, "P300").is_some() {
        obj.insert("admin_level".into(), json!("admin1"));
    }
    if let Some(c) = country.filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase())) {
        obj.insert("country".into(), json!(c));
    }
    obj.insert(
        "name".into(),
        json!(name.chars().take(200).collect::<String>()),
    );
    obj.insert("name_lang".into(), json!(lang));
    Some(Place { qid, name, obj })
}

/// The `place_fact` items of one slim entity, with the labels of the items it points to
/// (`{QID: {"labels": {lang: label}, "iso2": "DE"}}`; empty on the dump route). Pure.
pub fn facts(e: &Value, labels: &Map<String, Value>) -> Vec<(String, Value)> {
    let Some(p) = place(e, labels) else {
        return Vec::new();
    };
    let as_of = e["modified"]
        .as_str()
        .and_then(|m| m.get(..10))
        .map(str::to_string);
    let mut out: Vec<Fact> = Vec::new();
    let name = p.name.as_str();
    let mk = |prop: &str,
              key: String,
              fact_type: &'static str,
              text: String,
              sts: &[&Value],
              date_ref: Option<String>,
              quantity: Option<(f64, &'static str, Option<String>)>| Fact {
        id: format!("{}:{prop}{key}", p.qid),
        place: p.obj.clone(),
        fact_type,
        text,
        lang: "en".into(),
        origin: TextOrigin::Templated,
        date_ref,
        quantity,
        source_field: format!("wikidata:{prop}"),
        source: (SOURCE.0, SOURCE.1, sourced(sts)),
        licence: ("open", "CC0-1.0", "Wikidata (CC0 1.0)".into()),
        confidence: "structured",
        source_url: Some(format!("https://www.wikidata.org/wiki/{}#{prop}", p.qid)),
        as_of: as_of.clone(),
        value_lang: None,
    };

    // historical
    if let Some(st) = best(e, "P571")
        && let Some((r, ph)) = date_of(&st["value"])
    {
        out.push(mk(
            "P571",
            String::new(),
            "historical",
            format!("{name} was founded {ph}."),
            &[st],
            Some(r),
            None,
        ));
    }
    if let Some(st) = best(e, "P576")
        && let Some((r, ph)) = date_of(&st["value"])
    {
        out.push(mk(
            "P576",
            String::new(),
            "historical",
            format!("{name} was dissolved {ph}."),
            &[st],
            Some(r),
            None,
        ));
    }
    for st in all_statements(e, "P793").into_iter().take(MAX_EVENTS) {
        let (Some(q), Some(when)) = (item_id(&st["value"]), qual(st, "P585")) else {
            continue;
        };
        let (Some(ev), Some((r, ph))) = (label(labels, q), date_of(when)) else {
            continue;
        };
        out.push(mk(
            "P793",
            format!(":{q}"),
            "historical",
            format!("A key event in the history of {name}: {ev}, {ph}."),
            &[st],
            Some(r),
            None,
        ));
    }
    for st in english_first(all_statements(e, "P1448"))
        .into_iter()
        .take(3)
    {
        let Some((t, vl)) = mono(st, 150) else {
            continue;
        };
        let key = format!(":{}", super::place_facts::short_hash(t));
        if let Some(end) = qual(st, "P582").and_then(year_of) {
            let (r, ph) = (
                format!("{end:04}"),
                if end < 0 {
                    format!("{} BCE", -end)
                } else {
                    end.to_string()
                },
            );
            let mut f = mk(
                "P1448",
                key,
                "historical",
                format!("{name} was officially called \u{201c}{t}\u{201d} until {ph}."),
                &[st],
                Some(r),
                None,
            );
            f.value_lang = Some(vl);
            out.push(f);
        } else if t != name {
            let mut f = mk(
                "P1448",
                key,
                "popular",
                format!("The official name of {name} is \u{201c}{t}\u{201d}."),
                &[st],
                None,
                None,
            );
            f.value_lang = Some(vl);
            out.push(f);
        }
    }
    for st in all_statements(e, "P1435").into_iter().take(2) {
        let Some(q) = item_id(&st["value"]) else {
            continue;
        };
        let Some(d) = label(labels, q) else { continue };
        let since = qual(st, "P580").and_then(date_of);
        let text = match &since {
            Some((_, ph)) => format!("{name} was designated {d} {ph}."),
            None => format!("{name} is designated {d}."),
        };
        out.push(mk(
            "P1435",
            format!(":{q}"),
            "historical",
            text,
            &[st],
            since.map(|s| s.0),
            None,
        ));
    }

    // popular
    for st in english_first(all_statements(e, "P1449"))
        .into_iter()
        .take(3)
    {
        let Some((t, vl)) = mono(st, 120) else {
            continue;
        };
        let mut f = mk(
            "P1449",
            format!(":{}", super::place_facts::short_hash(t)),
            "popular",
            format!("{name} is nicknamed \u{201c}{t}\u{201d}."),
            &[st],
            None,
            None,
        );
        f.value_lang = Some(vl);
        out.push(f);
    }
    if let Some(st) = english_first(statements(e, "P1451")).into_iter().next()
        && let Some((t, vl)) = mono(st, 150)
    {
        let mut f = mk(
            "P1451",
            String::new(),
            "popular",
            format!("The motto of {name} is \u{201c}{t}\u{201d}."),
            &[st],
            None,
            None,
        );
        f.value_lang = Some(vl);
        out.push(f);
    }
    if let Some(st) = population(e)
        && let Some((n, "1")) = amount(&st["value"])
        && n >= 1.0
    {
        #[allow(clippy::cast_possible_truncation)]
        let people = n.round() as i64;
        let when = qual(st, "P585").and_then(year_of);
        let text = match when {
            Some(y) => format!("{name} had a population of {} in {y}.", thousands(people)),
            None => format!("{name} has a population of about {}.", thousands(people)),
        };
        out.push(mk(
            "P1082",
            String::new(),
            "popular",
            text,
            &[st],
            when.map(|y| format!("{y:04}")),
            Some((n.round(), "people", None)),
        ));
    }
    if let Some(st) = best(e, "P2044")
        && let Some((m, "m")) = amount(&st["value"])
    {
        let text = if m < 0.0 {
            format!("{name} lies {} m below sea level.", number(-m))
        } else {
            format!("{name} lies {} m above sea level.", number(m))
        };
        out.push(mk(
            "P2044",
            String::new(),
            "popular",
            text,
            &[st],
            None,
            Some((m, "m", None)),
        ));
    }
    if let Some(st) = best(e, "P2046")
        && let Some((a, "km2")) = amount(&st["value"])
        && a > 0.0
    {
        out.push(mk(
            "P2046",
            String::new(),
            "popular",
            format!("{name} covers an area of {} km\u{b2}.", number(a)),
            &[st],
            None,
            Some((a, "km2", None)),
        ));
    }
    if let Some(st) = best(e, "P757")
        && let Some(id) = st["value"].as_str().filter(|s| s.len() <= 20)
    {
        out.push(mk(
            "P757",
            String::new(),
            "popular",
            format!("{name} is a UNESCO World Heritage Site (site {id})."),
            &[st],
            None,
            None,
        ));
    }

    // unbelievable
    for (prop, word) in [("P610", "highest"), ("P1589", "lowest")] {
        let Some(st) = best(e, prop) else { continue };
        let Some(q) = item_id(&st["value"]) else {
            continue;
        };
        let Some(pt) = label(labels, q) else { continue };
        let elev = qual(st, "P2044")
            .and_then(amount)
            .filter(|(_, u)| *u == "m");
        let scope = Some(format!("{word} point of {name}"));
        let (text, quantity) = match elev {
            Some((m, _)) => (
                format!("The {word} point of {name} is {pt} ({} m).", number(m)),
                Some((m, "m", scope)),
            ),
            None => (format!("The {word} point of {name} is {pt}."), None),
        };
        out.push(mk(
            prop,
            String::new(),
            "unbelievable",
            text,
            &[st],
            None,
            quantity,
        ));
    }

    // little_known
    let named: Vec<(&Value, &str)> = all_statements(e, "P138")
        .into_iter()
        .take(2)
        .filter_map(|st| Some((st, label(labels, item_id(&st["value"])?)?)))
        .collect();
    if !named.is_empty() {
        let names: Vec<String> = named.iter().map(|(_, l)| (*l).to_string()).collect();
        let sts: Vec<&Value> = named.iter().map(|(s, _)| *s).collect();
        out.push(mk(
            "P138",
            String::new(),
            "little_known",
            format!("{name} is named after {}.", and_list(&names)),
            &sts,
            None,
            None,
        ));
    }
    for (prop, verb) in [("P190", "is twinned with"), ("P47", "shares a border with")] {
        let all = all_statements(e, prop);
        let listed: Vec<(&Value, &str)> = all
            .iter()
            .take(MAX_LISTED)
            .filter_map(|st| Some((*st, label(labels, item_id(&st["value"])?)?)))
            .collect();
        if listed.is_empty() {
            continue;
        }
        let mut names: Vec<String> = listed.iter().map(|(_, l)| (*l).to_string()).collect();
        let more = all.len() - listed.len();
        if more > 0 {
            names.push(format!(
                "{more} other place{}",
                if more == 1 { "" } else { "s" }
            ));
        }
        let sts: Vec<&Value> = listed.iter().map(|(s, _)| *s).collect();
        out.push(mk(
            prop,
            String::new(),
            "little_known",
            format!("{name} {verb} {}.", and_list(&names)),
            &sts,
            None,
            None,
        ));
    }

    out.into_iter().filter_map(Fact::into_data).collect()
}

/// A monolingual-text value (P1448, P1449, P1451): its text (non-empty, at most `max`
/// characters) and its own language, which the record keeps as `value_lang` (the quoted name
/// or motto may be in another language than the English sentence around it).
fn mono(st: &Value, max: usize) -> Option<(&str, String)> {
    let t = st["value"]["text"]
        .as_str()
        .filter(|t| !t.is_empty() && t.chars().count() <= max)?;
    let l = st["value"]["language"]
        .as_str()
        .filter(|l| {
            !l.is_empty()
                && l.len() <= 16
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .unwrap_or("und");
    Some((t, l.to_ascii_lowercase()))
}

/// Statements with an English value first (stable otherwise).
fn english_first(mut sts: Vec<&Value>) -> Vec<&Value> {
    sts.sort_by_key(|s| s["value"]["language"].as_str() != Some("en"));
    sts
}

/// The population statement: preferred, else the one with the latest P585.
fn population(e: &Value) -> Option<&Value> {
    let sts = statements(e, "P1082");
    if sts.iter().any(|s| s["rank"] == "preferred") {
        return sts.into_iter().next();
    }
    sts.into_iter()
        .max_by_key(|s| qual(s, "P585").and_then(year_of).unwrap_or(i64::MIN))
}

/// The bundle of an EntityData row: the slim entity and the referenced items' labels.
pub fn bundle(entity: Value, labels: BTreeMap<String, Value>) -> Value {
    json!({"format": BUNDLE_FORMAT, "entity": entity, "labels": labels})
}

/// The `place_fact` items of a stored `wikidata_facts` capture: a bundle (EntityData row) or
/// a JSON array of slim entities (a dump batch).
pub fn items(body: &[u8]) -> Result<Vec<(String, Value)>> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("wikidata_facts: {e}")))?;
    match v {
        Value::Object(o) if o.get("format").and_then(Value::as_str) == Some(BUNDLE_FORMAT) => {
            let labels = o
                .get("labels")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            Ok(facts(&o["entity"], &labels))
        }
        Value::Array(a) => {
            let none = Map::new();
            Ok(a.iter().flat_map(|e| facts(e, &none)).collect())
        }
        _ => Err(Error::Invalid(
            "wikidata_facts: not a bundle or a dump batch".into(),
        )),
    }
}

// ---------------------------------------------------------------------------------------------
// The adapter

struct EntityFilter;

impl DumpFilter for EntityFilter {
    fn version(&self) -> &'static str {
        FILTER_VERSION
    }

    fn split(&self) -> Split {
        Split::Lines
    }

    fn output(&self) -> OutputKind {
        OutputKind::JsonArray
    }

    fn filter(&mut self, unit: &[u8]) -> Result<Unit> {
        // One entity per line, each followed by `,`; the first and last lines are `[` and `]`.
        let t = unit.trim_ascii();
        let t = t.strip_suffix(b",").unwrap_or(t);
        if t.len() < 2 || t[0] != b'{' {
            return Ok(Unit::Drop);
        }
        let v: Value =
            serde_json::from_slice(t).map_err(|e| Error::Invalid(format!("wikidata dump: {e}")))?;
        Ok(match slim(&v) {
            Some(s) => Unit::Keep(serde_json::to_vec(&s)?),
            None => Unit::Drop,
        })
    }
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct WikidataFacts {
    settings: DumpSettings,
}

impl WikidataFacts {
    /// Default settings.
    pub const DEFAULT: WikidataFacts = WikidataFacts {
        settings: DumpSettings::DEFAULT,
    };

    /// With dump settings.
    pub fn new(settings: DumpSettings) -> Self {
        WikidataFacts { settings }
    }
}

#[async_trait]
impl SourceAdapter for WikidataFacts {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        if is_dump(&url) {
            return run_dump(ctx, ID, url, &mut EntityFilter, &self.settings).await;
        }
        let qid = entity_data_qid(&url).ok_or_else(|| {
            Error::Invalid("wikidata_facts: not an EntityData or dump URL".into())
        })?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            max_body_bytes: MAX_ENTITY_BYTES,
            subresource: true,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url.clone(), &opts, &mut rep).await?;
        check_status(&ex)?;
        let status = ex.status;
        // Not a place (any more): an empty bundle, so that the facts it had go stale.
        let entity = entity_of(&ex.body, &qid)?;
        let mut labels = BTreeMap::new();
        let refs = entity.as_ref().map(referenced).unwrap_or_default();
        for q in refs.into_iter().take(MAX_REFERENCED) {
            let u = on_origin(&url, &format!("/wiki/Special:EntityData/{q}.json"))?;
            let ex = fetch_raw(ctx, u, &opts, &mut rep).await?;
            match check_status(&ex) {
                Ok(()) => {}
                Err(Error::NotFound(_)) => continue,
                Err(e) => return Err(e),
            }
            if let Some(l) = label_entry(&ex.body, &q)? {
                labels.insert(q, l);
            }
        }
        let body = serde_json::to_vec(&bundle(entity.unwrap_or(Value::Null), labels))?;
        // The bundle carries the item's own status: it stands for that one response, so the
        // processor reconciles the row's facts against it (a fact removed on Wikidata goes
        // stale), unlike a status-less dump batch.
        append_resource_with_status(
            ctx,
            &url,
            MediaType::Json,
            "application/json",
            Bytes::from(body),
            Some(status.as_u16()),
            &mut rep,
        )
        .await?;
        rep.complete = true;
        Ok(rep)
    }
}
