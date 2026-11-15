//! Structured-first extraction (spec §5.7 step 2): schema.org JSON-LD / microdata objects are
//! mapped to kinds by data tables, with no LLM.
//!
//! The mapped record uses the shared field vocabulary below; the kind's schema decides what is
//! kept (records that don't validate are dropped by the pipeline).
//!
//! | Field | From |
//! |---|---|
//! | `name` | `name`, else `headline` |
//! | `description` | `description` (≤ 1,000 chars) |
//! | `url` | `url` |
//! | `start_date`, `end_date` | `startDate`, `endDate` |
//! | `venue` | `location.name` |
//! | `address`, `city`, `region`, `country`, `postal_code` | `address` (string or `PostalAddress`), also via `location` |
//! | `geo {lat, lon}` | `geo` (`GeoCoordinates`) or `latitude`/`longitude`, also via `location` |
//! | `phone` | `telephone` |
//! | `price`, `currency`, `booking_url` | `offers` (first offer) `price`, `priceCurrency`, `url` |
//! | `price_range` | `priceRange` |
//! | `opening_hours` | `openingHours` (string or list, joined with `; `) |
//! | `organizer` | `organizer.name` |
//! | `star_rating` | `starRating.ratingValue` |
//! | `author`, `published`, `modified` | `author.name`, `datePublished`, `dateModified` |
//! | `questions [{question, answer}]` | `FAQPage.mainEntity[]` `name` / `acceptedAnswer.text` |

use serde_json::{Map, Value, json};
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
                put(m, "country", text(a.get("addressCountry")));
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
        put(m, "geo", Some(json!({"lat": lat, "lon": lon})));
    }
}

/// Maps one structured object to `(kind, @id, data)`.
pub fn map_object(o: &Map<String, Value>) -> Option<(KindId, Option<String>, Value)> {
    let kind = kind_for(o)?;
    let mut m = Map::new();
    put(
        &mut m,
        "name",
        text(o.get("name")).or_else(|| text(o.get("headline"))),
    );
    put(
        &mut m,
        "description",
        text(o.get("description")).map(|d| d.chars().take(1000).collect::<String>()),
    );
    put(&mut m, "url", text(o.get("url")));
    put(&mut m, "start_date", text(o.get("startDate")));
    put(&mut m, "end_date", text(o.get("endDate")));
    put(&mut m, "phone", text(o.get("telephone")));
    put(&mut m, "price_range", text(o.get("priceRange")));
    match o.get("openingHours") {
        Some(Value::Array(a)) => {
            let hs: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            if !hs.is_empty() {
                put(&mut m, "opening_hours", Some(hs.join("; ")));
            }
        }
        other => put(&mut m, "opening_hours", text(other)),
    }
    put(&mut m, "organizer", text(o.get("organizer")));
    put(
        &mut m,
        "star_rating",
        number(first_obj(o.get("starRating")).and_then(|s| s.get("ratingValue"))),
    );
    put(&mut m, "author", text(o.get("author")));
    put(&mut m, "published", text(o.get("datePublished")));
    put(&mut m, "modified", text(o.get("dateModified")));
    if let Some(off) = first_obj(o.get("offers")) {
        put(
            &mut m,
            "price",
            number(off.get("price")).or(number(off.get("lowPrice"))),
        );
        put(&mut m, "currency", text(off.get("priceCurrency")));
        put(&mut m, "booking_url", text(off.get("url")));
    }
    address_into(&mut m, o);
    if let Some(loc) = first_obj(o.get("location")) {
        put(&mut m, "venue", text(loc.get("name")));
        address_into(&mut m, loc);
    } else if let Some(Value::String(s)) = o.get("location") {
        put(&mut m, "venue", Some(s.trim().to_string()));
    }
    if types_of(o).iter().any(|t| t == "FAQPage") {
        let qs: Vec<Value> = match o.get("mainEntity") {
            Some(Value::Array(a)) => a.iter().collect::<Vec<_>>(),
            Some(v) => vec![v],
            None => vec![],
        }
        .into_iter()
        .filter_map(Value::as_object)
        .filter_map(|q| {
            let question = text(q.get("name"))?;
            let answer = first_obj(q.get("acceptedAnswer")).and_then(|a| text(a.get("text")));
            Some(json!({"question": question, "answer": answer}))
        })
        .collect();
        if !qs.is_empty() {
            put(&mut m, "questions", Some(Value::Array(qs)));
        }
    }
    let id = o.get("@id").and_then(Value::as_str).map(str::to_string);
    Some((kind, id, Value::Object(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(d["price"], 25.0);
        assert_eq!(d["geo"]["lat"], 38.72);
    }

    #[test]
    fn unmapped_types_are_ignored() {
        let o = json!({"@type": "BreadcrumbList"});
        assert!(map_object(o.as_object().unwrap()).is_none());
        let o = json!({"@type": ["Thing", "FAQPage"], "mainEntity": [{"name": "Q?", "acceptedAnswer": {"text": "A."}}]});
        let (k, _, d) = map_object(o.as_object().unwrap()).unwrap();
        assert_eq!(&*k.0, "qa_thread");
        assert_eq!(d["questions"][0]["answer"], "A.");
    }
}
