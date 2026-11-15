//! JSON: JSON-LD documents are flattened like in HTML; any other JSON (API responses) is kept
//! as one structured value for adapters and extraction.

use serde_json::Value;
use vc_core::media::{MediaType, MediaTypeMatch};
use vc_core::parse::{ContentHandler, Limits, Parsed};
use vc_core::store::CaptureMeta;
use vc_core::{Error, Result};

use crate::structured::{flatten_jsonld, looks_like_jsonld};
use crate::text::maybe_gunzip;

/// The JSON handler. Nesting beyond serde_json's recursion limit (128) is rejected.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonHandler;

impl ContentHandler for JsonHandler {
    fn media(&self) -> &'static [MediaTypeMatch] {
        &[MediaTypeMatch::Is(MediaType::Json)]
    }

    fn handle(&self, _meta: &CaptureMeta, body: &[u8], _lim: &Limits) -> Result<Parsed> {
        let body = maybe_gunzip(body)?;
        let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&body);
        let v: Value =
            serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("json: {e}")))?;
        let mut structured = Vec::new();
        if looks_like_jsonld(&v) {
            flatten_jsonld(v, &mut structured);
        } else {
            structured.push(v);
        }
        Ok(Parsed {
            structured,
            ..Parsed::default()
        })
    }
}
