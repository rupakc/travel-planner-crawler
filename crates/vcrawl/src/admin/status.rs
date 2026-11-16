//! `vcrawl admin status`: frontier, queue, budget, sources, purges, raw store and role
//! heartbeats at a glance (spec §9). Counts only.

use serde::Serialize;
use sqlx::PgPool;

/// The status document.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Status {
    /// `url_state` rows per state.
    pub urls: serde_json::Value,
    /// Politeness keys: total, enabled, parked now.
    pub keys: serde_json::Value,
    /// `work_items` per kind and state.
    pub queue: serde_json::Value,
    /// LLM budget rows of today and this month (`cap`, `reserved`, `spent`).
    pub llm_budget: serde_json::Value,
    /// Sources enabled / disabled, open scout proposals.
    pub sources: serde_json::Value,
    /// Purge requests per state.
    pub purges: serde_json::Value,
    /// Committed segments: count, bytes, purged.
    pub segments: serde_json::Value,
    /// Records per state.
    pub records: serde_json::Value,
    /// Heartbeats: role/instance → seconds since the last beat.
    pub heartbeats: serde_json::Value,
}

const Q_URLS: &str = "SELECT coalesce(jsonb_object_agg(state, n), '{}') FROM
    (SELECT state, count(*) AS n FROM url_state GROUP BY state) t";
const Q_KEYS: &str = "SELECT jsonb_build_object(
    'total', count(*),
    'enabled', count(*) FILTER (WHERE enabled),
    'parked', count(*) FILTER (WHERE parked_until > now())) FROM politeness_keys";
const Q_QUEUE: &str = "SELECT coalesce(jsonb_object_agg(kind, s), '{}') FROM
    (SELECT kind, jsonb_object_agg(state, n) AS s FROM
        (SELECT kind, state, count(*) AS n FROM work_items GROUP BY kind, state) a
     GROUP BY kind) t";
const Q_BUDGET: &str = "SELECT coalesce(jsonb_object_agg(period, jsonb_build_object(
        'cap', cap, 'reserved', reserved, 'spent', spent)), '{}')
    FROM llm_budget WHERE period IN (to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD'),
                                     to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM'))";
const Q_SOURCES: &str = "SELECT jsonb_build_object(
    'enabled', (SELECT count(*) FROM sources WHERE enabled),
    'disabled', (SELECT count(*) FROM sources WHERE NOT enabled),
    'proposals_open', (SELECT count(*) FROM source_candidates WHERE status = 'proposed'))";
const Q_PURGES: &str = "SELECT coalesce(jsonb_object_agg(state, n), '{}') FROM
    (SELECT state, count(*) AS n FROM purge_requests GROUP BY state) t";
const Q_SEGMENTS: &str = "SELECT jsonb_build_object(
    'count', count(*), 'bytes', coalesce(sum(bytes), 0),
    'ephemeral', count(*) FILTER (WHERE ephemeral),
    'purged', count(*) FILTER (WHERE purged_at IS NOT NULL)) FROM segments";
const Q_RECORDS: &str = "SELECT coalesce(jsonb_object_agg(state, n), '{}') FROM
    (SELECT state, count(*) AS n FROM records GROUP BY state) t";
const Q_BEATS: &str = "SELECT coalesce(jsonb_object_agg(role || '/' || instance,
        round(extract(epoch FROM now() - last_beat))), '{}') FROM heartbeats";

async fn one(pool: &PgPool, q: &'static str) -> anyhow::Result<serde_json::Value> {
    Ok(sqlx::query_scalar(q).fetch_one(pool).await?)
}

/// Collects the status document.
pub async fn status(pool: &PgPool) -> anyhow::Result<Status> {
    Ok(Status {
        urls: one(pool, Q_URLS).await?,
        keys: one(pool, Q_KEYS).await?,
        queue: one(pool, Q_QUEUE).await?,
        llm_budget: one(pool, Q_BUDGET).await?,
        sources: one(pool, Q_SOURCES).await?,
        purges: one(pool, Q_PURGES).await?,
        segments: one(pool, Q_SEGMENTS).await?,
        records: one(pool, Q_RECORDS).await?,
        heartbeats: one(pool, Q_BEATS).await?,
    })
}
