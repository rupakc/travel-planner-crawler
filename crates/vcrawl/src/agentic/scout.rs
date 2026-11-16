//! Source scout, **propose only** (spec §5.7): for under-covered `(destination, kind)` cells
//! Gemini suggests sites; automatic checks run on each suggestion and the result becomes a row
//! in `source_candidates` (and a section of `docs/proposals/<date>.md`).
//!
//! The scout never enqueues, claims or fetches: it holds no frontier, queue or fetch client, and
//! it never writes `sources`, `urls` or `work_items`. Only a human (`vcrawl admin source approve
//! <id>`) turns a proposal into a `sources/*.yaml` PR. The checks that need the network (robots
//! for VoyagerBot, the `/terms` keyword scan, feed/sitemap presence) are recorded as `pending`;
//! they run later under normal permits.
//!
//! A suggestion on the denylist (including brand domains under **any** public suffix,
//! `expedia.*`) is stored as `rejected` (`decided_by = scout:denylist`) so it is never proposed
//! again.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;
use vc_core::llm::{LlmRequest, Purpose, ThinkingLevel};
use vc_core::policy::Policy;
use vc_core::{CanonicalUrl, Result};

use super::{AgenticLlm, Called, answer_json, clip, db};

/// The system prompt.
pub const PROMPT: &str = include_str!("../../../../prompts/agentic/scout.md");

/// Suggestions kept per cell.
pub const MAX_SITES: usize = 8;

/// `source_candidates.proposed_by`.
pub const PROPOSED_BY: &str = "scout";

/// `source_candidates.decided_by` for a denylisted suggestion.
pub const DENYLIST_DECIDER: &str = "scout:denylist";

/// One under-covered cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScoutCell {
    /// City or region **with its country** (e.g. `Kyoto, Japan`).
    pub destination: String,
    /// Record kind (e.g. `event`).
    pub kind: String,
}

/// What happened to one suggestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    /// Stored as `proposed`; waits for a human.
    Proposed,
    /// On the denylist; stored as `rejected`.
    Denylisted,
    /// Already a source or a candidate; nothing written.
    Known,
    /// Not a usable host name; nothing written.
    Invalid,
}

/// One suggestion and its checks.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoutProposal {
    /// `source_candidates.candidate_id` when a row was written.
    pub candidate_id: Option<i64>,
    /// Normalised host (or the raw suggestion when invalid).
    pub host: String,
    /// Site name.
    pub name: String,
    /// Why it covers the gap (clipped to 200 characters).
    pub reason: String,
    /// The cell it was suggested for.
    pub cell: ScoutCell,
    /// Check results (`source_candidates.checks`).
    pub checks: Value,
    /// Outcome.
    pub status: ProposalStatus,
}

/// The result of [`run_scout`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScoutReport {
    /// Every suggestion and what happened to it.
    pub proposals: Vec<ScoutProposal>,
    /// Model calls made.
    pub llm_calls: usize,
    /// Stopped early (LLM disabled or budget refused).
    pub stopped: bool,
}

impl ScoutReport {
    /// Suggestions stored as `proposed`.
    pub fn proposed(&self) -> impl Iterator<Item = &ScoutProposal> {
        self.proposals
            .iter()
            .filter(|p| p.status == ProposalStatus::Proposed)
    }
}

/// The response schema: `{sites: [{host, name, reason}]}`.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sites": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "host": {"type": "string"},
                        "name": {"type": "string"},
                        "reason": {"type": "string"}
                    },
                    "required": ["host"]
                }
            }
        },
        "required": ["sites"]
    })
}

/// A bare, lowercase host from a suggestion (`https://www.x.org/a` → `www.x.org`), or `None`
/// when it is not a plain DNS name with a dot (IP literals, ports, user info are refused).
pub fn normalize_host(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_lowercase();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(&s);
    let host = s.split(['/', '?', '#']).next()?.trim_end_matches('.');
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host
            .split('.')
            .all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-'))
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        && host.parse::<std::net::IpAddr>().is_err()
        && !host
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()));
    ok.then(|| host.to_string())
}

