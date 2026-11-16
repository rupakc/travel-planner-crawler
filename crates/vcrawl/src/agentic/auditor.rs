//! Quality auditor (spec §5.7): samples current record versions, has Gemini check each one
//! against the stored page text (purpose `auditor`), writes the verdict to `audits`, lowers the
//! record's quality on a failure, and recomputes the per-source `quality_score` (the mean
//! audit score of the source over the last [`AuditSettings::window_days`]). Sources whose score
//! falls below [`AuditSettings::alert_below`] are reported for alerting.

use std::collections::BTreeSet;
use std::future::Future;

use serde_json::{Value, json};
use sqlx::PgPool;
use vc_core::Result;
use vc_core::llm::{LlmRequest, Purpose, ThinkingLevel};

use super::{AgenticLlm, Called, answer_json, clip, db};

/// The system prompt.
pub const PROMPT: &str = include_str!("../../../../prompts/agentic/auditor.md");

/// The page text of a capture (`vc-store` text blobs in production).
pub trait AuditText: Send + Sync {
    /// The extracted text of `capture_id`, or `None` when it is gone (purged, rolled away).
    fn text(&self, capture_id: &str) -> impl Future<Output = Result<Option<String>>> + Send;
}

/// Auditor settings.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditSettings {
    /// Record versions sampled per run (spec: 100).
    pub sample: u32,
    /// Page text characters sent per audit.
    pub max_text_chars: usize,
    /// Days of audits averaged into a source's `quality_score`.
    pub window_days: i32,
    /// A source below this score is reported for alerting.
    pub alert_below: f32,
}

impl Default for AuditSettings {
    fn default() -> Self {
        AuditSettings {
            sample: 100,
            max_text_chars: 12_000,
            window_days: 90,
            alert_below: 0.6,
        }
    }
}

/// `audits.verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Fully supported.
    Ok,
    /// Small, harmless problems.
    Minor,
    /// Contradicts the page.
    Wrong,
    /// Not in the page at all.
    Unsupported,
}

impl Verdict {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Verdict::Ok,
            "minor" => Verdict::Minor,
            "wrong" => Verdict::Wrong,
            "unsupported" => Verdict::Unsupported,
            _ => return None,
        })
    }

    /// The stable name (`audits.verdict`).
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Minor => "minor",
            Verdict::Wrong => "wrong",
            Verdict::Unsupported => "unsupported",
        }
    }

    /// A failure lowers the record's quality.
    pub fn is_failure(self) -> bool {
        matches!(self, Verdict::Wrong | Verdict::Unsupported)
    }

    /// The score band the model's score is clamped into, so a verdict and its score agree.
    fn band(self) -> (f32, f32) {
        match self {
            Verdict::Ok => (0.8, 1.0),
            Verdict::Minor => (0.5, 0.9),
            Verdict::Wrong | Verdict::Unsupported => (0.0, 0.4),
        }
    }
}

/// One audited record version.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditResult {
    /// Record.
    pub record_id: i64,
    /// Version.
    pub version: i32,
    /// Source.
    pub source_id: String,
    /// Verdict.
    pub verdict: Verdict,
    /// Score in `[0, 1]`.
    pub score: f32,
}

/// The result of [`run_auditor`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditReport {
    /// Versions sampled.
    pub sampled: usize,
    /// Audits written.
    pub audits: Vec<AuditResult>,
    /// Versions skipped because their text is gone.
    pub no_text: usize,
    /// Model calls made.
    pub llm_calls: usize,
    /// Stopped early (LLM disabled or budget refused).
    pub stopped: bool,
    /// `(source_id, new quality_score)` of every source audited this run.
    pub source_scores: Vec<(String, f32)>,
    /// Sources whose score is below [`AuditSettings::alert_below`].
    pub below_threshold: Vec<String>,
}

/// The response schema: `{verdict, score, problems}`.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "verdict": {"type": "string", "enum": ["ok", "minor", "wrong", "unsupported"]},
            "score": {"type": "number"},
            "problems": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["verdict", "score"]
    })
}

type SampleRow = (i64, i32, String, String, Value, String);

