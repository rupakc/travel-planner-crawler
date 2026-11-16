//! `RecordSink`: upserts and tombstones, one transaction per call.

use std::collections::BTreeSet;

use sqlx::{PgConnection, Row};
use vc_core::index::{PurgeSelector, TombstoneReason, UpsertReport};
use vc_core::records::{ExtractedRecord, MAX_EXCERPT_CHARS, ObservationPoint, Provenance};
use vc_core::{Error, Result};

use crate::PgIndex;
use crate::entity::{self, LinkInput};
use crate::util::{
    Indexing, body_text, data_hash, db, display_name, fts_config, geo_of, indexing, links_entities,
    mentions_author, method_str, truncate_chars,
};

/// Appends one `changes` row (its `xid` is the writing transaction's).
pub(crate) async fn change(
    c: &mut PgConnection,
    owner_type: &str,
    owner_id: &str,
    op: &str,
    kind: Option<&str>,
) -> Result<()> {
    sqlx::query("INSERT INTO changes (owner_type, owner_id, op, kind) VALUES ($1, $2, $3, $4)")
        .bind(owner_type)
        .bind(owner_id)
        .bind(op)
        .bind(kind)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    Ok(())
}

/// Serialises entity resolution per kind (sorted, so concurrent writers never deadlock).
pub(crate) async fn lock_kinds<'a>(
    c: &mut PgConnection,
    kinds: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let set: BTreeSet<&str> = kinds.into_iter().collect();
    for k in set {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('vc-index:kind:' || $1, 0))")
            .bind(k)
            .execute(&mut *c)
            .await
            .map_err(db)?;
    }
    Ok(())
}

/// Standing exclusions that match the capture as a whole, and the excluded authors.
async fn exclusions(c: &mut PgConnection, prov: &Provenance) -> Result<(bool, Vec<String>)> {
    let host = prov.url.host().to_ascii_lowercase();
    let whole: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM tombstones WHERE
            (selector_type = 'url' AND selector = $1)
         OR (selector_type = 'url_prefix' AND starts_with($2, selector))
         OR (selector_type = 'host' AND selector = $3)
         OR (selector_type = 'source' AND selector = $4))",
    )
    .bind(prov.url_key.to_hex())
    .bind(prov.url.as_str())
    .bind(&host)
    .bind(&prov.source_id.0)
    .fetch_one(&mut *c)
    .await
    .map_err(db)?;
    let authors: Vec<String> =
        sqlx::query_scalar("SELECT selector FROM tombstones WHERE selector_type = 'author'")
            .fetch_all(&mut *c)
            .await
            .map_err(db)?;
    Ok((whole, authors))
}

pub(crate) async fn upsert(
    ix: &PgIndex,
    recs: Vec<ExtractedRecord>,
    prov: &Provenance,
) -> Result<UpsertReport> {
    if !prov.licence.class.ingestible() {
        return Err(Error::Policy(format!(
            "licence class {} is never ingested",
            prov.licence.class.as_str()
        )));
    }
    let mut report = UpsertReport::default();
    if recs.is_empty() {
        return Ok(report);
    }
    let mut tx = ix.pool.begin().await.map_err(db)?;
    lock_kinds(
        &mut tx,
        recs.iter()
            .filter(|r| r.observation.is_none())
            .map(|r| &*r.kind.0),
    )
    .await?;
    let (excluded, authors) = exclusions(&mut tx, prov).await?;
    if !excluded {
        let mut series_changed = BTreeSet::new();
        for rec in &recs {
            if authors.iter().any(|a| mentions_author(&rec.data, a)) {
                continue;
            }
            match &rec.observation {
                Some(pt) => {
                    if observation(&mut tx, pt, prov).await? {
                        report.observations += 1;
                        series_changed.insert((pt.series_key.clone(), rec.kind.0.to_string()));
                    }
                }
                None => record(ix, &mut tx, rec, prov, &mut report).await?,
            }
        }
        for (series, kind) in series_changed {
            change(&mut tx, "observation", &series, "upsert", Some(&kind)).await?;
            report.changes += 1;
        }
    }
    tx.commit().await.map_err(db)?;
    Ok(report)
}

