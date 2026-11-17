//! Stale records (spec §5.6 curation states): a record previously extracted from a page that a
//! newer capture of the page no longer contains is marked `stale` (still served, never
//! deleted), with provenance (`stale_since`, `stale_capture_id`, `stale_captured_at`,
//! migration 1902) and a `changes` row. A later capture that contains it again re-activates it.
//!
//! Absence is only evidence when this capture's extraction could have produced the record:
//! [`Coverage`] says which (kind, method) pairs were really extracted. Structured records count
//! when the kind is in the source's structured scope; LLM records only when the model actually
//! ran for that kind (an unchanged payload, a disabled LLM, the budget or a failed call never
//! stale anything). A kind with records dropped by validation, grounding or quality this time
//! is not judged at all, and an older capture never stales a record a newer one saw.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use vc_core::records::{ExtractionMethod, Provenance};
use vc_core::{Error, KindId, Result};
use vc_extract::{ExtractOutput, Skip};

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// Which (kind, method) pairs this capture's extraction covered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    nothing: bool,
    structured_all: bool,
    structured: HashSet<String>,
    llm: HashSet<String>,
    blocked: HashSet<String>,
}

/// Skips meaning the LLM did not (successfully) run for a kind.
const LLM_NOT_RUN: &[Skip] = &[
    Skip::UnknownKind,
    Skip::SourceClass,
    Skip::Unchanged,
    Skip::UsePolicy,
    Skip::LlmDisabled,
    Skip::Budget,
    Skip::LlmError,
    Skip::NoText,
    Skip::NotJson,
];

/// Skips meaning a record of the kind was on the page but dropped.
const DROPPED: &[Skip] = &[
    Skip::SchemaInvalid,
    Skip::GroundingInvalid,
    Skip::LowQuality,
    Skip::TooMany,
];

impl Coverage {
    /// Nothing is evidence: no record is staled. For one batch of a dump (every batch shares
    /// the dump's URL, so a record missing from this one is in another) and for adapter output
    /// that did not parse.
    pub fn none() -> Self {
        Coverage {
            nothing: true,
            ..Coverage::default()
        }
    }

    /// The coverage of one extraction with the source's `structured_kinds` (empty = any) and
    /// `llm_kinds`.
    pub fn of(structured_kinds: &[KindId], llm_kinds: &[KindId], out: &ExtractOutput) -> Self {
        let skipped = &out.report.skipped;
        if skipped
            .iter()
            .any(|(k, w)| &*k.0 == "*" || *w == Skip::RestrictedLicence)
        {
            return Coverage {
                nothing: true,
                ..Coverage::default()
            };
        }
        let has =
            |k: &KindId, set: &[Skip]| skipped.iter().any(|(sk, w)| sk == k && set.contains(w));
        let structured_out: HashSet<&KindId> = out
            .records
            .iter()
            .chain(&out.pending)
            .filter(|r| r.extractor.method != ExtractionMethod::Llm)
            .map(|r| &r.kind)
            .collect();
        Coverage {
            nothing: false,
            structured_all: structured_kinds.is_empty(),
            structured: structured_kinds.iter().map(|k| k.0.to_string()).collect(),
            llm: llm_kinds
                .iter()
                .filter(|k| !has(k, LLM_NOT_RUN) && !structured_out.contains(k))
                .map(|k| k.0.to_string())
                .collect(),
            blocked: skipped
                .iter()
                .filter(|(_, w)| DROPPED.contains(w))
                .map(|(k, _)| k.0.to_string())
                .collect(),
        }
    }

    /// Whether a missing record of `kind` whose current version was extracted by `method`
    /// (`structured` | `adapter` | `llm`) is evidence that it left the page.
    pub fn covers(&self, kind: &str, method: &str) -> bool {
        if self.nothing || self.blocked.contains(kind) {
            return false;
        }
        match method {
            "structured" | "adapter" => self.structured_all || self.structured.contains(kind),
            "llm" => self.llm.contains(kind),
            _ => false,
        }
    }
}

/// What [`reconcile`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaleReport {
    /// Records marked stale.
    pub staled: u64,
    /// Stale records seen again and re-activated.
    pub unstaled: u64,
}