/// `www.`-stripped form used to compare hosts.
fn bare(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

/// Hosts already covered: every `source_candidates.host` and the hosts of every source's seed
/// URLs (`www.`-stripped).
async fn known_hosts(pool: &PgPool) -> Result<HashSet<String>> {
    let rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT host FROM source_candidates
         UNION
         SELECT lower(substring(s FROM '^[a-zA-Z][a-zA-Z0-9+.-]*://([^/:?#]+)'))
         FROM sources, jsonb_array_elements_text(COALESCE(spec->'seeds', '[]'::jsonb)) AS s",
    )
    .fetch_all(pool)
    .await
    .map_err(db)?;
    Ok(rows
        .into_iter()
        .filter_map(|(h,)| h)
        .map(|h| bare(&h).to_string())
        .collect())
}

/// Asks Gemini for sites for each cell (purpose `scout`), checks them and writes proposals.
/// Never enqueues or fetches anything. With the LLM disabled it returns an empty report
/// without touching the database.
pub async fn run_scout(
    llm: &AgenticLlm,
    pool: &PgPool,
    policy: &dyn Policy,
    cells: &[ScoutCell],
) -> Result<ScoutReport> {
    let mut rep = ScoutReport::default();
    if !llm.enabled() || cells.is_empty() {
        rep.stopped = !llm.enabled();
        return Ok(rep);
    }
    let mut known = known_hosts(pool).await?;
    for cell in cells {
        let mut avoid: Vec<&String> = known.iter().collect();
        avoid.sort();
        avoid.truncate(200);
        let input = json!({
            "destination": clip(&cell.destination, 120),
            "kind": clip(&cell.kind, 40),
            "avoid": avoid,
        })
        .to_string();
        let req = LlmRequest {
            model: None,
            purpose: Purpose::Scout,
            system: PROMPT.to_string(),
            input,
            response_schema: Some(schema()),
            max_output_tokens: 1200,
            thinking: ThinkingLevel::Low,
        };
        let called = llm.call(req).await?;
        if called.stops() {
            rep.stopped = true;
            break;
        }
        rep.llm_calls += 1;
        let Called::Answer(resp) = called else {
            continue;
        };
        let Some(v) = answer_json(&resp) else {
            continue;
        };
        let sites = v.get("sites").and_then(Value::as_array).cloned();
        for site in sites.into_iter().flatten().take(MAX_SITES) {
            let raw = site.get("host").and_then(Value::as_str).unwrap_or("");
            let name = clip(site.get("name").and_then(Value::as_str).unwrap_or(""), 120);
            let reason = clip(
                site.get("reason").and_then(Value::as_str).unwrap_or(""),
                200,
            );
            let p = check_and_store(pool, policy, &mut known, cell, raw, name, reason).await?;
            rep.proposals.push(p);
        }
    }
    Ok(rep)
}

