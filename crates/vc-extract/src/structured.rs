//! Structured-first extraction (spec §5.7 step 2): schema.org JSON-LD / microdata objects are
//! mapped to kinds by data tables, with no LLM.
//!
//! The mapped record uses the field names of the v1 kind schemas (`schemas/<kind>.v1.json`);
//! the pipeline then fits it to the kind's schema (unknown fields dropped, long strings cut,
//! invalid enum values removed) and drops it if it still doesn't validate.
//!
//! | Field | From |
//! |---|---|
//! | `name` (`headline` for articles) | `name`, else `headline` |
//! | `description` / `summary` | `description` |
//! | `category`, `place_type`, `property_type`, `article_type` | the schema.org type |
//! | `start_date`, `start_time`, `end_date` | `startDate` (date + `HH:MM`), `endDate` |
//! | `venue` | `location.name` |
//! | `address`, `city`, `region`, `country`, `country_code` | `address` (string or `PostalAddress`), also via `location` |
//! | `lat`, `lng` | `geo` (`GeoCoordinates`) or `latitude`/`longitude`, also via `location` |
//! | `phone` | `telephone` |
//! | `price` (events) / `admission` (attractions) `{text, amount, currency, is_free}` | first `offers`; `isAccessibleForFree` |
//! | `ticket_url` (events), `info_url` (attractions), `website_url` (hotels) | `offers.url`, `url`, `url` |
//! | `opening_hours` | `openingHours` (string or list, joined with `; `) |
//! | `star_rating` | `starRating.ratingValue` |
//! | `author`, `published_date`, `updated_date` | `author.name`, `datePublished`, `dateModified` |
//! | `question_title`, `question_summary`, `answer_summary`, `answer_count` | `FAQPage.mainEntity[0]` |
//!
//! `source_url` is set to the page URL by the pipeline.

use serde_json::{Map, Value};
use vc_core::KindId;

/// Bumped when the mapping tables change (part of the extractor version).
pub const MAPPING_VERSION: u32 = 1;

/// schema.org type → kind id. Types ending in `Event` map to `event` as well.
pub const TYPE_TO_KIND: &[(&str, &str)] = &[
    ("Event", "event"),
    ("Festival", "event"),
    ("TouristAttraction", "attraction"),
    ("Museum", "attraction"),
    ("LandmarksOrHistoricalBuildings", "attraction"),
    ("Park", "attraction"),
    ("Zoo", "attraction"),
    ("Aquarium", "attraction"),
    ("AmusementPark", "attraction"),
    ("ArtGallery", "attraction"),
    ("PlaceOfWorship", "attraction"),
    ("Church", "attraction"),
    ("Monument", "attraction"),
    ("Place", "place"),
    ("City", "place"),
    ("TouristDestination", "place"),
    ("AdministrativeArea", "place"),
    ("LodgingBusiness", "hotel"),
    ("Hotel", "hotel"),
    ("Hostel", "hotel"),
    ("Motel", "hotel"),
    ("Resort", "hotel"),
    ("BedAndBreakfast", "hotel"),
    ("Campground", "hotel"),
    ("Article", "article"),
    ("NewsArticle", "article"),
    ("BlogPosting", "article"),
    ("Report", "article"),
    ("FAQPage", "qa_thread"),
    ("Airport", "airport"),
    ("TouristTrip", "walking_tour"),
];

fn types_of(o: &Map<String, Value>) -> Vec<String> {
    let t = o.get("@type").or_else(|| o.get("type"));
    let raw: Vec<&str> = match t {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    raw.into_iter()
        .map(|s| s.rsplit(['/', ':', '#']).next().unwrap_or(s).to_string())
        .collect()
}

/// The kind for a structured object, if its type is mapped.
pub fn kind_for(o: &Map<String, Value>) -> Option<KindId> {
    for t in types_of(o) {
        if let Some((_, k)) = TYPE_TO_KIND.iter().find(|(ty, _)| *ty == t) {
            return Some(KindId::new(k));
        }
        if t.ends_with("Event") {
            return Some(KindId::new("event"));
        }
    }
    None
}

/// Flattens `@graph` containers into their member objects.
pub fn flatten(objs: &[Value]) -> Vec<Map<String, Value>> {
    let mut out = Vec::new();
    for v in objs {
        match v {
            Value::Object(o) => {
                if let Some(Value::Array(g)) = o.get("@graph") {
                    out.extend(flatten(g));
                } else {
                    out.push(o.clone());
                }
            }
            Value::Array(a) => out.extend(flatten(a)),
            _ => {}
        }
    }
    out
}

fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(o) => text(o.get("name").or_else(|| o.get("@value"))),
        Value::Array(a) => a.iter().find_map(|x| text(Some(x))),
        _ => None,
    }
}

