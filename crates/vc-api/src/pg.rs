//! Postgres implementations of [`ClientStore`] and [`Catalog`] (`api_clients`, `sources`,
//! counts of `records` / `entities` / `changes`).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use vc_core::index::{Item, ObservationView, ProvenanceView};
use vc_core::licence::{Licence, LicenceClass};
use vc_core::records::{ExtractionMethod, ExtractorInfo};
use vc_core::usepolicy::{IndexUse, ServeUse, UsePolicy};
use vc_core::{Error, ExtractionRunId, KindId, ModelId, Result, SourceId};

use crate::auth::{ClientStore, StoredClient};
use crate::catalog::{Catalog, FactQuery, FactRow, FactorQuery, GridQuery, OccupancyQuery};
use crate::dto::{LicenceDto, SourceDto, StatsDto};

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn parse_class(s: &str) -> Option<LicenceClass> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// The API's Postgres store.
#[derive(Clone)]
pub struct PgApiStore {
    pool: PgPool,
}

impl PgApiStore {
    /// Wraps a pool.
    pub fn new(pool: PgPool) -> Self {
        PgApiStore { pool }
    }

    /// Inserts or replaces a client (admin tooling; `token_hash` from
    /// [`crate::auth::hash_token`]).
    pub async fn put_client(&self, c: &StoredClient) -> Result<()> {
        let classes: Vec<String> = c
            .licence_classes
            .iter()
            .map(|l| l.as_str().to_string())
            .collect();
        sqlx::query(
            "INSERT INTO api_clients (client_id, name, token_hash, licence_classes, rate_per_min, enabled)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (client_id) DO UPDATE SET name = EXCLUDED.name,
               token_hash = EXCLUDED.token_hash, licence_classes = EXCLUDED.licence_classes,
               rate_per_min = EXCLUDED.rate_per_min, enabled = EXCLUDED.enabled",
        )
        .bind(&c.client_id)
        .bind(&c.name)
        .bind(&c.token_hash)
        .bind(&classes)
        .bind(i32::try_from(c.rate_per_min).unwrap_or(i32::MAX))
        .bind(c.enabled)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }
}

type ClientRow = (String, String, String, Vec<String>, i32, bool);
type SourceRow = (
    String,
    String,
    String,
    String,
    serde_json::Value,
    String,
    bool,
);

#[async_trait]
impl ClientStore for PgApiStore {
    async fn get(&self, client_id: &str) -> Result<Option<StoredClient>> {
        let row: Option<ClientRow> = sqlx::query_as(
            "SELECT client_id, name, token_hash, licence_classes, rate_per_min, enabled
             FROM api_clients WHERE client_id = $1",
        )
        .bind(client_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        Ok(row.map(
            |(client_id, name, token_hash, classes, rate, enabled)| StoredClient {
                client_id,
                name,
                token_hash,
                licence_classes: classes.iter().filter_map(|s| parse_class(s)).collect(),
                rate_per_min: u32::try_from(rate).unwrap_or(0),
                enabled,
            },
        ))
    }

    async fn touch(&self, client_id: &str) -> Result<()> {
        sqlx::query("UPDATE api_clients SET last_used_at = now() WHERE client_id = $1")
            .bind(client_id)
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }

    async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(db)?;
        Ok(())
    }
}

