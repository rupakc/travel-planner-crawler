//! `wikivoyage_dump`: the English Wikivoyage `pages-articles` dump (CC BY-SA 4.0), streamed
//! and bzip2-decoded on the fly. Filter v1 keeps main-namespace pages (articles and
//! redirects) as JSON objects `{page_id, title, ns, rev_id, timestamp, url, redirect?, text?}`;
//! contributor names are not kept (attribution is "Wikivoyage contributors" + the page URL).

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::bytes::Regex;
use serde_json::json;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{Error, Result};

use super::common::xml_unescape;
use super::dump::{DumpFilter, DumpSettings, OutputKind, Split, Unit, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "wikivoyage-ns0-v1";

/// Article URL prefix.
pub const WIKI_BASE: &str = "https://en.wikivoyage.org/wiki/";

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

/// Filters one `<page>…</page>` element (pure; exposed for tests and re-processing).
pub fn filter_page(page: &[u8]) -> Result<Option<serde_json::Value>> {
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
        "rev_id": num(&REV_ID),
        "timestamp": cap(&TIMESTAMP, page).map(|b| String::from_utf8_lossy(b).into_owned()),
        "url": format!("{WIKI_BASE}{}", title.replace(' ', "_")),
    });
    if let Some(r) = cap(&REDIRECT, page) {
        obj["redirect"] = xml_unescape(utf8(r)?).into();
    } else if let Some(t) = cap(&TEXT, page) {
        obj["text"] = xml_unescape(utf8(t)?).into();
    }
    Ok(Some(obj))
}

struct PageFilter;

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
        Ok(match filter_page(unit)? {
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
        run_dump(ctx, self.id(), url, &mut PageFilter, &self.settings).await
    }
}
