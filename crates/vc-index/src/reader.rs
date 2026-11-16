//! `IndexReader`: hybrid search, entities, versions, the change feed and observations.
//! Only `active`/`stale` records are ever served; tombstoned and `gone` data never is.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sqlx::Row;
use sqlx::postgres::PgRow;
use vc_core::index::{
    ChangeCursor, ChangeOp, ChangeView, ChangesPage, ClaimView, EntityLookup, EntityView, Item,
    ObservationView, OwnerType, Page, ProvenanceView, RecordVersionView, SearchQuery,
};
use vc_core::licence::{Licence, LicenceClass};
use vc_core::records::ExtractorInfo;
use vc_core::usepolicy::UsePolicy;
use vc_core::{EntityId, Error, ExtractionRunId, KindId, ModelId, RecordId, Result, SourceId};

use crate::PgIndex;
use crate::util::{db, licence_class_from, method_from, servable, to_hex, vector_literal};

/// Max page size.
const MAX_LIMIT: u32 = 100;
/// How many nearest vectors feed the fusion.
const VECTOR_CANDIDATES: i64 = 200;
/// Max observation points per call.
const MAX_OBSERVATIONS: u32 = 10_000;

const SERVED: &str = "r.state IN ('active', 'stale')";

fn page_size(limit: u32) -> i64 {
    i64::from(limit.clamp(1, MAX_LIMIT))
}

fn licence_of(v: Option<Value>, class: &str, attribution: Option<String>) -> Licence {
    v.and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_else(|| Licence {
            class: licence_class_from(class),
            spdx: None,
            attribution: attribution.unwrap_or_default(),
            url: None,
        })
}

fn policy_of(v: Value) -> UsePolicy {
    // Fail closed: an unreadable policy serves nothing.
    serde_json::from_value(v).unwrap_or(UsePolicy::DENY_ALL)
}

fn extractor_of(row: &PgRow) -> ExtractorInfo {
    ExtractorInfo {
        version: row.get("extractor_ver"),
        method: method_from(row.get::<String, _>("method").as_str()),
        model: row
            .get::<Option<String>, _>("model_id")
            .map(|m| ModelId(Arc::from(m.as_str()))),
        prompt_hash: row.get("prompt_hash"),
        run_id: row
            .get::<Option<String>, _>("extraction_run_id")
            .and_then(|s| s.parse::<ExtractionRunId>().ok()),
    }
}

// ---------------------------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------------------------

fn parse_offset(cursor: Option<&str>) -> Result<i64> {
    match cursor {
        None => Ok(0),
        Some(c) => c
            .strip_prefix("o:")
            .and_then(|n| n.parse::<i64>().ok())
            .filter(|n| *n >= 0)
            .ok_or_else(|| Error::Invalid("cursor".into())),
    }
}

