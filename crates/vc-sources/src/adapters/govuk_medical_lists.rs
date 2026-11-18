//! `govuk_medical_lists`: the FCDO "doctors and medical facilities worldwide" lists on GOV.UK
//! (OGL v3), through the Content API.
//!
//! Three kinds of leased row, told apart by path (all conditional GETs):
//! - the collection `/api/content/government/collections/<slug>` → spool, then enqueue one row
//!   per listed publication;
//! - a publication `/api/content/government/publications/<slug>` (one per country) → spool,
//!   then enqueue its HTML attachments; the record mapping makes one link-level `article`
//!   record per publication (title, country, link, its own updated date: some lists are years
//!   old, the app shows that);
//! - an HTML attachment `/api/content/government/publications/<slug>/<attachment>` → spool;
//!   the record mapping reads its tables into `medical_facility` records, only where a table
//!   has a recognisable name column and a phone or address column.
//!
//! PDF and other file attachments are never fetched (another host); the publication record
//! links to them through its page. Follow-ups are always resolved on the leased URL's origin.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{
    Fetch, check_status, enqueue_adapter_rows, fetch_raw, json, not_modified, on_origin,
};
use super::html_text::{self, Table};
use super::phone::{self, Phone};

/// Content API prefix of collections.
pub const COLLECTION_PREFIX: &str = "/api/content/government/collections/";
/// Content API prefix of publications.
pub const PUBLICATION_PREFIX: &str = "/api/content/government/publications/";
/// GOV.UK's public origin (for web links in records).
pub const WEB_ORIGIN: &str = "https://www.gov.uk";

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn slug_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// One document a collection lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CollectionDoc {
    /// Title ("Albania: medical facilities").
    pub title: String,
    /// Content API path of the publication.
    pub api_path: String,
}

/// Parses a collection (`links.documents`, publications only).
pub fn parse_collection(body: &[u8]) -> Result<Vec<CollectionDoc>> {
    let v: Value = json(body)?;
    if str_at(&v, &["document_type"]) != Some("document_collection") {
        return Err(Error::Invalid("govuk: not a document collection".into()));
    }
    Ok(v.pointer("/links/documents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let api_path = str_at(d, &["api_path"])?;
            let slug = api_path.strip_prefix(PUBLICATION_PREFIX)?;
            slug_ok(slug).then(|| CollectionDoc {
                title: str_at(d, &["title"]).unwrap_or_default().to_string(),
                api_path: api_path.to_string(),
            })
        })
        .collect())
}

/// One attachment of a publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attachment {
    /// Title.
    pub title: String,
    /// `attachment_type` (`html`, `file`, `external`).
    pub kind: String,
    /// Its URL (a GOV.UK path for HTML attachments, an assets URL for files).
    pub url: String,
}

/// One country's medical list publication.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MedPublication {
    /// Title ("Albania: medical facilities").
    pub title: String,
    /// `base_path`.
    pub base_path: String,
    /// The country (first world location, else the title before `:`).
    pub country: Option<String>,
    /// `public_updated_at`.
    pub updated_at: Option<String>,
    /// `first_published_at`.
    pub first_published_at: Option<String>,
    /// Attachments.
    pub attachments: Vec<Attachment>,
}

/// The country a medical-list title names ("Albania: doctors" → "Albania").
pub fn country_of_title(title: &str) -> Option<String> {
    let (c, _) = title.split_once(':')?;
    Some(c.trim().to_string()).filter(|c| !c.is_empty() && c.len() <= 80)
}

/// Parses a publication.
pub fn parse_publication(body: &[u8]) -> Result<MedPublication> {
    let v: Value = json(body)?;
    if str_at(&v, &["schema_name"]) != Some("publication") {
        return Err(Error::Invalid("govuk: not a publication".into()));
    }
    let title = str_at(&v, &["title"])
        .ok_or_else(|| Error::Invalid("govuk: publication without title".into()))?
        .to_string();
    let country = v
        .pointer("/links/world_locations")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 1)
        .and_then(|a| str_at(&a[0], &["title"]))
        .map(str::to_string)
        .or_else(|| country_of_title(&title));
    let attachments = v
        .pointer("/details/attachments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| {
            Some(Attachment {
                title: str_at(a, &["title"]).unwrap_or_default().to_string(),
                kind: str_at(a, &["attachment_type"])?.to_string(),
                url: str_at(a, &["url"])?.to_string(),
            })
        })
        .collect();
    Ok(MedPublication {
        title,
        base_path: str_at(&v, &["base_path"]).unwrap_or_default().to_string(),
        country,
        updated_at: str_at(&v, &["public_updated_at"]).map(str::to_string),
        first_published_at: str_at(&v, &["first_published_at"]).map(str::to_string),
        attachments,
    })
}

