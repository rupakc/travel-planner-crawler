//! `govuk_world_orgs`: UK embassies, high commissions and consulates from GOV.UK (OGL v3).
//!
//! Two kinds of leased row, told apart by path:
//! - the search API page `/api/search.json?filter_format=worldwide_organisation&…` → spool,
//!   then enqueue one Content API row per organisation (`/api/content/world/organisations/<slug>`)
//!   and, when the listing has more results, the next page (`start=`);
//! - an organisation `/api/content/world/organisations/<slug>` → spool (conditional GET). The
//!   record mapping ([`parse_org`] → `embassy` records) reads its offices: the main office and
//!   any home-page offices, each with its contact (address, phones) and opening hours.
//!
//! Follow-ups are always resolved on the leased URL's own origin.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{
    Fetch, check_status, enqueue_adapter_rows, fetch_raw, json, not_modified, on_origin,
};
use super::html_text;

/// The search API path.
pub const SEARCH_PATH: &str = "/api/search.json";
/// Content API prefix of worldwide organisations.
pub const ORG_PREFIX: &str = "/api/content/world/organisations/";
/// Cap on organisations enqueued from one search page.
pub const MAX_ORGS_PER_PAGE: usize = 1000;

/// One search API page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPage {
    /// Organisation paths (`/world/organisations/<slug>`), in listing order.
    pub links: Vec<String>,
    /// `total`.
    pub total: u64,
    /// `start`.
    pub start: u64,
}

#[derive(Deserialize)]
struct Search {
    #[serde(default)]
    results: Vec<SearchResult>,
    #[serde(default)]
    total: u64,
    #[serde(default)]
    start: u64,
}
#[derive(Deserialize)]
struct SearchResult {
    link: Option<String>,
}

fn is_org_link(l: &str) -> bool {
    l.strip_prefix("/world/organisations/").is_some_and(|slug| {
        !slug.is_empty()
            && slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

/// Parses a search API page (organisation links only).
pub fn parse_search(body: &[u8]) -> Result<SearchPage> {
    let s: Search = json(body)?;
    Ok(SearchPage {
        links: s
            .results
            .into_iter()
            .filter_map(|r| r.link)
            .filter(|l| is_org_link(l))
            .collect(),
        total: s.total,
        start: s.start,
    })
}

/// A phone number as GOV.UK lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrgPhone {
    /// `title` (e.g. "Fax", "Consular enquiries").
    pub title: String,
    /// `number` as written.
    pub number: String,
}

/// One office of an organisation, with its contact.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrgOffice {
    /// Office title.
    pub title: String,
    /// `content_id`.
    pub content_id: String,
    /// The office page.
    pub web_url: Option<String>,
    /// `public_updated_at` of the office.
    pub updated_at: Option<String>,
    /// Opening hours (the paragraphs under an "… hours" heading of `access_and_opening_times`).
    pub hours: Option<String>,
    /// Street address, locality and postal code, as written.
    pub address: Option<String>,
    /// Locality.
    pub city: Option<String>,
    /// The address's `iso2_country_code`, upper-cased.
    pub country_code: Option<String>,
    /// The address's `world_location`.
    pub country: Option<String>,
    /// Phones.
    pub phones: Vec<OrgPhone>,
    /// Emails.
    pub emails: Vec<String>,
    /// The official contact form for emergencies and enquiries, when the contact names one.
    pub contact_form_url: Option<String>,
    /// `public_updated_at` of the contact.
    pub contact_updated_at: Option<String>,
}

/// A worldwide organisation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Org {
    /// Title ("British Embassy Bangkok").
    pub title: String,
    /// `base_path`.
    pub base_path: String,
    /// The first world location (the country it is in).
    pub world_location: Option<String>,
    /// Its offices (main office first).
    pub offices: Vec<OrgOffice>,
}

static HOURS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(opening|office|working)\s+hours\b").expect("regex"));
static CONTACT_FORM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https://www\.contact\.service\.csd\.fcdo\.gov\.uk/[A-Za-z0-9/_\-]+")
        .expect("regex")
});

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn hours_of(html: &str) -> Option<String> {
    let paras = html_text::under_heading(html, &HOURS);
    let first = paras.into_iter().next()?;
    (first.chars().count() <= 400).then_some(first)
}