pub(crate) async fn search(ix: &PgIndex, q: &SearchQuery) -> Result<Page<Item>> {
    let limit = page_size(q.limit);
    let offset = parse_offset(q.cursor.as_deref())?;
    let empty = Page {
        items: vec![],
        next_cursor: None,
    };
    let classes: Vec<&str> = q
        .licence_classes
        .iter()
        .filter(|c| c.ingestible())
        .map(LicenceClass::as_str)
        .collect();
    if classes.is_empty() {
        return Ok(empty);
    }
    let text =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    let kinds: Vec<&str> = q.kinds.iter().map(|k| &*k.0).collect();
    let (near_lat, near_lon, radius_m) = match q.near {
        Some((p, km)) => (Some(p.lat), Some(p.lon), Some(km.max(0.0) * 1000.0)),
        None => (None, None, None),
    };

    // The vector leg (only with a query embedder and a text query).
    let mut vec_cte = "vec AS (SELECT NULL::bigint AS record_id, NULL::bigint AS rk
              WHERE false AND $13::text IS NULL AND $14::text IS NULL)"
        .to_string();
    let mut qvec: Option<(String, String)> = None;
    if let (Some(e), Some(t)) = (&ix.query_embedder, &text) {
        match e.embed_query(t).await {
            Ok(Some(v)) if v.len() == e.dim() => {
                let dim = e.dim();
                vec_cte = format!(
                    "vec AS (
                       SELECT record_id, row_number() OVER (ORDER BY dist, record_id) AS rk
                       FROM (SELECT c.record_id,
                                    (e.vec::halfvec({dim})) <=> ($13::text::halfvec({dim})) AS dist
                             FROM cand c
                             JOIN embeddings e ON e.owner_type = 'record'
                                 AND e.owner_id = c.record_id::text AND e.model_id = $14
                                 AND e.dim = {dim} AND e.content_hash = c.hash
                             ORDER BY dist, c.record_id LIMIT {VECTOR_CANDIDATES}) n)"
                );
                qvec = Some((vector_literal(&v)?, e.model().0.to_string()));
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(error = %err, "query embedding failed; text-only search"),
        }
    }
    let (qv, qm) = qvec.unzip();

    let sql = format!(
        "WITH cand AS (
           SELECT r.record_id, r.quality, r.last_seen, r.tsv, r.fts_config,
                  r.current_data_hash AS hash,
                  ($4::bigint IS NOT NULL AND $4::bigint = ANY(r.place_ids)) AS exact
           FROM records r
           WHERE {SERVED} AND r.current_version > 0 AND r.tsv IS NOT NULL
             AND r.licence_class = ANY($1::text[])
             AND (cardinality($2::text[]) = 0 OR r.kind = ANY($2::text[]))
             AND ($3::timestamptz IS NULL OR r.updated_at >= $3::timestamptz)
             AND (($4::bigint IS NULL AND $5::float8 IS NULL)
                  OR ($4::bigint IS NOT NULL AND $4::bigint = ANY(r.place_ids))
                  OR ($5::float8 IS NOT NULL AND r.lat IS NOT NULL
                      AND vc_haversine_m(r.lat, r.lon, $5::float8, $6::float8) <= $7::float8))
         ),
         txt AS (
           SELECT record_id, row_number() OVER (ORDER BY rank DESC, record_id) AS rk
           FROM (SELECT record_id,
                        greatest(ts_rank(tsv, websearch_to_tsquery('simple', $8::text)),
                                 ts_rank(tsv, websearch_to_tsquery(fts_config, $8::text))) AS rank
                 FROM cand
                 WHERE $8::text IS NOT NULL
                   AND (tsv @@ websearch_to_tsquery('simple', $8::text)
                        OR tsv @@ websearch_to_tsquery(fts_config, $8::text))) t
         ),
         {vec_cte},
         scored AS (
           SELECT c.record_id, c.exact,
                  (CASE WHEN $8::text IS NULL THEN 1.0 / ($9::float8 + 1)
                        ELSE coalesce(1.0 / ($9::float8 + t.rk), 0)
                           + coalesce(1.0 / ($9::float8 + v.rk), 0) END)
                  * greatest(c.quality, 0.01)
                  * power(0.5, greatest(extract(epoch FROM now() - c.last_seen), 0)
                               / 86400.0 / $10::float8) AS score
           FROM cand c
           LEFT JOIN txt t USING (record_id)
           LEFT JOIN vec v USING (record_id)
           WHERE $8::text IS NULL OR t.record_id IS NOT NULL OR v.record_id IS NOT NULL
         )
         SELECT record_id, score::float8 AS score FROM scored
         ORDER BY exact DESC, score DESC, record_id
         LIMIT $11 OFFSET $12"
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&classes)
        .bind(&kinds)
        .bind(q.updated_since)
        .bind(q.place_id)
        .bind(near_lat)
        .bind(near_lon)
        .bind(radius_m)
        .bind(text.as_deref())
        .bind(f64::from(ix.config.rrf_k))
        .bind(f64::from(ix.config.freshness_half_life_days.max(1)))
        .bind(limit + 1)
        .bind(offset)
        .bind(qv)
        .bind(qm)
        .fetch_all(&ix.pool)
        .await
        .map_err(db)?;

    let mut hits: Vec<(i64, f64)> = rows
        .iter()
        .map(|r| (r.get("record_id"), r.get("score")))
        .collect();
    let more = hits.len() as i64 > limit;
    hits.truncate(limit as usize);
    let ids: Vec<i64> = hits.iter().map(|h| h.0).collect();
    let mut items = load_records(ix, &ids).await?;
    let out = hits
        .into_iter()
        .filter_map(|(id, score)| {
            items.remove(&id).map(|mut it| {
                it.score = Some(score as f32);
                it
            })
        })
        .collect();
    Ok(Page {
        items: out,
        next_cursor: more.then(|| format!("o:{}", offset + limit)),
    })
}