/// Appends (or corrects) one observation point; true when a row was written.
async fn observation(
    c: &mut PgConnection,
    pt: &ObservationPoint,
    prov: &Provenance,
) -> Result<bool> {
    let licence = serde_json::to_value(&prov.licence)?;
    let n = sqlx::query(
        "INSERT INTO observations
            (series_key, observed_at, source_id, value, capture_id, captured_at,
             licence, url_key, url, host)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (series_key, observed_at, source_id) DO UPDATE SET
            value = EXCLUDED.value, capture_id = EXCLUDED.capture_id,
            captured_at = EXCLUDED.captured_at, licence = EXCLUDED.licence,
            url_key = EXCLUDED.url_key, url = EXCLUDED.url, host = EXCLUDED.host
         WHERE observations.value IS DISTINCT FROM EXCLUDED.value
           AND NOT observations.tombstoned
           AND observations.captured_at <= EXCLUDED.captured_at",
    )
    .bind(&pt.series_key)
    .bind(pt.observed_at)
    .bind(&prov.source_id.0)
    .bind(&pt.value)
    .bind(prov.capture.capture_id.to_string())
    .bind(prov.capture.captured_at)
    .bind(licence)
    .bind(&prov.url_key.as_bytes()[..])
    .bind(prov.url.as_str())
    .bind(prov.url.host().to_ascii_lowercase())
    .execute(&mut *c)
    .await
    .map_err(db)?
    .rows_affected();
    Ok(n > 0)
}