async fn check_and_store(
    pool: &PgPool,
    policy: &dyn Policy,
    known: &mut HashSet<String>,
    cell: &ScoutCell,
    raw: &str,
    name: String,
    reason: String,
) -> Result<ScoutProposal> {
    let mut p = ScoutProposal {
        candidate_id: None,
        host: clip(raw, 253),
        name,
        reason,
        cell: cell.clone(),
        checks: json!({}),
        status: ProposalStatus::Invalid,
    };
    let Some(host) = normalize_host(raw) else {
        return Ok(p);
    };
    p.host = host.clone();
    let Ok(url) = CanonicalUrl::parse(&format!("https://{host}/")) else {
        return Ok(p);
    };
    if known.contains(bare(&host)) {
        p.status = ProposalStatus::Known;
        return Ok(p);
    }
    known.insert(bare(&host).to_string());
    let deny = policy.denied(&url);
    p.checks = json!({
        "denylist": match &deny {
            Some(d) => json!({"result": "denied", "rule": d.rule, "reason": d.reason}),
            None => json!({"result": "pass"}),
        },
        "robots": {"result": "pending"},
        "terms": {"result": "pending"},
        "feeds": {"result": "pending"},
        "licence": {"result": "pending"},
        "name": p.name,
        "reason": p.reason,
    });
    let (status, decided_by) = match deny {
        Some(_) => ("rejected", Some(DENYLIST_DECIDER)),
        None => ("proposed", None),
    };
    let id: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO source_candidates
             (host, proposed_by, destination, kinds, checks, status, decided_at, decided_by)
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7::text IS NULL THEN NULL ELSE now() END, $7)
         ON CONFLICT (host) DO NOTHING
         RETURNING candidate_id",
    )
    .bind(&host)
    .bind(PROPOSED_BY)
    .bind(&cell.destination)
    .bind(vec![cell.kind.clone()])
    .bind(&p.checks)
    .bind(status)
    .bind(decided_by)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    p.candidate_id = id.map(|(i,)| i);
    p.status = match (decided_by, p.candidate_id) {
        (_, None) => ProposalStatus::Known,
        (Some(_), Some(_)) => ProposalStatus::Denylisted,
        (None, Some(_)) => ProposalStatus::Proposed,
    };
    if p.status != ProposalStatus::Proposed {
        tracing::info!(status = ?p.status, "scout suggestion not proposed");
    }
    Ok(p)
}

/// The `docs/proposals/<date>.md` text for a run: proposals waiting for review, then the
/// suggestions refused by the denylist.
pub fn render_proposals_md(date: NaiveDate, rep: &ScoutReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Source proposals, {date}\n");
    let _ = writeln!(
        s,
        "Written by the source scout. **Nothing here is crawled.** A person reviews each \
         proposal and runs `vcrawl admin source approve <id>` (which opens a `sources/*.yaml` \
         PR) or `vcrawl admin source reject <id>`. Robots, terms, feed and licence checks are \
         `pending` until they run under normal permits.\n"
    );
    let proposed: Vec<&ScoutProposal> = rep.proposed().collect();
    let _ = writeln!(s, "## Proposed ({})\n", proposed.len());
    if proposed.is_empty() {
        let _ = writeln!(s, "None.\n");
    } else {
        let _ = writeln!(s, "| id | host | name | destination | kind | why |");
        let _ = writeln!(s, "|---|---|---|---|---|---|");
        for p in proposed {
            let _ = writeln!(
                s,
                "| {} | `{}` | {} | {} | {} | {} |",
                p.candidate_id.unwrap_or_default(),
                p.host,
                md_cell(&p.name),
                md_cell(&p.cell.destination),
                md_cell(&p.cell.kind),
                md_cell(&p.reason),
            );
        }
        let _ = writeln!(s);
    }
    let denied: Vec<&ScoutProposal> = rep
        .proposals
        .iter()
        .filter(|p| p.status == ProposalStatus::Denylisted)
        .collect();
    let _ = writeln!(s, "## Refused by the denylist ({})\n", denied.len());
    if denied.is_empty() {
        let _ = writeln!(s, "None.");
    } else {
        for p in denied {
            let rule = p
                .checks
                .pointer("/denylist/rule")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let _ = writeln!(s, "- `{}` (rule `{}`)", p.host, md_cell(rule));
        }
    }
    s
}

fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Writes [`render_proposals_md`] to `<dir>/<date>.md` (appending when the file exists, so two
/// runs on one day keep both) and returns the path.
pub fn write_proposals_md(dir: &Path, date: NaiveDate, rep: &ScoutReport) -> Result<PathBuf> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{date}.md"));
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(render_proposals_md(date, rep).as_bytes())?;
    Ok(path)
}