/// Served records as items, keyed by id.
async fn load_records(ix: &PgIndex, ids: &[i64]) -> Result<HashMap<i64, Item>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT r.record_id, r.kind, r.title, r.place_ids, r.licence, r.licence_class,
                r.conflict, r.updated_at, r.url, r.source_id,
                v.kind_schema, v.data, v.excerpt, v.use_policy, v.captured_at, v.attribution,
                v.extractor_ver, v.method, v.model_id, v.prompt_hash, v.extraction_run_id
         FROM records r
         JOIN record_versions v ON v.record_id = r.record_id AND v.version = r.current_version
         WHERE r.record_id = ANY($1) AND {SERVED}"
    )))
    .bind(ids)
    .fetch_all(&ix.pool)
    .await
    .map_err(db)?;
    let mut out = HashMap::new();
    for row in rows {
        let id: i64 = row.get("record_id");
        let policy = policy_of(row.get("use_policy"));
        let (data, excerpt) = servable(&policy, row.get("data"), row.get("excerpt"));
        let class: String = row.get("licence_class");
        out.insert(
            id,
            Item {
                id: id.to_string(),
                kind: KindId::new(row.get::<String, _>("kind").as_str()),
                schema: row.get("kind_schema"),
                title: row.get("title"),
                excerpt,
                data,
                place_ids: row.get("place_ids"),
                licence: licence_of(row.get("licence"), &class, row.get("attribution")),
                provenance: vec![ProvenanceView {
                    source_id: SourceId(row.get("source_id")),
                    url: row.get::<Option<String>, _>("url").unwrap_or_default(),
                    captured_at: row.get("captured_at"),
                    extractor: extractor_of(&row),
                }],
                conflict: row.get("conflict"),
                use_policy: policy,
                updated_at: row.get("updated_at"),
                score: None,
            },
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------------------------

pub(crate) async fn entity(ix: &PgIndex, id: &EntityId) -> Result<EntityLookup> {
    let id_s = id.to_string();
    let ent = sqlx::query(
        "SELECT kind, name, place_ids, licence_class, licence_spdx, conflict, updated_at
         FROM entities WHERE entity_id = $1 AND state = 'active'",
    )
    .bind(&id_s)
    .fetch_optional(&ix.pool)
    .await
    .map_err(db)?;
    let Some(ent) = ent else {
        let to: Option<String> =
            sqlx::query_scalar("SELECT new_id FROM entity_redirects WHERE old_id = $1")
                .bind(&id_s)
                .fetch_optional(&ix.pool)
                .await
                .map_err(db)?;
        return Ok(match to.and_then(|t| t.parse::<EntityId>().ok()) {
            Some(t) => EntityLookup::Redirect(t),
            None => EntityLookup::NotFound,
        });
    };

    let claims = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT c.attribute, c.value, c.record_id, c.version, c.confidence
         FROM entity_claims c JOIN records r USING (record_id)
         WHERE c.entity_id = $1 AND {SERVED}
         ORDER BY c.attribute, c.confidence DESC, c.record_id DESC"
    )))
    .bind(&id_s)
    .fetch_all(&ix.pool)
    .await
    .map_err(db)?;
    if claims.is_empty() {
        return Ok(EntityLookup::NotFound);
    }
    let claims: Vec<ClaimView> = claims
        .iter()
        .map(|r| ClaimView {
            attribute: r.get("attribute"),
            value: r.get("value"),
            record_id: RecordId(r.get("record_id")),
            version: r.get("version"),
            confidence: r.get("confidence"),
        })
        .collect();
    // The best claim per attribute (claims are sorted by attribute, then confidence).
    let mut data = Map::new();
    for c in &claims {
        data.entry(c.attribute.clone())
            .or_insert_with(|| c.value.clone());
    }

    let record_ids: Vec<i64> = claims
        .iter()
        .map(|c| c.record_id.0)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut records: Vec<Item> = load_records(ix, &record_ids).await?.into_values().collect();
    records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));

    let class_s: String = ent.get("licence_class");
    let attributions: BTreeSet<&str> = records
        .iter()
        .map(|r| r.licence.attribution.as_str())
        .filter(|a| !a.is_empty())
        .collect();
    let first = records.first().map(|r| r.licence.clone());
    let same = |f: &dyn Fn(&Licence, &Licence) -> bool| {
        first
            .as_ref()
            .is_some_and(|l0| records.iter().all(|r| f(l0, &r.licence)))
    };
    let licence = Licence {
        class: licence_class_from(&class_s),
        spdx: ent
            .get::<Option<String>, _>("licence_spdx")
            .filter(|_| same(&|a, b| a.spdx == b.spdx)),
        attribution: attributions.into_iter().collect::<Vec<_>>().join("; "),
        url: first
            .as_ref()
            .and_then(|l| l.url.clone())
            .filter(|_| same(&|a, b| a.url == b.url)),
    };
    let use_policy = records
        .iter()
        .fold(UsePolicy::ALLOW_ALL, |p, r| p.meet(&r.use_policy));
    let schema = records
        .first()
        .map(|r| r.schema.clone())
        .unwrap_or_default();
    let item = Item {
        id: id_s,
        kind: KindId::new(ent.get::<String, _>("kind").as_str()),
        schema,
        title: Some(ent.get("name")),
        excerpt: None,
        data: Value::Object(data),
        place_ids: ent.get("place_ids"),
        licence,
        provenance: records.iter().flat_map(|r| r.provenance.clone()).collect(),
        conflict: ent.get("conflict"),
        use_policy,
        updated_at: ent.get("updated_at"),
        score: None,
    };
    Ok(EntityLookup::Found(Box::new(EntityView { item, claims })))
}