fn number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', ".").parse().ok(),
        Value::Array(a) => a.iter().find_map(|x| number(Some(x))),
        _ => None,
    }
}

fn first_obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    match v? {
        Value::Object(o) => Some(o),
        Value::Array(a) => a.iter().find_map(Value::as_object),
        _ => None,
    }
}

fn put(m: &mut Map<String, Value>, k: &str, v: Option<impl Into<Value>>) {
    if let Some(v) = v {
        m.entry(k.to_string()).or_insert(v.into());
    }
}

fn address_into(m: &mut Map<String, Value>, o: &Map<String, Value>) {
    match o.get("address") {
        Some(Value::String(s)) => put(m, "address", Some(s.trim().to_string())),
        Some(a) => {
            if let Some(a) = first_obj(Some(a)) {
                let parts: Vec<String> = [
                    "streetAddress",
                    "addressLocality",
                    "addressRegion",
                    "postalCode",
                    "addressCountry",
                ]
                .iter()
                .filter_map(|k| text(a.get(*k)))
                .collect();
                if !parts.is_empty() {
                    put(m, "address", Some(parts.join(", ")));
                }
                put(m, "city", text(a.get("addressLocality")));
                put(m, "region", text(a.get("addressRegion")));
                let country = text(a.get("addressCountry"));
                if let Some(c) = country.as_deref().filter(|c| c.len() == 2) {
                    put(m, "country_code", Some(c.to_ascii_uppercase()));
                }
                put(m, "country", country);
                put(m, "postal_code", text(a.get("postalCode")));
            }
        }
        None => {}
    }
    let geo = first_obj(o.get("geo"));
    let lat = number(geo.and_then(|g| g.get("latitude")).or(o.get("latitude")));
    let lon = number(geo.and_then(|g| g.get("longitude")).or(o.get("longitude")));
    if let (Some(lat), Some(lon)) = (lat, lon)
        && (-90.0..=90.0).contains(&lat)
        && (-180.0..=180.0).contains(&lon)
    {
        put(m, "lat", Some(lat));
        put(m, "lng", Some(lon));
    }
}

/// schema.org type → `(kind, category field, category value)`.
const CATEGORY: &[(&str, &str, &str)] = &[
    ("MusicEvent", "category", "music"),
    ("Festival", "category", "festival"),
    ("ExhibitionEvent", "category", "exhibition"),
    ("SportsEvent", "category", "sports"),
    ("TheaterEvent", "category", "theatre"),
    ("BusinessEvent", "category", "conference"),
    ("Museum", "category", "museum"),
    ("ArtGallery", "category", "museum"),
    ("Park", "category", "park_garden"),
    ("Zoo", "category", "zoo_aquarium"),
    ("Aquarium", "category", "zoo_aquarium"),
    ("AmusementPark", "category", "theme_park"),
    ("LandmarksOrHistoricalBuildings", "category", "landmark"),
    ("PlaceOfWorship", "category", "religious_site"),
    ("Church", "category", "religious_site"),
    ("Monument", "category", "monument"),
    ("City", "place_type", "city"),
    ("AdministrativeArea", "place_type", "region"),
    ("Hotel", "property_type", "hotel"),
    ("Hostel", "property_type", "hostel"),
    ("Resort", "property_type", "resort"),
    ("BedAndBreakfast", "property_type", "bnb"),
    ("Campground", "property_type", "campsite"),
    ("NewsArticle", "article_type", "news"),
];

fn date_part(s: &str) -> Option<String> {
    let d = s.get(..10)?;
    let b = d.as_bytes();
    (b[4] == b'-' && b[7] == b'-' && d.chars().filter(char::is_ascii_digit).count() == 8)
        .then(|| d.to_string())
}

fn time_part(s: &str) -> Option<String> {
    let t = s.get(11..16)?;
    let b = t.as_bytes();
    (b[2] == b':' && t.chars().filter(char::is_ascii_digit).count() == 4).then(|| t.to_string())
}

