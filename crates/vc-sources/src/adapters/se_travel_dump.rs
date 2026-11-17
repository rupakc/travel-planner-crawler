//! `se_travel_dump`: the Stack Exchange Travel data dump (CC BY-SA), `Posts.xml` streamed
//! through archive.org's in-archive file view of `travel.stackexchange.com.7z` (no 7z decoder
//! needed). Filter v1 keeps questions and answers as JSON objects. Author identity is kept
//! **only** as the required attribution (display name or user link, spec §5.10); editor
//! names and other personal fields are dropped. Erasure is `admin purge --author`; a purged
//! author stays out of later dumps: the caller hands the adapter the purged authors' hashes
//! ([`SeTravelDump::with_excluded_authors`], from `tombstones`) and their posts are dropped by
//! the filter, before anything is spooled.
//!
//! A leased `https://archive.org/details/<item>` row is mapped to
//! `https://archive.org/download/<item>/<item>/<site file>/Posts.xml`; any other leased URL is
//! downloaded as is.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use quick_xml::events::Event;
use serde_json::{Value, json};
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::xml_attr;
use super::dump::{DumpFilter, DumpSettings, OutputKind, Split, Unit, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "se-travel-posts-v1";
/// The site.
pub const SITE: &str = "https://travel.stackexchange.com";
/// The archive file of the site in the dump item.
pub const SITE_FILE: &str = "travel.stackexchange.com.7z";

/// Filters one `<row …/>` line of `Posts.xml` (pure; exposed for tests and re-processing).
pub fn filter_row(line: &[u8]) -> Result<Option<Value>> {
    let line = std::str::from_utf8(line).map_err(|e| Error::Invalid(format!("se: {e}")))?;
    let t = line.trim_start_matches('\u{feff}').trim();
    if !t.starts_with("<row ") {
        return Ok(None);
    }
    let mut r = quick_xml::Reader::from_str(t);
    let e = match r.read_event() {
        Ok(Event::Empty(e)) => e,
        Ok(_) => return Ok(None),
        Err(e) => return Err(Error::Invalid(format!("se xml: {e}"))),
    };
    let a = |n: &str| xml_attr(&e, n);
    let kind = match a("PostTypeId").as_deref() {
        Some("1") => "question",
        Some("2") => "answer",
        _ => return Ok(None),
    };
    let id: u64 = a("Id")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::Invalid("se: row without Id".into()))?;
    let num = |n: &str| a(n).and_then(|s| s.parse::<i64>().ok());
    let owner_id = num("OwnerUserId");
    let author = a("OwnerDisplayName")
        .filter(|s| !s.trim().is_empty())
        .or_else(|| owner_id.map(|u| format!("user{u}")));
    let short = if kind == "question" { "q" } else { "a" };
    let tags = a("Tags").map(|t| {
        t.split(['|', '<', '>'])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    Ok(Some(json!({
        "id": id,
        "post_type": kind,
        "parent_id": num("ParentId"),
        "accepted_answer_id": num("AcceptedAnswerId"),
        "created": a("CreationDate"),
        "score": num("Score"),
        "title": a("Title"),
        "tags": tags,
        "body_html": a("Body"),
        "content_license": a("ContentLicense"),
        "url": format!("{SITE}/{short}/{id}"),
        "attribution": {
            "author": author,
            "author_url": owner_id.map(|u| format!("{SITE}/users/{u}")),
        },
    })))
}

/// The lowercase hex sha256 of an author name: the form a purged author is excluded by (the
/// `sha256:<hex>` selector of `tombstones` without its prefix).
pub fn author_digest(name: &str) -> String {
    crate::sha256::hex(&crate::sha256::Sha256::digest(name.as_bytes()))
}

/// [`filter_row`], dropping posts whose author's [`author_digest`] is in `excluded`.
pub fn filter_row_excluding(line: &[u8], excluded: &BTreeSet<String>) -> Result<Option<Value>> {
    let v = filter_row(line)?;
    if excluded.is_empty() {
        return Ok(v);
    }
    Ok(v.filter(|p| {
        !p["attribution"]["author"]
            .as_str()
            .is_some_and(|a| excluded.contains(&author_digest(a)))
    }))
}

/// The download URL for a leased row.
pub fn download_url(leased: &CanonicalUrl) -> Result<CanonicalUrl> {
    let path = leased.as_url().path();
    let Some(item) = path.strip_prefix("/details/") else {
        return Ok(leased.clone());
    };
    let item = item.trim_end_matches('/');
    if item.is_empty() || item.contains('/') {
        return Err(Error::Invalid("se: bad archive.org item".into()));
    }
    leased
        .join(&format!("/download/{item}/{item}/{SITE_FILE}/Posts.xml"))
        .map_err(|e| Error::Invalid(e.to_string()))
}

struct PostsFilter {
    excluded: Arc<BTreeSet<String>>,
}

impl DumpFilter for PostsFilter {
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
        Ok(match filter_row_excluding(unit, &self.excluded)? {
            Some(v) => Unit::Keep(serde_json::to_vec(&v)?),
            None => Unit::Drop,
        })
    }
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct SeTravelDump {
    settings: DumpSettings,
    excluded: Option<Arc<BTreeSet<String>>>,
}

impl SeTravelDump {
    /// Default settings.
    pub const DEFAULT: SeTravelDump = SeTravelDump {
        settings: DumpSettings::DEFAULT,
        excluded: None,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        SeTravelDump {
            settings,
            excluded: None,
        }
    }

    /// Drops the posts of these authors (lowercase hex sha256 of the name, see
    /// [`author_digest`]; a `sha256:` prefix is accepted): the purged authors, loaded by the
    /// caller when the run starts.
    #[must_use]
    pub fn with_excluded_authors(mut self, digests: impl IntoIterator<Item = String>) -> Self {
        let set = digests
            .into_iter()
            .map(|d| d.strip_prefix("sha256:").unwrap_or(&d).to_ascii_lowercase())
            .collect();
        self.excluded = Some(Arc::new(set));
        self
    }
}

#[async_trait]
impl SourceAdapter for SeTravelDump {
    fn id(&self) -> &'static str {
        "se_travel_dump"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = download_url(&ctx.url.url)?;
        let mut f = PostsFilter {
            excluded: self.excluded.clone().unwrap_or_default(),
        };
        run_dump(ctx, self.id(), url, &mut f, &self.settings).await
    }
}