// ---------------------------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------------------------

pub(crate) async fn record_versions(
    ix: &PgIndex,
    id: RecordId,
    limit: u32,
    cursor: Option<String>,
) -> Result<Page<RecordVersionView>> {
    let before = match cursor.as_deref() {
        None => i32::MAX,
        Some(c) => c
            .strip_prefix("v:")
            .and_then(|n| n.parse::<i32>().ok())
            .ok_or_else(|| Error::Invalid("cursor".into()))?,
    };
    let served: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM records r WHERE r.record_id = $1 AND {SERVED})"
    )))
    .bind(id.0)
    .fetch_one(&ix.pool)
    .await
    .map_err(db)?;
    if !served {
        return Err(Error::NotFound(format!("record {id}")));
    }
    let limit = page_size(limit);
    let rows = sqlx::query(
        "SELECT version, kind_schema, data, data_hash, captured_at, extractor_ver, method,
                model_id, prompt_hash, extraction_run_id, licence_class, use_policy,
                valid_from, superseded_at, excerpt
         FROM record_versions WHERE record_id = $1 AND version < $2
         ORDER BY version DESC LIMIT $3",
    )
    .bind(id.0)
    .bind(before)
    .bind(limit + 1)
    .fetch_all(&ix.pool)
    .await
    .map_err(db)?;
    let more = rows.len() as i64 > limit;
    let items: Vec<RecordVersionView> = rows
        .iter()
        .take(limit as usize)
        .map(|r| {
            let policy = policy_of(r.get("use_policy"));
            let (data, _) = servable(&policy, r.get("data"), None);
            RecordVersionView {
                record_id: id,
                version: r.get("version"),
                schema: r.get("kind_schema"),
                data,
                data_hash: to_hex(&r.get::<Vec<u8>, _>("data_hash")),
                captured_at: r.get("captured_at"),
                extractor: extractor_of(r),
                licence_class: licence_class_from(r.get::<String, _>("licence_class").as_str()),
                use_policy: policy,
                valid_from: r.get("valid_from"),
                superseded_at: r.get("superseded_at"),
            }
        })
        .collect();
    let next_cursor = if more {
        items.last().map(|v| format!("v:{}", v.version))
    } else {
        None
    };
    Ok(Page { items, next_cursor })
}