fn price_obj(o: &Map<String, Value>) -> Option<Value> {
    let off = first_obj(o.get("offers"));
    let amount = off.and_then(|f| number(f.get("price")).or(number(f.get("lowPrice"))));
    let free = o
        .get("isAccessibleForFree")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || amount == Some(0.0);
    if amount.is_none() && !free {
        return None;
    }
    let currency = off.and_then(|f| text(f.get("priceCurrency")));
    let mut p = Map::new();
    let txt = match (amount, &currency) {
        (Some(0.0), _) => "Free".to_string(),
        (Some(a), Some(c)) => format!("{a} {c}"),
        (Some(a), None) => a.to_string(),
        (None, _) => "Free".to_string(),
    };
    put(&mut p, "text", Some(txt));
    put(&mut p, "amount", amount.or(free.then_some(0.0)));
    put(
        &mut p,
        "currency",
        currency
            .filter(|c| c.len() == 3)
            .map(|c| c.to_ascii_uppercase()),
    );
    put(&mut p, "is_free", Some(free));
    Some(Value::Object(p))
}

/// Maps one structured object to `(kind, @id, data)`.
pub fn map_object(o: &Map<String, Value>) -> Option<(KindId, Option<String>, Value)> {
    let kind = kind_for(o)?;
    let types = types_of(o);
    let mut m = Map::new();
    let k = &*kind.0;
    let name = text(o.get("name")).or_else(|| text(o.get("headline")));
    let desc = text(o.get("description"));
    if let Some((_, field, val)) = CATEGORY
        .iter()
        .find(|(t, _, _)| types.iter().any(|x| x == t))
    {
        put(&mut m, field, Some(*val));
    }
    match k {
        "article" => {
            put(&mut m, "headline", name);
            put(&mut m, "summary", desc);
            put(&mut m, "author", text(o.get("author")));
            put(
                &mut m,
                "published_date",
                text(o.get("datePublished")).and_then(|d| date_part(&d)),
            );
            put(
                &mut m,
                "updated_date",
                text(o.get("dateModified")).and_then(|d| date_part(&d)),
            );
            put(&mut m, "article_type", Some("other"));
        }
        "qa_thread" => {
            let qs: Vec<&Map<String, Value>> = match o.get("mainEntity") {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_object).collect(),
                Some(Value::Object(q)) => vec![q],
                _ => vec![],
            };
            let q = qs.first()?;
            let title = text(q.get("name"))?;
            put(
                &mut m,
                "question_summary",
                text(q.get("text")).or(Some(title.clone())),
            );
            put(&mut m, "question_title", Some(title));
            put(
                &mut m,
                "answer_summary",
                first_obj(q.get("acceptedAnswer")).and_then(|a| text(a.get("text"))),
            );
            put(
                &mut m,
                "has_accepted_answer",
                Some(q.get("acceptedAnswer").is_some()),
            );
            put(
                &mut m,
                "answer_count",
                q.get("answerCount").and_then(Value::as_i64),
            );
        }
        _ => {
            put(&mut m, "name", name);
            put(
                &mut m,
                if k == "place" {
                    "summary"
                } else {
                    "description"
                },
                desc,
            );
        }
    }
    put(
        &mut m,
        "category",
        (k == "event" || k == "attraction").then_some("other"),
    );
    put(&mut m, "place_type", (k == "place").then_some("other"));
    put(&mut m, "property_type", (k == "hotel").then_some("other"));
    if let Some(s) = text(o.get("startDate")) {
        put(&mut m, "start_date", date_part(&s));
        put(&mut m, "start_time", time_part(&s));
    }
    put(
        &mut m,
        "end_date",
        text(o.get("endDate")).and_then(|d| date_part(&d)),
    );
    put(&mut m, "phone", text(o.get("telephone")));
    match o.get("openingHours") {
        Some(Value::Array(a)) => {
            let hs: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            if !hs.is_empty() {
                put(&mut m, "opening_hours", Some(hs.join("; ")));
            }
        }
        other => put(&mut m, "opening_hours", text(other)),
    }
    put(
        &mut m,
        "star_rating",
        number(first_obj(o.get("starRating")).and_then(|s| s.get("ratingValue"))),
    );
    match k {
        "event" => {
            put(&mut m, "price", price_obj(o));
            put(
                &mut m,
                "ticket_url",
                first_obj(o.get("offers")).and_then(|f| text(f.get("url"))),
            );
        }
        "attraction" => {
            put(&mut m, "admission", price_obj(o));
            put(&mut m, "info_url", text(o.get("url")));
        }
        "hotel" => put(&mut m, "website_url", text(o.get("url"))),
        _ => {}
    }
    address_into(&mut m, o);
    if let Some(loc) = first_obj(o.get("location")) {
        put(&mut m, "venue", text(loc.get("name")));
        address_into(&mut m, loc);
    } else if let Some(Value::String(s)) = o.get("location") {
        put(&mut m, "venue", Some(s.trim().to_string()));
    }
    let id = o.get("@id").and_then(Value::as_str).map(str::to_string);
    Some((kind, id, Value::Object(m)))
}