/// Upserts one versioned record (and its entity link).
async fn record(
    ix: &PgIndex,
    c: &mut PgConnection,
    rec: &ExtractedRecord,
    prov: &Provenance,
    report: &mut UpsertReport,
) -> Result<()> {
    let kind = &*rec.kind.0;
    let hash = data_hash(&rec.data);
    let policy = rec.use_policy.meet(&prov.use_policy);
    let captured_at = prov.capture.captured_at;

    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO records (kind, source_id, url_key, record_key, licence_class)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (url_key, kind, record_key) DO NOTHING
         RETURNING record_id",
    )
    .bind(kind)
    .bind(&prov.source_id.0)
    .bind(&prov.url_key.as_bytes()[..])
    .bind(&rec.record_key.0)
    .bind(prov.licence.class.as_str())
    .fetch_optional(&mut *c)
    .await
    .map_err(db)?;

    // `revive`: a `gone` record seen again unchanged is re-served without a new version.
    let (record_id, version, revive) = match inserted {
        Some(id) => (id, 0, false),
        None => {
            let row = sqlx::query(
                "SELECT record_id, state, current_version, current_data_hash, last_captured_at
                 FROM records WHERE url_key = $1 AND kind = $2 AND record_key = $3 FOR UPDATE",
            )
            .bind(&prov.url_key.as_bytes()[..])
            .bind(kind)
            .bind(&rec.record_key.0)
            .fetch_one(&mut *c)
            .await
            .map_err(db)?;
            let id: i64 = row.get("record_id");
            let state: String = row.get("state");
            let cur: i32 = row.get("current_version");
            let cur_hash: Option<Vec<u8>> = row.get("current_data_hash");
            let last: Option<chrono::DateTime<chrono::Utc>> = row.get("last_captured_at");
            if state == "tombstoned" {
                return Ok(());
            }
            let older = last.is_some_and(|l| l > captured_at);
            let same = cur > 0 && cur_hash.as_deref() == Some(&hash[..]);
            if cur > 0 && (older || same) && !(same && !older && state == "gone") {
                report.unchanged += 1;
                if !older {
                    touch(c, id, cur, &policy, captured_at, kind, report).await?;
                }
                return Ok(());
            }
            (id, cur, same)
        }
    };

    let new_version = if revive { version } else { version + 1 };
    let licence = serde_json::to_value(&prov.licence)?;
    let policy_json = serde_json::to_value(policy)?;
    let excerpt = rec
        .excerpt
        .as_deref()
        .map(|e| truncate_chars(e, MAX_EXCERPT_CHARS));
    let name = display_name(rec.title.as_deref(), &rec.data);
    let (lat, lon) = geo_of(&rec.data).unzip();
    let cfg = fts_config(rec.lang.as_deref());
    let body = match indexing(&policy) {
        Indexing::None => None,
        Indexing::Title => Some(String::new()),
        Indexing::Full => Some(body_text(excerpt.as_deref(), &rec.data)),
    };

    if revive {
        sqlx::query(
            "UPDATE record_versions SET use_policy = $3 WHERE record_id = $1 AND version = $2",
        )
        .bind(record_id)
        .bind(version)
        .bind(&policy_json)
        .execute(&mut *c)
        .await
        .map_err(db)?;
    } else {
        write_version(
            c,
            record_id,
            new_version,
            rec,
            prov,
            &hash,
            &policy_json,
            name.as_deref(),
            excerpt.as_deref(),
        )
        .await?;
    }
    sqlx::query(
        "UPDATE records SET
            current_version = $2, current_data_hash = $3, licence_class = $4, title = $5,
            place_ids = $6, quality = $7, conflict = $8, url = $9, host = $10, licence = $11,
            source_class = $12, lat = $13, lon = $14, last_captured_at = $15,
            source_id = $16, state = 'active', tombstone_reason = NULL,
            fts_config = $17::regconfig, search_text = $18,
            tsv = CASE WHEN $18::text IS NULL THEN NULL ELSE
                    setweight(to_tsvector($17::regconfig, coalesce($5, '')), 'A')
                 || setweight(to_tsvector('simple', coalesce($5, '')), 'A')
                 || to_tsvector($17::regconfig, $18::text)
                 || to_tsvector('simple', $18::text) END,
            last_seen = now(), updated_at = now()
         WHERE record_id = $1",
    )
    .bind(record_id)
    .bind(new_version)
    .bind(&hash)
    .bind(prov.licence.class.as_str())
    .bind(name.as_deref())
    .bind(&rec.place_ids)
    .bind(rec.quality.clamp(0.0, 1.0))
    .bind(rec.conflict)
    .bind(prov.url.as_str())
    .bind(prov.url.host().to_ascii_lowercase())
    .bind(&licence)
    .bind(prov.source_class.as_str())
    .bind(lat)
    .bind(lon)
    .bind(captured_at)
    .bind(&prov.source_id.0)
    .bind(cfg)
    .bind(body.as_deref())
    .execute(&mut *c)
    .await
    .map_err(db)?;

    if version == 0 {
        report.inserted += 1;
    } else if revive {
        report.unchanged += 1;
    } else {
        report.new_versions += 1;
    }
    change(c, "record", &record_id.to_string(), "upsert", Some(kind)).await?;
    report.changes += 1;

    let link = LinkInput {
        record_id,
        version: new_version,
        kind,
        name: name.as_deref(),
        place_ids: &rec.place_ids,
        point: lat.zip(lon),
        licence: &prov.licence,
        data: &rec.data,
        quality: rec.quality,
    };
    report.changes += if links_entities(&policy) && name.is_some() {
        entity::link(c, ix.config(), &link).await?
    } else {
        entity::unlink_records(c, &[record_id]).await?
    };
    Ok(())
}