/// One facility from a list table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Facility {
    /// Name as written.
    pub name: String,
    /// The heading the table appears under.
    pub section: Option<String>,
    /// Specialty or services column.
    pub specialty: Option<String>,
    /// City column.
    pub city: Option<String>,
    /// Address column.
    pub address: Option<String>,
    /// Phones.
    pub phones: Vec<Phone>,
    /// Emails.
    pub emails: Vec<String>,
    /// Website (a link in the name or website column).
    pub website: Option<String>,
}

/// An HTML list attachment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MedList {
    /// Title ("List of medical facilities in Albania").
    pub title: String,
    /// `base_path`.
    pub base_path: String,
    /// The parent publication's title ("Albania: medical facilities").
    pub parent_title: Option<String>,
    /// `public_updated_at`.
    pub updated_at: Option<String>,
    /// Facilities from the tables that could be read.
    pub facilities: Vec<Facility>,
}

#[derive(Default)]
struct Cols {
    name: Option<usize>,
    phone: Option<usize>,
    address: Option<usize>,
    email: Option<usize>,
    web: Option<usize>,
    city: Option<usize>,
    specialty: Option<usize>,
}

fn columns(headers: &[String]) -> Option<Cols> {
    let mut c = Cols::default();
    let find = |c: &Cols, words: &[&str]| {
        headers.iter().enumerate().find_map(|(i, h)| {
            let used = [
                c.name,
                c.phone,
                c.address,
                c.email,
                c.web,
                c.city,
                c.specialty,
            ]
            .contains(&Some(i));
            (!used && words.iter().any(|w| h.contains(w))).then_some(i)
        })
    };
    c.email = find(&c, &["email", "e-mail"]);
    c.web = find(&c, &["website", "web site", "web"]);
    c.phone = find(&c, &["tel", "phone", "contact number", "number"]);
    c.address = find(&c, &["address", "location"]);
    c.city = find(&c, &["city", "town"]);
    c.specialty = find(&c, &["special", "service", "type"]);
    c.name = find(
        &c,
        &[
            "name",
            "facility",
            "hospital",
            "clinic",
            "doctor",
            "practitioner",
            "institution",
            "provider",
        ],
    );
    (c.name.is_some() && (c.phone.is_some() || c.address.is_some())).then_some(c)
}

fn http_link(links: Vec<String>) -> Option<String> {
    links.into_iter().find(|l| {
        (l.starts_with("https://") || l.starts_with("http://"))
            && !l.contains(char::is_whitespace)
            && l.len() <= 2048
    })
}

fn facilities(t: &Table) -> Vec<Facility> {
    let Some(c) = columns(&t.headers) else {
        return Vec::new();
    };
    let get = |row: &[html_text::Cell], i: Option<usize>| -> Option<html_text::Cell> {
        i.and_then(|i| row.get(i)).cloned()
    };
    let one = |cell: Option<html_text::Cell>| -> Option<String> {
        cell.map(|c| c.text.lines().collect::<Vec<_>>().join(", "))
            .filter(|s| !s.trim().is_empty())
    };
    let mut out = Vec::new();
    for row in &t.rows {
        let Some(name_cell) = get(row, c.name) else {
            continue;
        };
        let name = name_cell
            .text
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        if name.is_empty() || name.chars().count() > 200 {
            continue;
        }
        let phones: Vec<Phone> = get(row, c.phone)
            .map(|p| p.text.lines().flat_map(phone::parse).collect())
            .unwrap_or_default();
        let mut emails: Vec<String> = Vec::new();
        for cell in [get(row, c.email), Some(name_cell.clone())]
            .into_iter()
            .flatten()
        {
            for h in html_text::hrefs(&cell.html) {
                if let Some(m) = h.strip_prefix("mailto:") {
                    let m = m.trim().to_string();
                    if m.contains('@') && !m.contains(char::is_whitespace) && !emails.contains(&m) {
                        emails.push(m);
                    }
                }
            }
            if cell.html == name_cell.html {
                continue;
            }
            for w in cell.text.split_whitespace() {
                let w = w.trim_matches(|ch: char| ch == ',' || ch == ';');
                if w.contains('@') && !w.starts_with('@') && !emails.contains(&w.to_string()) {
                    emails.push(w.to_string());
                }
            }
        }
        emails.truncate(3);
        let website = http_link(html_text::hrefs(&name_cell.html)).or_else(|| {
            get(row, c.web).and_then(|w| {
                http_link(html_text::hrefs(&w.html)).or_else(|| {
                    let t = w.text.trim();
                    (t.starts_with("https://") || t.starts_with("http://"))
                        .then(|| t.to_string())
                        .filter(|t| !t.contains(char::is_whitespace))
                })
            })
        });
        out.push(Facility {
            name,
            section: t.section.clone(),
            specialty: one(get(row, c.specialty)),
            city: one(get(row, c.city)),
            address: one(get(row, c.address)),
            phones: phones.into_iter().take(6).collect(),
            emails,
            website,
        });
    }
    out
}