fn office(o: &Value, contact: Option<&Value>) -> Option<OrgOffice> {
    let title = str_at(o, &["title"])?.to_string();
    let content_id = str_at(o, &["content_id"])?.to_string();
    let mut out = OrgOffice {
        title,
        content_id,
        web_url: str_at(o, &["web_url"]).map(str::to_string),
        updated_at: str_at(o, &["public_updated_at"]).map(str::to_string),
        hours: str_at(o, &["details", "access_and_opening_times"]).and_then(hours_of),
        address: None,
        city: None,
        country_code: None,
        country: None,
        phones: Vec::new(),
        emails: Vec::new(),
        contact_form_url: None,
        contact_updated_at: None,
    };
    let Some(c) = contact else {
        return Some(out);
    };
    let d = c.get("details").unwrap_or(&Value::Null);
    if let Some(a) = d
        .get("post_addresses")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
    {
        let parts: Vec<&str> = ["street_address", "locality", "postal_code"]
            .iter()
            .filter_map(|k| str_at(a, &[k]))
            .collect();
        out.address = Some(parts.join(", ")).filter(|s| !s.is_empty());
        out.city = str_at(a, &["locality"]).map(str::to_string);
        out.country_code = str_at(a, &["iso2_country_code"])
            .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic()))
            .map(str::to_ascii_uppercase);
        out.country = str_at(a, &["world_location"]).map(str::to_string);
    }
    for p in d
        .get("phone_numbers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(n) = str_at(p, &["number"]) {
            out.phones.push(OrgPhone {
                title: str_at(p, &["title"]).unwrap_or_default().to_string(),
                number: n.to_string(),
            });
        }
    }
    for e in d
        .get("email_addresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(m) = str_at(e, &["email"]).filter(|m| m.contains('@')) {
            out.emails.push(m.to_string());
        }
    }
    let form_link = d
        .get("contact_form_links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| str_at(l, &["link"]))
        .find(|l| CONTACT_FORM.is_match(l))
        .map(str::to_string);
    out.contact_form_url = form_link.or_else(|| {
        str_at(d, &["description"])
            .and_then(|t| CONTACT_FORM.find(t))
            .map(|m| m.as_str().trim_end_matches(['/', '.']).to_string())
    });
    out.contact_updated_at = str_at(c, &["public_updated_at"]).map(str::to_string);
    Some(out)
}

/// Parses an organisation: its offices joined to their contacts
/// (`details.office_contact_associations`).
pub fn parse_org(body: &[u8]) -> Result<Org> {
    let v: Value = json(body)?;
    if str_at(&v, &["document_type"]) != Some("worldwide_organisation") {
        return Err(Error::Invalid("govuk: not a worldwide organisation".into()));
    }
    let title = str_at(&v, &["title"])
        .ok_or_else(|| Error::Invalid("govuk: organisation without title".into()))?
        .to_string();
    let links = v.get("links").unwrap_or(&Value::Null);
    let contacts: Vec<&Value> = links
        .get("contacts")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let assoc: Vec<(String, String)> = v
        .pointer("/details/office_contact_associations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some((
                str_at(a, &["office_content_id"])?.to_string(),
                str_at(a, &["contact_content_id"])?.to_string(),
            ))
        })
        .collect();
    let mut offices = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for o in ["main_office", "home_page_offices"]
        .iter()
        .filter_map(|k| links.get(k).and_then(Value::as_array))
        .flatten()
    {
        let Some(id) = str_at(o, &["content_id"]) else {
            continue;
        };
        if !seen.insert(id.to_string()) || o.get("withdrawn") == Some(&Value::Bool(true)) {
            continue;
        }
        let contact = assoc
            .iter()
            .find(|(oid, _)| oid == id)
            .and_then(|(_, cid)| {
                contacts
                    .iter()
                    .copied()
                    .find(|c| str_at(c, &["content_id"]) == Some(cid))
            });
        if let Some(off) = office(o, contact) {
            offices.push(off);
        }
    }
    let world_location = links
        .get("world_locations")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|w| str_at(w, &["title"]))
        .map(str::to_string);
    Ok(Org {
        title,
        base_path: str_at(&v, &["base_path"]).unwrap_or_default().to_string(),
        world_location,
        offices,
    })
}

/// What a leased URL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Search,
    Org,
}

fn row_of(u: &CanonicalUrl) -> Option<Row> {
    let p = u.as_url().path();
    if p == SEARCH_PATH {
        Some(Row::Search)
    } else if p
        .strip_prefix(ORG_PREFIX)
        .is_some_and(|s| is_org_link(&format!("/world/organisations/{s}")))
    {
        Some(Row::Org)
    } else {
        None
    }
}

/// The URL of the next search page, if the listing has more results.
fn next_page(url: &CanonicalUrl, page: &SearchPage, n: usize) -> Option<CanonicalUrl> {
    let next = page.start + n as u64;
    if n == 0 || next >= page.total {
        return None;
    }
    let mut u = url.as_url().clone();
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| k != "start")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    u.query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("start", &next.to_string());
    CanonicalUrl::from_url(u).ok()
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct GovukWorldOrgs;

#[async_trait]
impl SourceAdapter for GovukWorldOrgs {
    fn id(&self) -> &'static str {
        "govuk_world_orgs"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let row = row_of(&url)
            .ok_or_else(|| Error::Invalid("govuk: not a search or organisation URL".into()))?;
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            conditional: row == Row::Org,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url.clone(), &opts, &mut rep).await?;
        if not_modified(&ex) {
            rep.complete = true;
            return Ok(rep);
        }
        check_status(&ex)?;
        match row {
            Row::Search => {
                let page = parse_search(&ex.body)?;
                let n = page.links.len();
                let mut urls = page
                    .links
                    .iter()
                    .take(MAX_ORGS_PER_PAGE)
                    .map(|l| on_origin(&url, &format!("/api/content{l}")))
                    .collect::<Result<Vec<_>>>()?;
                urls.sort();
                urls.dedup();
                enqueue_adapter_rows(ctx, self.id(), urls, 0.6, &mut rep).await?;
                if let Some(next) = next_page(&url, &page, n) {
                    enqueue_adapter_rows(ctx, self.id(), vec![next], 0.7, &mut rep).await?;
                }
            }
            Row::Org => {
                parse_org(&ex.body)?;
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}
