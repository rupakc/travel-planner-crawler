//! Entity resolution (spec §5.6): v1 rule = same kind + shared place + normalised-name trigram
//! similarity ≥ `merge_min_trigram` + distance ≤ `merge_max_distance_m`, and only between
//! licence-compatible records (§5.9): the same licence class, and for `share_alike` the same
//! SPDX licence (CC BY-SA and ODbL never mix). Anything else stays a separate entity.

use serde_json::Value;
use sqlx::{PgConnection, Row};
use vc_config::IndexConfig;
use vc_core::licence::{Licence, LicenceClass};
use vc_core::{EntityId, Error, Result};

use crate::PgIndex;
use crate::sink::{change, lock_kinds};
use crate::util::{db, norm_name};

/// What [`link`] needs to know about one record version.
pub(crate) struct LinkInput<'a> {
    pub record_id: i64,
    pub version: i32,
    pub kind: &'a str,
    pub name: Option<&'a str>,
    pub place_ids: &'a [i64],
    pub point: Option<(f64, f64)>,
    pub licence: &'a Licence,
    pub data: &'a Value,
    pub quality: f32,
}

/// The SPDX id that must match for a merge (`share_alike` only).
fn spdx_key(l: &Licence) -> Option<&str> {
    match l.class {
        LicenceClass::ShareAlike => Some(l.spdx.as_deref().unwrap_or("")),
        _ => None,
    }
}

/// Links a record version to its entity (creating or merging as the rule says). Returns the
/// number of `changes` rows written.
pub(crate) async fn link(
    c: &mut PgConnection,
    cfg: &IndexConfig,
    r: &LinkInput<'_>,
) -> Result<u32> {
    let Some(name) = r.name else { return Ok(0) };
    let norm = norm_name(name);
    let class = r.licence.class.as_str();
    let spdx = spdx_key(r.licence);

    let existing: Option<String> = sqlx::query_scalar(
        "SELECT e.entity_id FROM entity_claims c JOIN entities e USING (entity_id)
         WHERE c.record_id = $1 AND e.state = 'active'
         ORDER BY e.created_at, e.entity_id LIMIT 1",
    )
    .bind(r.record_id)
    .fetch_optional(&mut *c)
    .await
    .map_err(db)?;

    let mut candidates: Vec<String> = match r.point {
        Some((lat, lon)) if !r.place_ids.is_empty() && !norm.is_empty() => sqlx::query_scalar(
            "SELECT entity_id FROM entities
             WHERE kind = $1 AND state = 'active' AND place_ids && $2
               AND similarity(norm_name, $3) >= $4
               AND lat IS NOT NULL AND lon IS NOT NULL
               AND vc_haversine_m(lat, lon, $5, $6) <= $7
               AND licence_class = $8
               AND ($9::text IS NULL OR coalesce(licence_spdx, '') = $9)
             ORDER BY created_at, entity_id",
        )
        .bind(r.kind)
        .bind(r.place_ids)
        .bind(&norm)
        .bind(cfg.merge_min_trigram)
        .bind(lat)
        .bind(lon)
        .bind(cfg.merge_max_distance_m)
        .bind(class)
        .bind(spdx)
        .fetch_all(&mut *c)
        .await
        .map_err(db)?,
        _ => Vec::new(),
    };
    let mut changes = 0;

    // The survivor is the oldest of the record's own entity and every matching entity.
    if let Some(e) = &existing
        && !candidates.contains(e)
    {
        candidates.push(e.clone());
    }
    let target = if candidates.is_empty() {
        let id = EntityId::new().to_string();
        let (lat, lon) = r.point.unzip();
        sqlx::query(
            "INSERT INTO entities (entity_id, kind, name, norm_name, place_ids, lat, lon,
                                   licence_class, licence_spdx)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(r.kind)
        .bind(name)
        .bind(&norm)
        .bind(r.place_ids)
        .bind(lat)
        .bind(lon)
        .bind(class)
        .bind(r.licence.spdx.as_deref())
        .execute(&mut *c)
        .await
        .map_err(db)?;
        id
    } else {
        let oldest: String = sqlx::query_scalar(
            "SELECT entity_id FROM entities WHERE entity_id = ANY($1)
             ORDER BY created_at, entity_id LIMIT 1",
        )
        .bind(&candidates)
        .fetch_one(&mut *c)
        .await
        .map_err(db)?;
        oldest
    };

    // The record's claims (one per top-level attribute) move to / refresh on the target.
    sqlx::query("DELETE FROM entity_claims WHERE record_id = $1 AND entity_id <> $2")
        .bind(r.record_id)
        .bind(&target)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    let attrs: Vec<(String, Value)> = match r.data {
        Value::Object(m) => m.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        other => vec![("value".to_string(), other.clone())],
    };
    let names: Vec<String> = attrs.iter().map(|(k, _)| k.clone()).collect();
    sqlx::query(
        "DELETE FROM entity_claims WHERE record_id = $1 AND entity_id = $2
           AND NOT (attribute = ANY($3))",
    )
    .bind(r.record_id)
    .bind(&target)
    .bind(&names)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    for (attr, value) in &attrs {
        sqlx::query(
            "INSERT INTO entity_claims (entity_id, attribute, record_id, version, value, confidence)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (entity_id, attribute, record_id) DO UPDATE SET
                version = EXCLUDED.version, value = EXCLUDED.value,
                confidence = EXCLUDED.confidence",
        )
        .bind(&target)
        .bind(attr)
        .bind(r.record_id)
        .bind(r.version)
        .bind(value)
        .bind(r.quality.clamp(0.0, 1.0))
        .execute(&mut *c)
        .await
        .map_err(db)?;
    }

    for other in candidates.iter().filter(|e| **e != target) {
        changes += merge(c, other, &target).await?;
    }
    // The record's previous entity, when it was not among the candidates, may now be empty.
    if let Some(e) = existing.filter(|e| *e != target && !candidates.contains(e)) {
        changes += refresh(c, &e).await?;
    }
    changes += refresh(c, &target).await?;
    Ok(changes)
}