/// Parses an HTML list attachment.
pub fn parse_list(body: &[u8]) -> Result<MedList> {
    let v: Value = json(body)?;
    if str_at(&v, &["schema_name"]) != Some("html_publication") {
        return Err(Error::Invalid("govuk: not an HTML publication".into()));
    }
    let html = str_at(&v, &["details", "body"]).unwrap_or_default();
    Ok(MedList {
        title: str_at(&v, &["title"]).unwrap_or_default().to_string(),
        base_path: str_at(&v, &["base_path"]).unwrap_or_default().to_string(),
        parent_title: v
            .pointer("/links/parent")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|p| str_at(p, &["title"]))
            .map(str::to_string),
        updated_at: str_at(&v, &["public_updated_at"]).map(str::to_string),
        facilities: html_text::tables(html)
            .iter()
            .flat_map(facilities)
            .collect(),
    })
}

/// What a leased URL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// A collection.
    Collection,
    /// A publication (one per country).
    Publication,
    /// An HTML attachment.
    List,
}

/// The kind of a leased URL, by path.
pub fn row_of(u: &CanonicalUrl) -> Option<Row> {
    let p = u.as_url().path().trim_end_matches('/');
    if let Some(s) = p.strip_prefix(COLLECTION_PREFIX) {
        return slug_ok(s).then_some(Row::Collection);
    }
    let rest = p.strip_prefix(PUBLICATION_PREFIX)?;
    match rest.split_once('/') {
        None => slug_ok(rest).then_some(Row::Publication),
        Some((a, b)) => (slug_ok(a) && slug_ok(b)).then_some(Row::List),
    }
}

/// The adapter.
#[derive(Debug, Clone, Copy, Default)]
pub struct GovukMedicalLists;

#[async_trait]
impl SourceAdapter for GovukMedicalLists {
    fn id(&self) -> &'static str {
        "govuk_medical_lists"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let mut rep = AdapterReport::default();
        let url = ctx.url.url.clone();
        let row = row_of(&url).ok_or_else(|| {
            Error::Invalid("govuk: not a collection, publication or attachment URL".into())
        })?;
        let opts = Fetch {
            accept: Some("application/json"),
            hint: Some(MediaType::Json),
            conditional: true,
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url.clone(), &opts, &mut rep).await?;
        if not_modified(&ex) {
            rep.complete = true;
            return Ok(rep);
        }
        check_status(&ex)?;
        match row {
            Row::Collection => {
                let docs = parse_collection(&ex.body)?;
                let mut urls = docs
                    .iter()
                    .map(|d| on_origin(&url, &d.api_path))
                    .collect::<Result<Vec<_>>>()?;
                urls.sort();
                urls.dedup();
                enqueue_adapter_rows(ctx, self.id(), urls, 0.6, &mut rep).await?;
            }
            Row::Publication => {
                let p = parse_publication(&ex.body)?;
                let own = format!("{}/", p.base_path.trim_end_matches('/'));
                let mut urls = p
                    .attachments
                    .iter()
                    .filter(|a| a.kind == "html" && a.url.starts_with(&own))
                    .map(|a| on_origin(&url, &format!("/api/content{}", a.url)))
                    .collect::<Result<Vec<_>>>()?;
                urls.sort();
                urls.dedup();
                enqueue_adapter_rows(ctx, self.id(), urls, 0.6, &mut rep).await?;
            }
            Row::List => {
                parse_list(&ex.body)?;
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}
