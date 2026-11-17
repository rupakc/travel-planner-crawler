//! `vcrawl admin source validate|list|approve|disable` (spec §5.7 scout, §5.12).
//!
//! * `validate <id>`: the static half of onboarding — `sources/<id>.yaml` parses, passes
//!   `vc_sources::validate` (schema rules, adapters, licence/source-class gates) and no seed,
//!   sitemap or feed is on `policy/denylist.yaml`. The network half (robots for VoyagerBot,
//!   3 permit-gated sample fetches) runs in the scheduler under a normal `ClaimPermit`; this
//!   command never touches the network.
//! * `list [--proposed]`: sources in the database, or open scout proposals.
//! * `approve <id>`: a source id re-enables a disabled source (refused for `restricted`
//!   licences and for specs that no longer validate); a numeric id approves a scout proposal
//!   (`source_candidates`) and prints a draft spec for the `sources/*.yaml` PR. Only a human
//!   runs this: nothing is crawled because of a proposal before that PR merges.
//! * `disable <id>`: stops scheduling a source (`sources.enabled = false`; records keep their
//!   provenance) and parks its due URLs (`url_state.state = 'disabled'`); `approve` resumes
//!   them. Both record the decision in `sources.enabled_override`, which re-seeding never
//!   overwrites: a disabled source stays disabled when its spec changes, an approved one stays
//!   enabled (unless its licence becomes restricted).

use std::path::Path;

use anyhow::Context as _;
use clap::Subcommand;
use serde::Serialize;
use sqlx::{PgPool, Row};
use vc_core::sources::SourceSpec;
use vc_sources::{ValidateOpts, Validation};

use crate::Ctx;

/// `admin source …`.
#[derive(Debug, Clone, Subcommand)]
pub enum SourceCmd {
    /// Static checks of `sources/<id>.yaml` (no network).
    Validate {
        /// Source id (file stem).
        id: String,
    },
    /// Lists sources, or open scout proposals with `--proposed`.
    List {
        /// Scout proposals awaiting a decision.
        #[arg(long)]
        proposed: bool,
    },
    /// Re-enables a source, or approves a scout proposal (numeric id).
    Approve {
        /// Source id, or `source_candidates.candidate_id`.
        id: String,
        /// Who decided (recorded on proposals).
        #[arg(long, default_value = "operator")]
        by: String,
    },
    /// Disables a source.
    Disable {
        /// Source id.
        id: String,
    },
}

/// The result of `source validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ValidateReport {
    /// Source id.
    pub id: String,
    /// No errors.
    pub ok: bool,
    /// Errors.
    pub errors: Vec<String>,
    /// Warnings.
    pub warnings: Vec<String>,
}

/// One row of `source list`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceRow {
    /// Id.
    pub source_id: String,
    /// Enabled.
    pub enabled: bool,
    /// Licence class.
    pub licence_class: String,
    /// Source class.
    pub source_class: String,
    /// Method.
    pub method: String,
    /// The admin decision that wins over the spec (`None`: the spec's `enabled`).
    pub enabled_override: Option<bool>,
}

/// One open scout proposal.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CandidateRow {
    /// Id.
    pub candidate_id: i64,
    /// Host.
    pub host: String,
    /// Status.
    pub status: String,
    /// Kinds proposed for.
    pub kinds: Vec<String>,
    /// Automatic check results.
    pub checks: serde_json::Value,
}

/// Runs the command.
pub async fn run(ctx: &Ctx, cmd: SourceCmd) -> anyhow::Result<serde_json::Value> {
    if let SourceCmd::Validate { id } = &cmd {
        let rep = validate_file(
            Path::new(&ctx.cfg.sources.dir),
            Path::new(&ctx.cfg.policy.denylist_path),
            id,
        )?;
        let v = serde_json::to_value(&rep)?;
        anyhow::ensure!(rep.ok, "source {id} is invalid: {v}");
        return Ok(v);
    }
    let pool = crate::wiring::db::ops_pool(&ctx.cfg, 4).await?;
    Ok(match cmd {
        SourceCmd::Validate { .. } => unreachable!("handled above"),
        SourceCmd::List { proposed: false } => serde_json::to_value(list(&pool).await?)?,
        SourceCmd::List { proposed: true } => serde_json::to_value(proposals(&pool).await?)?,
        SourceCmd::Approve { id, by } => approve(&pool, &id, &by).await?,
        SourceCmd::Disable { id } => {
            let urls = disable(&pool, &id).await?;
            serde_json::json!({ "source_id": id, "enabled": false, "urls_disabled": urls })
        }
    })
}

/// Static validation of `<dir>/<id>.yaml` against the adapters and the denylist.
pub fn validate_file(dir: &Path, denylist: &Path, id: &str) -> anyhow::Result<ValidateReport> {
    anyhow::ensure!(
        !id.is_empty() && !id.contains(['/', '\\', '.']),
        "source id {id:?} is not a file stem"
    );
    let spec = vc_sources::load_file(&dir.join(format!("{id}.yaml")))?;
    let dl = vc_sources::load_denylist(denylist).context("load the denylist")?;
    let adapters = vc_sources::adapters::ids();
    let v = vc_sources::validate(
        &spec,
        &ValidateOpts {
            file_stem: Some(id),
            adapters: &adapters,
            denylist: Some(&dl),
        },
    );
    Ok(report(id, v))
}