#[async_trait]
impl Catalog for PgApiStore {
    async fn sources(&self) -> Result<Vec<SourceDto>> {
        let rows: Vec<SourceRow> = sqlx::query_as(
            "SELECT source_id, name, category, source_class, licence, method, enabled
             FROM sources WHERE licence_class <> 'restricted' ORDER BY source_id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let mut out = Vec::with_capacity(rows.len());
        for (source_id, name, category, source_class, licence, method, enabled) in rows {
            let lic: Licence = serde_json::from_value(licence)?;
            let Some(licence) = LicenceDto::from_core(&lic) else {
                continue;
            };
            out.push(SourceDto {
                source_id,
                name,
                category,
                source_class,
                licence,
                method,
                enabled,
            });
        }
        Ok(out)
    }

    async fn stats(&self) -> Result<StatsDto> {
        let (sources_enabled, records_active, entities_active, changes_seq_max): (
            i64,
            i64,
            i64,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM sources WHERE enabled),
                    (SELECT count(*) FROM records WHERE state = 'active'),
                    (SELECT count(*) FROM entities WHERE state = 'active'),
                    (SELECT max(seq) FROM changes)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(StatsDto {
            sources_enabled,
            records_active,
            entities_active,
            changes_seq_max,
            generated_at: Utc::now(),
        })
    }

    async fn ready(&self) -> Result<()> {
        self.ping().await
    }

    async fn carbon_factors(&self, q: &FactorQuery) -> Result<Vec<Item>> {
        let classes: Vec<String> = q
            .licence_classes
            .iter()
            .filter(|c| c.ingestible())
            .map(|c| c.as_str().to_string())
            .collect();
        if classes.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT r.record_id, r.kind, r.title, r.place_ids, r.licence, r.licence_class,
                    r.conflict, r.updated_at, r.url, r.source_id,
                    v.kind_schema, v.data, v.excerpt, v.use_policy, v.captured_at, v.attribution,
                    v.extractor_ver, v.method, v.model_id, v.prompt_hash, v.extraction_run_id
             FROM records r
             JOIN record_versions v ON v.record_id = r.record_id AND v.version = r.current_version
             WHERE r.kind = 'emission_factor' AND r.state IN ('active', 'stale')
               AND r.current_version > 0 AND r.licence_class = ANY($1)
               AND v.use_policy->>'index' = 'full' AND v.use_policy->>'serve' <> 'link_only'
               AND ($2::text IS NULL OR v.data->>'category' = $2)
               AND ($3::text IS NULL OR v.data->>'country_code' = $3)
               AND ($4::text IS NULL OR v.data->>'region_code' = $4)
               AND ($5::int IS NULL OR (CASE WHEN jsonb_typeof(v.data->'year') = 'number'
                                             THEN (v.data->>'year')::numeric END) = $5)
               AND ($6::text IS NULL OR v.data->>'gas_basis' = $6)
               AND ($7::text IS NULL OR strpos(v.data->>'activity', $7) > 0)
               AND ($8::text IS NULL OR r.source_id = $8)
             ORDER BY (CASE WHEN jsonb_typeof(v.data->'year') = 'number'
                            THEN (v.data->>'year')::numeric END) DESC NULLS LAST, r.record_id
             LIMIT $9 OFFSET $10",
        )
        .bind(&classes)
        .bind(&q.category)
        .bind(&q.country)
        .bind(&q.region)
        .bind(q.year)
        .bind(&q.gas_basis)
        .bind(&q.activity)
        .bind(&q.source)
        .bind(i64::from(q.limit))
        .bind(i64::from(q.offset))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows.iter().filter_map(factor_item).collect())
    }

    async fn carbon_occupancy(&self, q: &OccupancyQuery) -> Result<Vec<Item>> {
        let classes: Vec<String> = q
            .licence_classes
            .iter()
            .filter(|c| c.ingestible())
            .map(|c| c.as_str().to_string())
            .collect();
        if classes.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT r.record_id, r.kind, r.title, r.place_ids, r.licence, r.licence_class,
                    r.conflict, r.updated_at, r.url, r.source_id,
                    v.kind_schema, v.data, v.excerpt, v.use_policy, v.captured_at, v.attribution,
                    v.extractor_ver, v.method, v.model_id, v.prompt_hash, v.extraction_run_id
             FROM records r
             JOIN record_versions v ON v.record_id = r.record_id AND v.version = r.current_version
             WHERE r.kind = 'occupancy' AND r.state IN ('active', 'stale')
               AND r.current_version > 0 AND r.licence_class = ANY($1)
               AND v.use_policy->>'index' = 'full' AND v.use_policy->>'serve' <> 'link_only'
               AND ($2::text IS NULL OR v.data->>'mode' = $2)
               AND ($3::text IS NULL OR v.data->>'country_code' = $3)
               AND ($4::int IS NULL OR (CASE WHEN jsonb_typeof(v.data->'year') = 'number'
                                             THEN (v.data->>'year')::numeric END) = $4)
               AND ($5::text IS NULL OR v.data->>'trip_purpose' = $5)
               AND ($6::text IS NULL OR v.data->>'basis' = $6)
               AND ($7::text IS NULL OR r.source_id = $7)
             ORDER BY (CASE WHEN jsonb_typeof(v.data->'year') = 'number'
                            THEN (v.data->>'year')::numeric END) DESC NULLS LAST, r.record_id
             LIMIT $8 OFFSET $9",
        )
        .bind(&classes)
        .bind(&q.mode)
        .bind(&q.country)
        .bind(q.year)
        .bind(&q.trip_purpose)
        .bind(&q.basis)
        .bind(&q.source)
        .bind(i64::from(q.limit))
        .bind(i64::from(q.offset))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows.iter().filter_map(factor_item).collect())
    }

    async fn facts(&self, q: &FactQuery) -> Result<Vec<FactRow>> {
        let classes: Vec<String> = q
            .licence_classes
            .iter()
            .filter(|c| c.ingestible())
            .map(|c| c.as_str().to_string())
            .collect();
        if classes.is_empty() {
            return Ok(Vec::new());
        }
        // `cur`: current place facts. A bare country (ISO code only, no GeoNames id or QID,
        // e.g. a Factbook fact) takes the GeoNames id and QID of the first Wikidata country
        // fact (lowest record id) of the same country. Filters apply to that resolved place.
        let rows = sqlx::query(
            "WITH cur AS (
                SELECT r.record_id, r.kind, r.title, r.place_ids, r.licence, r.licence_class,
                       r.conflict, r.updated_at, r.url, r.source_id,
                       v.kind_schema, v.data, v.excerpt, v.use_policy, v.captured_at,
                       v.attribution, v.extractor_ver, v.method, v.model_id, v.prompt_hash,
                       v.extraction_run_id
                FROM records r
                JOIN record_versions v
                  ON v.record_id = r.record_id AND v.version = r.current_version
                WHERE r.kind = 'place_fact' AND r.state IN ('active', 'stale')
                  AND r.current_version > 0
             ), served AS (
                SELECT c.*,
                       CASE WHEN c.data->'place'->>'admin_level' = 'country'
                                 AND (c.data->'place') ? 'country'
                                 AND NOT ((c.data->'place') ? 'geonames_id')
                                 AND NOT ((c.data->'place') ? 'wikidata_qid')
                            THEN COALESCE(c.data->'place' || (
                                  SELECT jsonb_strip_nulls(jsonb_build_object(
                                           'geonames_id', w.data->'place'->'geonames_id',
                                           'wikidata_qid', w.data->'place'->'wikidata_qid'))
                                  FROM cur w
                                  WHERE (w.data->'place') ? 'wikidata_qid'
                                    AND w.data->'place'->>'admin_level' = 'country'
                                    AND w.data->'place'->>'country' = c.data->'place'->>'country'
                                  ORDER BY w.record_id
                                  LIMIT 1), c.data->'place')
                            ELSE c.data->'place' END AS rplace
                FROM cur c
                WHERE c.licence_class = ANY($1)
                  AND c.use_policy->>'index' = 'full' AND c.use_policy->>'serve' <> 'link_only'
                  AND COALESCE(c.data->>'text_origin', 'templated') IN ('templated', 'rewritten')
             )
             SELECT s.*, src.name AS source_name, src.source_class AS source_class
             FROM served s
             LEFT JOIN sources src ON src.source_id = s.source_id
             WHERE s.rplace IS NOT NULL
               AND ($2::bigint IS NULL
                    OR (CASE WHEN jsonb_typeof(s.rplace->'geonames_id') = 'number'
                             THEN (s.rplace->>'geonames_id')::numeric END) = $2
                    OR $2 = ANY(s.place_ids))
               AND ($3::text IS NULL OR s.rplace->>'wikidata_qid' = $3)
               AND ($4::text IS NULL OR s.rplace->>'country' = $4)
               AND ($5::text IS NULL OR s.data->>'fact_type' = $5)
             ORDER BY s.record_id
             LIMIT $6 OFFSET $7",
        )
        .bind(&classes)
        .bind(q.geonames_id)
        .bind(&q.wikidata_qid)
        .bind(&q.country)
        .bind(&q.fact_type)
        .bind(i64::from(q.limit))
        .bind(i64::from(q.offset))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                Some(FactRow {
                    item: factor_item(row)?,
                    place: row.get("rplace"),
                    source_name: row.get("source_name"),
                    source_class: row.get("source_class"),
                })
            })
            .collect())
    }

    async fn carbon_grid(&self, q: &GridQuery) -> Result<Vec<ObservationView>> {
        let rows = sqlx::query(
            "SELECT series_key, observed_at, value, source_id, licence FROM observations
             WHERE series_key LIKE $1 AND observed_at >= $2 AND observed_at < $3
               AND NOT tombstoned
             ORDER BY observed_at DESC, series_key, source_id
             LIMIT $4",
        )
        .bind(q.series_pattern())
        .bind(q.from)
        .bind(q.to)
        .bind(i64::from(q.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows
            .iter()
            .map(|r| ObservationView {
                series_key: r.get("series_key"),
                observed_at: r.get("observed_at"),
                value: r.get("value"),
                source_id: SourceId(r.get("source_id")),
                licence: licence_or_restricted(r.get("licence"), "restricted", None),
            })
            .collect())
    }
}