/// Audits a sample of current record versions. With the LLM disabled it returns an empty
/// report without reading or writing anything.
pub async fn run_auditor<T: AuditText>(
    llm: &AgenticLlm,
    pool: &PgPool,
    text: &T,
    settings: &AuditSettings,
) -> Result<AuditReport> {
    let mut rep = AuditReport::default();
    if !llm.enabled() {
        rep.stopped = true;
        return Ok(rep);
    }
    let rows: Vec<SampleRow> = sqlx::query_as(
        "SELECT rv.record_id, rv.version, r.source_id, r.kind, rv.data, rv.capture_id
         FROM record_versions rv JOIN records r USING (record_id)
         WHERE rv.superseded_at IS NULL AND r.state = 'active'
           AND rv.version = r.current_version
         ORDER BY random() LIMIT $1",
    )
    .bind(i64::from(settings.sample))
    .fetch_all(pool)
    .await
    .map_err(db)?;
    rep.sampled = rows.len();
    for (record_id, version, source_id, kind, data, capture_id) in rows {
        let Some(page) = text.text(&capture_id).await? else {
            rep.no_text += 1;
            continue;
        };
        let input = json!({
            "kind": kind,
            "record": data,
            "page_text": clip(&page, settings.max_text_chars),
        })
        .to_string();
        let req = LlmRequest {
            model: None,
            purpose: Purpose::Auditor,
            system: PROMPT.to_string(),
            input,
            response_schema: Some(schema()),
            max_output_tokens: 300,
            thinking: ThinkingLevel::Minimal,
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
        let Some(verdict) = v
            .get("verdict")
            .and_then(Value::as_str)
            .and_then(Verdict::parse)
        else {
            continue;
        };
        let (lo, hi) = verdict.band();
        let raw = v
            .get("score")
            .and_then(Value::as_f64)
            .unwrap_or(f64::from(hi)) as f32;
        let score = if raw.is_finite() {
            raw.clamp(lo, hi)
        } else {
            hi
        };
        let problems: Vec<String> = v
            .get("problems")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .take(5)
            .map(|p| clip(p, 120))
            .collect();
        let mut tx = pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO audits (record_id, version, source_id, verdict, score, notes, model)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(record_id)
        .bind(version)
        .bind(&source_id)
        .bind(verdict.as_str())
        .bind(score)
        .bind(json!({"problems": problems}))
        .bind(resp.model.0.as_ref())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        if verdict.is_failure() {
            sqlx::query(
                "UPDATE records SET quality = LEAST(quality, $2), updated_at = now()
                 WHERE record_id = $1",
            )
            .bind(record_id)
            .bind(score)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
            sqlx::query(
                "UPDATE record_versions SET quality = LEAST(quality, $3)
                 WHERE record_id = $1 AND version = $2",
            )
            .bind(record_id)
            .bind(version)
            .bind(score)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        }
        tx.commit().await.map_err(db)?;
        rep.audits.push(AuditResult {
            record_id,
            version,
            source_id,
            verdict,
            score,
        });
    }
    let sources: BTreeSet<&str> = rep.audits.iter().map(|a| a.source_id.as_str()).collect();
    for source_id in sources {
        let row: Option<(f32,)> = sqlx::query_as(
            "UPDATE sources SET quality_score = sub.q, updated_at = now()
             FROM (SELECT avg(score)::real AS q FROM audits
                   WHERE source_id = $1 AND created_at > now() - make_interval(days => $2)) sub
             WHERE source_id = $1 AND sub.q IS NOT NULL
             RETURNING quality_score",
        )
        .bind(source_id)
        .bind(settings.window_days)
        .fetch_optional(pool)
        .await
        .map_err(db)?;
        if let Some((q,)) = row {
            rep.source_scores.push((source_id.to_string(), q));
            if q < settings.alert_below {
                rep.below_threshold.push(source_id.to_string());
            }
        }
    }
    if !rep.below_threshold.is_empty() {
        tracing::warn!(
            sources = rep.below_threshold.len(),
            "sources below the audit quality threshold"
        );
    }
    Ok(rep)
}
