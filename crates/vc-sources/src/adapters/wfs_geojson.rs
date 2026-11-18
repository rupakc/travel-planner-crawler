//! `wfs_geojson`: small city point layers served by an OGC WFS as GeoJSON (Vienna
//! `data.wien.gv.at`, Zürich `www.ogd.stadt-zuerich.ch`), driven by a [`WfsMapping`] per layer:
//! the endpoint path, the layer name (`typeName`) and the [`RowMapping`] of a feature
//! (`/properties/<NAME>`, `/geometry/coordinates/1` for the latitude).
//!
//! The leased URL is the source's seed, a complete `GetFeature` request (`outputFormat` JSON or
//! GeoJSON, WGS 84); one request per run, spooled before anything is mapped. Layers are a few
//! hundred features, so there is no paging.
//!
//! **Adding a layer:** add a mapping, record a fixture, add the `GetFeature` URL to the source's
//! seeds and point its `adapter` at `wfs_geojson`.

use async_trait::async_trait;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, json};
use super::row_map::{Fields, Kind, RowMapping, map_row};

/// Adapter id.
pub const ID: &str = "wfs_geojson";

/// One layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WfsMapping {
    /// The WFS endpoint path.
    pub path: &'static str,
    /// The layer (`typeName` / `typename`, compared case-insensitively, namespace included).
    pub layer: &'static str,
    /// Feature → record.
    pub map: RowMapping,
}

const LAT: &str = "/geometry/coordinates/1";
const LNG: &str = "/geometry/coordinates/0";

const VIENNA: RowMapping = RowMapping {
    source: "",
    kind: Kind::Attraction,
    fields: Fields::NONE,
    city: "Vienna",
    country: "Austria",
    country_code: "AT",
    categories: &[],
    accessible: &[],
};

const ZURICH: RowMapping = RowMapping {
    city: "Zurich",
    country: "Switzerland",
    country_code: "CH",
    ..VIENNA
};

/// Every built-in mapping.
pub const MAPPINGS: &[WfsMapping] = &[
    // Vienna sights (Sehenswürdigkeiten), City of Vienna, CC BY 4.0.
    WfsMapping {
        path: "/daten/geo",
        layer: "ogdwien:SEHENSWUERDIGOGD",
        map: RowMapping {
            source: "wien-ogd",
            kind: Kind::Attraction,
            fields: Fields {
                id: &["/id"],
                name: "/properties/NAME",
                address: &["/properties/ADRESSE"],
                url: "/properties/WEITERE_INF",
                lat: LAT,
                lng: LNG,
                ..Fields::NONE
            },
            categories: &[("", "landmark")],
            ..VIENNA
        },
    },
    // Vienna museums.
    WfsMapping {
        path: "/daten/geo",
        layer: "ogdwien:MUSEUMOGD",
        map: RowMapping {
            source: "wien-ogd",
            kind: Kind::Attraction,
            fields: Fields {
                id: &["/id"],
                name: "/properties/NAME",
                address: &["/properties/ADRESSE"],
                url: "/properties/WEITERE_INF",
                lat: LAT,
                lng: LNG,
                ..Fields::NONE
            },
            categories: &[("", "museum")],
            ..VIENNA
        },
    },
    // Vienna public toilets (MA 48): street corner, hours, attendant and accessible cabin.
    WfsMapping {
        path: "/daten/geo",
        layer: "ogdwien:WCANLAGEOGD",
        map: RowMapping {
            source: "wien-ogd",
            kind: Kind::Amenity,
            fields: Fields {
                id: &["/id"],
                name_fallback: Some(("Public toilet", "/properties/STRASSE")),
                address: &["/properties/STRASSE"],
                phone: "/properties/TELEFON",
                url: "/properties/INFORMATION",
                hours: "/properties/OEFFNUNGSZEIT",
                description: "/properties/KATEGORIE",
                accessible: "/properties/KATEGORIE",
                lat: LAT,
                lng: LNG,
                ..Fields::NONE
            },
            categories: &[("", "toilet")],
            accessible: &["behindertenkabine"],
            ..VIENNA
        },
    },
    // Vienna free public Wi-Fi (wien.at Wi-Fi) locations.
    WfsMapping {
        path: "/daten/geo",
        layer: "ogdwien:WLANWIENATOGD",
        map: RowMapping {
            source: "wien-ogd",
            kind: Kind::Amenity,
            fields: Fields {
                id: &["/id"],
                name: "/properties/NAME",
                address: &["/properties/ADRESSE"],
                url: "/properties/WEITERE_INFORMATIONEN",
                lat: LAT,
                lng: LNG,
                ..Fields::NONE
            },
            categories: &[("", "wifi_hotspot")],
            ..VIENNA
        },
    },
    // Züri WC (Stadt Zürich, CC0): toilets without and with wheelchair access (two layers).
    WfsMapping {
        path: "/wfs/geoportal/Zueri_WC",
        layer: "poi_zueriwc_view",
        map: ZURICH_WC,
    },
    WfsMapping {
        path: "/wfs/geoportal/Zueri_WC",
        layer: "poi_zueriwc_rs_view",
        map: ZURICH_WC,
    },
];

const ZURICH_WC: RowMapping = RowMapping {
    source: "zuerich-ogd",
    kind: Kind::Amenity,
    fields: Fields {
        id: &["/id"],
        name: "/properties/name",
        address: &["/properties/adresse"],
        hours: "/properties/oeffnungsz",
        description: "/properties/bemerkung",
        accessible: "/properties/kategorie",
        lat: LAT,
        lng: LNG,
        ..Fields::NONE
    },
    categories: &[("", "toilet")],
    accessible: &["(rollstuhlgängig"],
    ..ZURICH
};

/// The layer named by a `GetFeature` URL (`typeName` or `typename`).
pub fn layer(url: &CanonicalUrl) -> Option<String> {
    url.as_url()
        .query_pairs()
        .find(|(k, _)| k.eq_ignore_ascii_case("typename") || k.eq_ignore_ascii_case("typenames"))
        .map(|(_, v)| v.into_owned())
}

/// The mapping of a `GetFeature` URL (path and layer).
pub fn mapping_for(url: &CanonicalUrl) -> Option<&'static WfsMapping> {
    let (p, l) = (url.as_url().path(), layer(url)?);
    MAPPINGS
        .iter()
        .find(|m| m.path == p && m.layer.eq_ignore_ascii_case(&l))
}

/// The records (`(id, kind, data)`) of a stored `GetFeature` response.
pub fn items(
    url: &CanonicalUrl,
    body: &[u8],
) -> Result<Vec<(Option<String>, &'static str, Value)>> {
    let m = mapping_for(url)
        .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this layer")))?;
    let v: Value = json(body)?;
    let features = v
        .get("features")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Invalid(format!("{ID}: not a GeoJSON FeatureCollection")))?;
    Ok(features.iter().filter_map(|f| map_row(&m.map, f)).collect())
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct WfsGeojson;

#[async_trait]
impl SourceAdapter for WfsGeojson {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let row = ctx.url.url.clone();
        if mapping_for(&row).is_none() {
            return Err(Error::Invalid(format!("{ID}: no mapping for this layer")));
        }
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            conditional: true,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, row, &opts, &mut rep).await?;
        if ex.status.as_u16() != 304 {
            check_status(&ex)?;
        }
        rep.complete = true;
        Ok(rep)
    }
}