/// A stored licence, else one of `class` (fail closed: unknown = `restricted`).
fn licence_or_restricted(
    v: Option<serde_json::Value>,
    class: &str,
    attribution: Option<String>,
) -> Licence {
    v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| Licence {
            class: parse_class(class).unwrap_or(LicenceClass::Restricted),
            spdx: None,
            attribution: attribution.unwrap_or_default(),
            url: None,
        })
}

fn method_of(s: &str) -> ExtractionMethod {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .unwrap_or(ExtractionMethod::Adapter)
}

/// One `emission_factor`, `occupancy` or `place_fact` row as an [`Item`]; `None` when its use policy does not allow
/// serving the data (fail closed on an unreadable policy; the SQL already filters on it so
/// that paging stays exact).
fn factor_item(row: &PgRow) -> Option<Item> {
    let policy: UsePolicy =
        serde_json::from_value(row.get("use_policy")).unwrap_or(UsePolicy::DENY_ALL);
    if policy.index != IndexUse::Full || policy.serve == ServeUse::LinkOnly {
        return None;
    }
    let class: String = row.get("licence_class");
    let id: i64 = row.get("record_id");
    Some(Item {
        id: id.to_string(),
        kind: KindId::new(row.get::<String, _>("kind").as_str()),
        schema: row.get("kind_schema"),
        title: row.get("title"),
        excerpt: if policy.serve == ServeUse::Full {
            row.get("excerpt")
        } else {
            None
        },
        data: row.get("data"),
        place_ids: row.get("place_ids"),
        licence: licence_or_restricted(row.get("licence"), &class, row.get("attribution")),
        provenance: vec![ProvenanceView {
            source_id: SourceId(row.get("source_id")),
            url: row.get::<Option<String>, _>("url").unwrap_or_default(),
            captured_at: row.get("captured_at"),
            extractor: ExtractorInfo {
                version: row.get("extractor_ver"),
                method: method_of(row.get::<String, _>("method").as_str()),
                model: row
                    .get::<Option<String>, _>("model_id")
                    .map(|m| ModelId(Arc::from(m.as_str()))),
                prompt_hash: row.get("prompt_hash"),
                run_id: row
                    .get::<Option<String>, _>("extraction_run_id")
                    .and_then(|s| s.parse::<ExtractionRunId>().ok()),
            },
        }],
        conflict: row.get("conflict"),
        use_policy: policy,
        updated_at: row.get("updated_at"),
        score: None,
    })
}