/// Recomputes an entity's aggregates from its live claims; tombstones it when none are left.
/// Returns the number of `changes` rows written (always 1 for an existing entity).
async fn refresh(c: &mut PgConnection, entity_id: &str) -> Result<u32> {
    let row = sqlx::query(
        "WITH live AS (
            SELECT DISTINCT r.record_id, r.place_ids, r.conflict
            FROM entity_claims c JOIN records r USING (record_id)
            WHERE c.entity_id = $1 AND r.state IN ('active', 'stale'))
         UPDATE entities e SET
            place_ids = coalesce((SELECT array_agg(DISTINCT p ORDER BY p)
                                  FROM live, unnest(live.place_ids) p), e.place_ids),
            conflict = coalesce((SELECT bool_or(conflict) FROM live), false),
            state = CASE WHEN EXISTS (SELECT 1 FROM live) THEN 'active' ELSE 'tombstoned' END,
            updated_at = now()
         WHERE e.entity_id = $1
         RETURNING e.state, e.kind",
    )
    .bind(entity_id)
    .fetch_optional(&mut *c)
    .await
    .map_err(db)?;
    let Some(row) = row else { return Ok(0) };
    let state: String = row.get("state");
    let kind: String = row.get("kind");
    let op = if state == "active" {
        "upsert"
    } else {
        sqlx::query("DELETE FROM embeddings WHERE owner_type = 'entity' AND owner_id = $1")
            .bind(entity_id)
            .execute(&mut *c)
            .await
            .map_err(db)?;
        "tombstone"
    };
    change(c, "entity", entity_id, op, Some(&kind)).await?;
    Ok(1)
}