/// Supersedes the current version and inserts `version`.
#[allow(clippy::too_many_arguments)]
async fn write_version(
    c: &mut PgConnection,
    record_id: i64,
    new_version: i32,
    rec: &ExtractedRecord,
    prov: &Provenance,
    hash: &[u8],
    policy_json: &serde_json::Value,
    name: Option<&str>,
    excerpt: Option<&str>,
) -> Result<()> {
    let kind = &*rec.kind.0;
    let captured_at = prov.capture.captured_at;
    sqlx::query(
        "UPDATE record_versions SET superseded_at = now()
         WHERE record_id = $1 AND superseded_at IS NULL",
    )
    .bind(record_id)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    sqlx::query(
        "INSERT INTO record_versions
            (record_id, version, capture_id, captured_at, data, data_hash, kind_schema,
             extractor_ver, method, model_id, prompt_hash, extraction_run_id, quality, lang,
             place_ids, licence_class, attribution, use_policy, conflict, title, excerpt,
             valid_from)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                 $18, $19, $20, $21, $4)",
    )
    .bind(record_id)
    .bind(new_version)
    .bind(prov.capture.capture_id.to_string())
    .bind(captured_at)
    .bind(&rec.data)
    .bind(hash)
    .bind(format!("{}@{}", kind, rec.schema_version))
    .bind(&rec.extractor.version)
    .bind(method_str(rec.extractor.method))
    .bind(rec.extractor.model.as_ref().map(|m| m.0.to_string()))
    .bind(rec.extractor.prompt_hash.as_deref())
    .bind(rec.extractor.run_id.map(|r| r.to_string()))
    .bind(rec.quality.clamp(0.0, 1.0))
    .bind(rec.lang.as_deref())
    .bind(&rec.place_ids)
    .bind(prov.licence.class.as_str())
    .bind(&prov.licence.attribution)
    .bind(policy_json)
    .bind(rec.conflict)
    .bind(name)
    .bind(excerpt)
    .execute(&mut *c)
    .await
    .map_err(db)?;

    Ok(())
}

/// A record seen again unchanged: refresh `last_seen` and apply a changed use policy in place.
#[allow(clippy::too_many_arguments)]
async fn touch(
    c: &mut PgConnection,
    record_id: i64,
    version: i32,
    policy: &vc_core::usepolicy::UsePolicy,
    captured_at: chrono::DateTime<chrono::Utc>,
    kind: &str,
    report: &mut UpsertReport,
) -> Result<()> {
    let policy_json = serde_json::to_value(policy)?;
    let policy_changed: bool = sqlx::query_scalar(
        "UPDATE record_versions SET use_policy = $3
         WHERE record_id = $1 AND version = $2 AND use_policy IS DISTINCT FROM $3
         RETURNING true",
    )
    .bind(record_id)
    .bind(version)
    .bind(&policy_json)
    .fetch_optional(&mut *c)
    .await
    .map_err(db)?
    .unwrap_or(false);
    sqlx::query(
        "UPDATE records SET last_seen = now(), last_captured_at = $2,
            updated_at = CASE WHEN $3 THEN now() ELSE updated_at END
         WHERE record_id = $1",
    )
    .bind(record_id)
    .bind(captured_at)
    .bind(policy_changed)
    .execute(&mut *c)
    .await
    .map_err(db)?;
    if policy_changed {
        // Re-derive what may be indexed under the new policy.
        let body_needed = indexing(policy);
        if body_needed != Indexing::Full {
            sqlx::query(
                "UPDATE records SET search_text = CASE WHEN $2 THEN '' ELSE NULL END,
                    tsv = CASE WHEN $2 THEN setweight(to_tsvector(fts_config, coalesce(title, '')), 'A')
                              || setweight(to_tsvector('simple', coalesce(title, '')), 'A')
                          ELSE NULL END
                 WHERE record_id = $1",
            )
            .bind(record_id)
            .bind(body_needed == Indexing::Title)
            .execute(&mut *c)
            .await
            .map_err(db)?;
        }
        if !links_entities(policy) {
            report.changes += entity::unlink_records(c, &[record_id]).await?;
        }
        sqlx::query("DELETE FROM embeddings WHERE owner_type = 'record' AND owner_id = $1")
            .bind(record_id.to_string())
            .execute(&mut *c)
            .await
            .map_err(db)?;
    }
    if policy_changed {
        change(c, "record", &record_id.to_string(), "upsert", Some(kind)).await?;
        report.changes += 1;
    }
    Ok(())
}