/// `(record_id, kind, record_key, state, method, last_captured_at, stale_captured_at)`.
type Row = (
    i64,
    String,
    String,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

/// Marks this page's records (same URL and source) that the capture `prov` no longer contains
/// as stale, and re-activates stale ones it contains again (`present` = (kind, record key) of
/// every record extracted from it, held ones included). One transaction; idempotent.
pub async fn reconcile(
    pool: &PgPool,
    prov: &Provenance,
    present: &HashSet<(String, String)>,
    cov: &Coverage,
) -> Result<StaleReport> {
    let at = prov.capture.captured_at;
    let mut rep = StaleReport::default();
    let mut tx = pool.begin().await.map_err(db)?;
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT r.record_id, r.kind, r.record_key, r.state, v.method, r.last_captured_at,
                r.stale_captured_at
         FROM records r
         LEFT JOIN record_versions v
           ON v.record_id = r.record_id AND v.version = r.current_version
              AND v.superseded_at IS NULL
         WHERE r.url_key = $1 AND r.source_id = $2 AND r.state IN ('active', 'stale')
         ORDER BY r.record_id
         FOR UPDATE OF r",
    )
    .bind(prov.url_key.as_bytes().as_slice())
    .bind(&prov.source_id.0)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;
    for (id, kind, key, state, method, last, stale_at) in rows {
        let seen = present.contains(&(kind.clone(), key));
        if seen {
            let Some(stale_at) = stale_at else { continue };
            if stale_at > at {
                continue; // a newer capture found it missing
            }
            sqlx::query(
                "UPDATE records SET state = 'active', stale_since = NULL,
                    stale_capture_id = NULL, stale_captured_at = NULL, updated_at = now()
                 WHERE record_id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            if state == "stale" {
                change(&mut tx, id, &kind).await?;
                rep.unstaled += 1;
            }
        } else if state == "active"
            && last.is_none_or(|l| l < at)
            && method.as_deref().is_some_and(|m| cov.covers(&kind, m))
        {
            sqlx::query(
                "UPDATE records SET state = 'stale', stale_since = now(),
                    stale_capture_id = $2, stale_captured_at = $3, updated_at = now()
                 WHERE record_id = $1",
            )
            .bind(id)
            .bind(prov.capture.capture_id.to_string())
            .bind(at)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            change(&mut tx, id, &kind).await?;
            rep.staled += 1;
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(rep)
}

/// A `changes` row for a record whose state changed (clients re-read it).
async fn change(c: &mut sqlx::PgConnection, id: i64, kind: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO changes (owner_type, owner_id, op, kind) VALUES ('record', $1, 'upsert', $2)",
    )
    .bind(id.to_string())
    .bind(kind)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(skips: &[(&str, Skip)]) -> ExtractOutput {
        let mut o = ExtractOutput::default();
        o.report.skipped = skips.iter().map(|(k, w)| (KindId::new(k), *w)).collect();
        o
    }

    fn k(s: &[&str]) -> Vec<KindId> {
        s.iter().map(|x| KindId::new(x)).collect()
    }

    #[test]
    fn structured_scope_decides_structured_records() {
        let c = Coverage::of(&k(&["event"]), &[], &out(&[]));
        assert!(c.covers("event", "structured"));
        assert!(c.covers("event", "adapter"));
        assert!(
            !c.covers("place", "structured"),
            "outside the source's scope"
        );
        assert!(!c.covers("event", "llm"), "no LLM kinds");
        let any = Coverage::of(&[], &[], &out(&[]));
        assert!(any.covers("place", "structured"), "empty scope = any kind");
    }

    #[test]
    fn llm_records_only_when_the_model_ran() {
        let ran = Coverage::of(&[], &k(&["event"]), &out(&[]));
        assert!(ran.covers("event", "llm"));
        for why in [
            Skip::Unchanged,
            Skip::LlmDisabled,
            Skip::Budget,
            Skip::LlmError,
            Skip::UsePolicy,
            Skip::NoText,
            Skip::NotJson,
        ] {
            let c = Coverage::of(&[], &k(&["event"]), &out(&[("event", why)]));
            assert!(!c.covers("event", "llm"), "{why:?}");
        }
    }

    #[test]
    fn dropped_records_and_restricted_block_judgement() {
        for why in [
            Skip::SchemaInvalid,
            Skip::GroundingInvalid,
            Skip::LowQuality,
            Skip::TooMany,
        ] {
            let c = Coverage::of(&[], &[], &out(&[("event", why)]));
            assert!(!c.covers("event", "structured"), "{why:?}");
            assert!(c.covers("place", "structured"), "other kinds unaffected");
        }
        let c = Coverage::of(&[], &[], &out(&[("*", Skip::RestrictedLicence)]));
        assert!(!c.covers("event", "structured"));
        assert!(!c.covers("event", "unknown-method"));
    }
}