/// Fits a value to a schema: unknown properties dropped (when `additionalProperties` is
/// false), strings cut to `maxLength`, values outside an `enum` removed. Validation still
/// decides.
pub fn fit_to_schema(v: &mut Value, schema: &Value) {
    match v {
        Value::Object(m) => {
            let props = schema.get("properties").and_then(Value::as_object);
            let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
            m.retain(|k, val| {
                let Some(ps) = props.and_then(|p| p.get(k)) else {
                    return !closed;
                };
                if let Some(e) = ps.get("enum").and_then(Value::as_array) {
                    return e.contains(val);
                }
                true
            });
            if let Some(p) = props {
                for (k, val) in m.iter_mut() {
                    if let Some(ps) = p.get(k) {
                        fit_to_schema(val, ps);
                    }
                }
            }
        }
        Value::Array(a) => {
            if let Some(items) = schema.get("items") {
                a.iter_mut().for_each(|x| fit_to_schema(x, items));
            }
        }
        Value::String(s) => {
            if let Some(n) = schema.get("maxLength").and_then(Value::as_u64)
                && s.chars().count() > n as usize
            {
                *s = s.chars().take(n as usize).collect();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_event_with_location_and_offer() {
        let v = json!({
            "@context": "https://schema.org",
            "@graph": [{
                "@type": "MusicEvent", "@id": "https://example.org/e/1",
                "name": "Jazz Night", "startDate": "2026-11-20T19:00",
                "location": {"@type": "Place", "name": "Blue Room",
                    "address": {"@type": "PostalAddress", "addressLocality": "Lisbon", "addressCountry": "PT"},
                    "geo": {"latitude": "38.72", "longitude": -9.14}},
                "offers": {"price": "25.00", "priceCurrency": "EUR", "url": "https://example.org/t/1"}
            }]
        });
        let objs = flatten(&[v]);
        let (k, id, d) = map_object(&objs[0]).unwrap();
        assert_eq!(&*k.0, "event");
        assert_eq!(id.as_deref(), Some("https://example.org/e/1"));
        assert_eq!(d["city"], "Lisbon");
        assert_eq!(d["venue"], "Blue Room");
        assert_eq!(d["category"], "music");
        assert_eq!(d["start_date"], "2026-11-20");
        assert_eq!(d["start_time"], "19:00");
        assert_eq!(d["price"]["amount"], 25.0);
        assert_eq!(d["price"]["currency"], "EUR");
        assert_eq!(d["price"]["is_free"], false);
        assert_eq!(d["ticket_url"], "https://example.org/t/1");
        assert_eq!(d["country_code"], "PT");
        assert_eq!(d["lat"], 38.72);
    }

    #[test]
    fn fit_drops_unknown_and_cuts() {
        let schema = json!({"type": "object", "additionalProperties": false, "properties": {
            "name": {"type": "string", "maxLength": 3}, "category": {"enum": ["a"]},
            "price": {"type": "object", "additionalProperties": false, "properties": {"amount": {"type": "number"}}}}});
        let mut v =
            json!({"name": "abcdef", "category": "b", "x": 1, "price": {"amount": 1, "y": 2}});
        fit_to_schema(&mut v, &schema);
        assert_eq!(v, json!({"name": "abc", "price": {"amount": 1}}));
    }

    #[test]
    fn unmapped_types_are_ignored() {
        let o = json!({"@type": "BreadcrumbList"});
        assert!(map_object(o.as_object().unwrap()).is_none());
        let o = json!({"@type": ["Thing", "FAQPage"], "mainEntity": [{"name": "Q?", "acceptedAnswer": {"text": "A."}}]});
        let (k, _, d) = map_object(o.as_object().unwrap()).unwrap();
        assert_eq!(&*k.0, "qa_thread");
        assert_eq!(d["question_title"], "Q?");
        assert_eq!(d["answer_summary"], "A.");
    }
}