/// Removes the claims of `record_ids` (tombstoned, gone or no longer servable records) and
/// refreshes the entities they fed. Returns the number of `changes` rows written.
pub(crate) async fn unlink_records(c: &mut PgConnection, record_ids: &[i64]) -> Result<u32> {
    let entities: Vec<String> = sqlx::query_scalar(
        "WITH d AS (DELETE FROM entity_claims WHERE record_id = ANY($1) RETURNING entity_id)
         SELECT DISTINCT entity_id FROM d ORDER BY entity_id",
    )
    .bind(record_ids)
    .fetch_all(&mut *c)
    .await
    .map_err(db)?;
    let mut n = 0;
    for e in entities {
        n += refresh(c, &e).await?;
    }
    Ok(n)
}

/// Merges `from` into `into` inside the caller's transaction. Returns `changes` rows written.
async fn merge(c: &mut PgConnection, from: &str, into: &str) -> Result<u32> {
    sqlx::query(
        "INSERT INTO entity_claims (entity_id, attribute, record_id, version, value, confidence,
                                    created_at)
         SELECT $2, attribute, record_id, version, value, confidence, created_at
         FROM entity_claims WHERE entity_id = $1
         ON CONFLICT (entity_id, attribute, record_id) DO NOTHING",
    )
    .bind(from)
    .bind(into)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    sqlx::query("DELETE FROM entity_claims WHERE entity_id = $1")
        .bind(from)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    // Collapse chains: everything that pointed at `from` now points at `into`.
    sqlx::query("UPDATE entity_redirects SET new_id = $2, merged_at = now() WHERE new_id = $1")
        .bind(from)
        .bind(into)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    sqlx::query(
        "INSERT INTO entity_redirects (old_id, new_id) VALUES ($1, $2)
         ON CONFLICT (old_id) DO UPDATE SET new_id = EXCLUDED.new_id, merged_at = now()",
    )
    .bind(from)
    .bind(into)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    sqlx::query(
        "UPDATE entities i SET conflict = i.conflict OR f.conflict,
                lat = coalesce(i.lat, f.lat), lon = coalesce(i.lon, f.lon)
         FROM entities f WHERE i.entity_id = $2 AND f.entity_id = $1",
    )
    .bind(from)
    .bind(into)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    sqlx::query("DELETE FROM embeddings WHERE owner_type = 'entity' AND owner_id = $1")
        .bind(from)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    let kind: Option<String> =
        sqlx::query_scalar("DELETE FROM entities WHERE entity_id = $1 RETURNING kind")
            .bind(from)
            .fetch_optional(&mut *c)
            .await
            .map_err(db)?;
    change(c, "entity", from, "redirect", kind.as_deref()).await?;
    tracing::debug!(from, into, "entity merged");
    Ok(1)
}

/// Operator merge (`PgIndex::merge_entities`).
pub(crate) async fn merge_public(ix: &PgIndex, from: &EntityId, into: &EntityId) -> Result<()> {
    if from == into {
        return Err(Error::Invalid("cannot merge an entity into itself".into()));
    }
    let (from, into) = (from.to_string(), into.to_string());
    let mut tx = ix.pool.begin().await.map_err(db)?;
    let rows = sqlx::query(
        "SELECT entity_id, kind, licence_class, coalesce(licence_spdx, '') AS spdx
         FROM entities WHERE entity_id IN ($1, $2) AND state = 'active'",
    )
    .bind(&from)
    .bind(&into)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;
    if rows.len() != 2 {
        return Err(Error::NotFound("entity".into()));
    }
    let get = |r: &sqlx::postgres::PgRow, k: &str| -> String { r.get(k) };
    let (a, b) = (&rows[0], &rows[1]);
    if get(a, "kind") != get(b, "kind") {
        return Err(Error::Policy(
            "entities of different kinds never merge".into(),
        ));
    }
    let class = get(a, "licence_class");
    if class != get(b, "licence_class")
        || (class == LicenceClass::ShareAlike.as_str() && get(a, "spdx") != get(b, "spdx"))
    {
        return Err(Error::Policy(
            "entities with incompatible licences never merge".into(),
        ));
    }
    let kind = get(a, "kind");
    lock_kinds(&mut tx, [kind.as_str()]).await?;
    merge(&mut tx, &from, &into).await?;
    refresh(&mut tx, &into).await?;
    tx.commit().await.map_err(db)?;
    Ok(())
}