/// The selector as (`tombstones.selector_type`, `selector`) and a SQL predicate on `records r`
/// with the selector bound as `$1`.
fn selector_sql(sel: &PurgeSelector) -> (&'static str, String, &'static str) {
    match sel {
        PurgeSelector::Url(k) => ("url", k.to_hex(), "r.url_key = decode($1, 'hex')"),
        PurgeSelector::UrlPrefix(p) => ("url_prefix", p.clone(), "starts_with(r.url, $1)"),
        PurgeSelector::Host(h) => (
            "host",
            h.trim_end_matches('.').to_ascii_lowercase(),
            "r.host = $1",
        ),
        PurgeSelector::Author(a) => (
            "author",
            a.clone(),
            "EXISTS (SELECT 1 FROM record_versions v WHERE v.record_id = r.record_id
                 AND jsonb_path_exists(v.data,
                     'lax $.** ? (@.author == $a || @.author.name == $a || @.authors == $a)',
                     jsonb_build_object('a', $1::text)))",
        ),
        PurgeSelector::Source(s) => ("source", s.0.clone(), "r.source_id = $1"),
        PurgeSelector::Record(id) => ("record", id.0.to_string(), "r.record_id = $1::bigint"),
    }
}

/// The same selector on `observations o` (authors never select observations).
fn observation_sql(sel: &PurgeSelector) -> Option<&'static str> {
    match sel {
        PurgeSelector::Url(_) => Some("o.url_key = decode($1, 'hex')"),
        PurgeSelector::UrlPrefix(_) => Some("starts_with(o.url, $1)"),
        PurgeSelector::Host(_) => Some("o.host = $1"),
        PurgeSelector::Source(_) => Some("o.source_id = $1"),
        PurgeSelector::Record(_) => Some("o.record_id = $1::bigint"),
        PurgeSelector::Author(_) => None,
    }
}

pub(crate) async fn tombstone(
    ix: &PgIndex,
    sel: &PurgeSelector,
    reason: TombstoneReason,
) -> Result<u64> {
    let (sel_type, sel_value, pred) = selector_sql(sel);
    let reason_s = match reason {
        TombstoneReason::OptOut => "opt_out",
        TombstoneReason::Robots => "robots",
        TombstoneReason::Denylist => "denylist",
        TombstoneReason::Purge => "purge",
        TombstoneReason::Gone => "gone",
    };
    let gone = reason == TombstoneReason::Gone;
    let mut tx = ix.pool.begin().await.map_err(db)?;

    let kinds: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT r.kind FROM records r WHERE {pred}"
    )))
    .bind(&sel_value)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;
    lock_kinds(&mut tx, kinds.iter().map(String::as_str)).await?;

    let state_filter = if gone {
        "r.state IN ('active', 'stale')"
    } else {
        "r.state <> 'tombstoned'"
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE records r SET state = $2, tombstone_reason = $3, tsv = NULL, search_text = NULL,
                updated_at = now()
         WHERE {pred} AND {state_filter}
         RETURNING r.record_id, r.kind"
    )))
    .bind(&sel_value)
    .bind(if gone { "gone" } else { "tombstoned" })
    .bind(reason_s)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;

    let ids: Vec<i64> = rows.iter().map(|r| r.get("record_id")).collect();
    for r in &rows {
        let id: i64 = r.get("record_id");
        let kind: String = r.get("kind");
        change(&mut tx, "record", &id.to_string(), "tombstone", Some(&kind)).await?;
    }
    if !ids.is_empty() {
        let texts: Vec<String> = ids.iter().map(i64::to_string).collect();
        sqlx::query("DELETE FROM embeddings WHERE owner_type = 'record' AND owner_id = ANY($1)")
            .bind(&texts)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        entity::unlink_records(&mut tx, &ids).await?;
    }

    if !gone {
        if let Some(opred) = observation_sql(sel) {
            let series: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "WITH t AS (UPDATE observations o SET tombstoned = true
                            WHERE {opred} AND NOT o.tombstoned RETURNING o.series_key)
                 SELECT DISTINCT series_key FROM t"
            )))
            .bind(&sel_value)
            .fetch_all(&mut *tx)
            .await
            .map_err(db)?;
            for s in series {
                change(&mut tx, "observation", &s, "tombstone", None).await?;
            }
        }
        sqlx::query(
            "INSERT INTO tombstones (selector_type, selector, reason) VALUES ($1, $2, $3)
             ON CONFLICT (selector_type, selector) DO NOTHING",
        )
        .bind(sel_type)
        .bind(&sel_value)
        .bind(reason_s)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    tracing::info!(
        selector = sel_type,
        reason = reason_s,
        records = ids.len(),
        "tombstoned"
    );
    Ok(ids.len() as u64)
}