// ---------------------------------------------------------------------------------------------
// Change feed
// ---------------------------------------------------------------------------------------------

fn owner_type(s: &str) -> Option<OwnerType> {
    Some(match s {
        "record" => OwnerType::Record,
        "entity" => OwnerType::Entity,
        "observation" => OwnerType::Observation,
        "source" => OwnerType::Source,
        _ => return None,
    })
}

fn change_op(s: &str) -> Option<ChangeOp> {
    Some(match s {
        "upsert" => ChangeOp::Upsert,
        "tombstone" => ChangeOp::Tombstone,
        "redirect" => ChangeOp::Redirect,
        _ => return None,
    })
}

pub(crate) async fn changes(
    ix: &PgIndex,
    since: Option<ChangeCursor>,
    kinds: &[KindId],
    limit: u32,
) -> Result<ChangesPage> {
    let kinds: Vec<&str> = kinds.iter().map(|k| &*k.0).collect();
    // Only transactions older than every in-flight one are visible, so a slow writer's rows can
    // never be skipped by a cursor that has already moved past later commits (spec §17.9).
    let rows = sqlx::query(
        "SELECT xid::text AS xid, seq, owner_type, owner_id, op, kind, at FROM changes
         WHERE xid < pg_snapshot_xmin(pg_current_snapshot())
           AND ($1::text IS NULL OR (xid, seq) > ($1::text::xid8, $2::bigint))
           AND (cardinality($3::text[]) = 0 OR kind = ANY($3::text[]))
         ORDER BY xid, seq
         LIMIT $4",
    )
    .bind(since.map(|c| c.xid.to_string()))
    .bind(since.map_or(0, |c| c.seq))
    .bind(&kinds)
    .bind(i64::from(limit.clamp(1, 1000)))
    .fetch_all(&ix.pool)
    .await
    .map_err(db)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let xid: String = r.get("xid");
        let cursor = ChangeCursor {
            xid: xid
                .parse()
                .map_err(|_| Error::Db("changes.xid is not a u64".into()))?,
            seq: r.get("seq"),
        };
        let (Some(ot), Some(op)) = (
            owner_type(r.get::<String, _>("owner_type").as_str()),
            change_op(r.get::<String, _>("op").as_str()),
        ) else {
            continue;
        };
        out.push(ChangeView {
            cursor,
            owner_type: ot,
            owner_id: r.get("owner_id"),
            op,
            kind: r
                .get::<Option<String>, _>("kind")
                .map(|k| KindId::new(k.as_str())),
            at: r.get("at"),
        });
    }
    let next = out.last().map(|c| c.cursor).or(since);
    Ok(ChangesPage { changes: out, next })
}

// ---------------------------------------------------------------------------------------------
// Observations
// ---------------------------------------------------------------------------------------------

pub(crate) async fn observations(
    ix: &PgIndex,
    series: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: u32,
) -> Result<Vec<ObservationView>> {
    let rows = sqlx::query(
        "SELECT series_key, observed_at, value, source_id, licence FROM observations
         WHERE series_key = $1 AND observed_at >= $2 AND observed_at < $3 AND NOT tombstoned
         ORDER BY observed_at, source_id
         LIMIT $4",
    )
    .bind(series)
    .bind(from)
    .bind(to)
    .bind(i64::from(limit.clamp(1, MAX_OBSERVATIONS)))
    .fetch_all(&ix.pool)
    .await
    .map_err(db)?;
    Ok(rows
        .iter()
        .map(|r| ObservationView {
            series_key: r.get("series_key"),
            observed_at: r.get("observed_at"),
            value: r.get("value"),
            source_id: SourceId(r.get("source_id")),
            licence: licence_of(r.get("licence"), "restricted", None),
        })
        .collect())
}