fn report(id: &str, v: Validation) -> ValidateReport {
    ValidateReport {
        id: id.to_string(),
        ok: v.is_ok(),
        errors: v.errors,
        warnings: v.warnings,
    }
}

/// Every source in the database, by id.
pub async fn list(pool: &PgPool) -> anyhow::Result<Vec<SourceRow>> {
    let rows = sqlx::query(
        "SELECT source_id, enabled, licence_class, source_class, method, enabled_override
         FROM sources
         ORDER BY source_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| SourceRow {
            source_id: r.get("source_id"),
            enabled: r.get("enabled"),
            licence_class: r.get("licence_class"),
            source_class: r.get("source_class"),
            method: r.get("method"),
            enabled_override: r.get("enabled_override"),
        })
        .collect())
}

/// Open scout proposals.
pub async fn proposals(pool: &PgPool) -> anyhow::Result<Vec<CandidateRow>> {
    let rows = sqlx::query(
        "SELECT candidate_id, host, status, kinds, checks FROM source_candidates
         WHERE status = 'proposed' ORDER BY candidate_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| CandidateRow {
            candidate_id: r.get("candidate_id"),
            host: r.get("host"),
            status: r.get("status"),
            kinds: r.get("kinds"),
            checks: r.get("checks"),
        })
        .collect())
}

/// `source approve`: a numeric id approves a proposal, anything else re-enables a source.
pub async fn approve(pool: &PgPool, id: &str, by: &str) -> anyhow::Result<serde_json::Value> {
    if let Ok(cid) = id.parse::<i64>() {
        return approve_candidate(pool, cid, by).await;
    }
    let row = sqlx::query("SELECT licence_class, spec FROM sources WHERE source_id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .with_context(|| format!("source {id} is not in the database (seed it first)"))?;
    let class: String = row.get("licence_class");
    anyhow::ensure!(
        class != "restricted",
        "source {id} has a restricted licence: it is never ingested"
    );
    let spec: SourceSpec = serde_json::from_value(row.get("spec")).context("stored spec")?;
    let adapters = vc_sources::adapters::ids();
    let v = vc_sources::validate(
        &spec,
        &ValidateOpts {
            file_stem: None,
            adapters: &adapters,
            denylist: None,
        },
    );
    anyhow::ensure!(
        v.is_ok(),
        "source {id} does not validate: {}",
        v.errors.join("; ")
    );
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE sources SET enabled = true, enabled_override = true, updated_at = now()
         WHERE source_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let urls = sqlx::query(
        "UPDATE url_state SET state = 'due' WHERE source_id = $1 AND state = 'disabled'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    tracing::info!(source = id, urls, "source approved (enabled)");
    Ok(serde_json::json!({ "source_id": id, "enabled": true, "urls_resumed": urls }))
}

async fn approve_candidate(pool: &PgPool, id: i64, by: &str) -> anyhow::Result<serde_json::Value> {
    let row = sqlx::query(
        "UPDATE source_candidates SET status = 'approved', decided_at = now(), decided_by = $2
         WHERE candidate_id = $1 AND status = 'proposed'
         RETURNING host, kinds, destination, checks",
    )
    .bind(id)
    .bind(by)
    .fetch_optional(pool)
    .await?
    .with_context(|| format!("no open proposal {id}"))?;
    let host: String = row.get("host");
    let kinds: Vec<String> = row.get("kinds");
    tracing::info!(candidate = id, "scout proposal approved");
    Ok(serde_json::json!({
        "candidate_id": id,
        "status": "approved",
        "host": host,
        "next": "complete the draft into sources/<id>.yaml, run `vcrawl admin source validate <id>`, open a PR",
        "draft_spec": {
            "id": draft_id(&host),
            "name": host,
            "category": "TODO",
            "source_class": "TODO (official|reference|editorial|community|commercial_api)",
            "licence": { "class": "TODO", "attribution": "TODO" },
            "method": "html",
            "seeds": [format!("https://{host}/")],
            "kinds": kinds,
            "cadence": "TODO",
            "enabled": false,
            "scout_checks": row.get::<serde_json::Value, _>("checks"),
            "destination": row.get::<Option<String>, _>("destination"),
        }
    }))
}

/// A source id derived from a host (`www.Example.co.uk` → `example-co-uk`).
pub fn draft_id(host: &str) -> String {
    let h = host.to_ascii_lowercase();
    let h = h.strip_prefix("www.").unwrap_or(&h);
    let mut id: String = h
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    id.truncate(63);
    id.trim_matches('-').to_string()
}

/// `source disable`: the source and its due URLs (`url_state.state = 'disabled'`, never
/// claimed). Leased and pending URLs finish normally. Returns the URLs parked.
pub async fn disable(pool: &PgPool, id: &str) -> anyhow::Result<u64> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query(
        "UPDATE sources SET enabled = false, enabled_override = false, updated_at = now()
             WHERE source_id = $1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    anyhow::ensure!(n == 1, "source {id} is not in the database");
    let urls = sqlx::query(
        "UPDATE url_state SET state = 'disabled' WHERE source_id = $1 AND state = 'due'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    tracing::info!(source = id, urls, "source disabled");
    Ok(urls)
}
