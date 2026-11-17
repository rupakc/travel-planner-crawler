//! Author erasure (spec §5.10 forums, §17.7; GDPR): what an `--author` purge touches, without
//! ever storing the name.
//!
//! * **Hashed selector.** The display name is reduced to `sha256:<hex>` before anything is
//!   written: `purge_requests.selector`, the standing exclusion in `tombstones` (which
//!   `vc-index` honours on re-ingest by hashing each author it sees) and every predicate below
//!   compare hashes, never the name.
//! * **Matched records** are those with a version whose data names the author as an author
//!   (`author`, `author.name` or `authors[]`, at any depth), the same rule as `vc-index`. They are
//!   tombstoned one by one (`RecordSink::tombstone(Record)`), and the scope is fixed in
//!   `purge_requests.scope` before anything is scrubbed (afterwards the data no longer names
//!   the author, so it could not be found again).
//! * **Scrub, scoped to the matched records:** in every stored version the author's own posts
//!   (each object attributed to them) have every string replaced by `[removed]`, `authors[]`
//!   entries are replaced, `data_hash` is recomputed, the name is cut from `attribution` /
//!   `title` (whole words only: "Jordan" never touches "Jordanson") and the excerpt is dropped;
//!   the record's licence attribution, title and `current_data_hash` follow, its embeddings and
//!   entity claims go, and a `changes` row is written. Observations of those records, held
//!   records and cached LLM outputs naming the author as an author go too. Nothing outside the
//!   matched records is touched (a city or another user sharing the string stays).
//! * **Raw scope:** the captures that produced a version of a matched record, plus every capture
//!   of the records' URLs and of those captures' URLs, so a later capture that only re-saw
//!   (touched) a record is cut out as well; plus every output chunk of an author-keeping dump
//!   adapter ([`AUTHOR_DUMP_ADAPTERS`]) that holds a post attributed to the author (found by
//!   reading the chunk: the extracted records of such dumps carry no author at all).
//! * **Later dumps:** the fetcher hands `se_travel_dump` the hashed selectors of every author
//!   tombstone when a run starts ([`crate::wiring::sources::adapter_for_run`]), and the adapter
//!   drops their posts before anything is spooled, so a re-ingested dump brings no purged
//!   author back to raw, the text tier or the index.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use vc_core::RecordId;
use vc_core::index::{PurgeSelector, RecordSink, TombstoneReason};

use super::purge::{DoomedCapture, REMOVED};

/// Dump adapters whose output keeps post authors (as the `attribution.author` of each post).
pub const AUTHOR_DUMP_ADAPTERS: &[&str] = &["adapter:se_travel_dump"];

/// Prefix of a hashed author selector.
pub const HASH_PREFIX: &str = "sha256:";

/// Lowercase hex sha256 of `s`.
pub fn sha256_hex(s: &str) -> String {
    use std::fmt::Write;
    let d = Sha256::digest(s.as_bytes());
    let mut out = String::with_capacity(64);
    for b in d {
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The stored form of an author selector value: `sha256:<hex>` (a hashed value stays as is).
pub fn hashed(name: &str) -> String {
    if is_hashed(name) {
        name.to_string()
    } else {
        format!("{HASH_PREFIX}{}", sha256_hex(name))
    }
}

fn is_hashed(s: &str) -> bool {
    s.strip_prefix(HASH_PREFIX)
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The selector as stored: authors hashed, everything else unchanged.
pub fn stored_selector(sel: &PurgeSelector) -> PurgeSelector {
    match sel {
        PurgeSelector::Author(a) => PurgeSelector::Author(hashed(a)),
        other => other.clone(),
    }
}

/// The hex digest of a (hashed or plain) author selector value.
pub fn digest(author: &str) -> String {
    hashed(author)[HASH_PREFIX.len()..].to_string()
}

/// SQL: whether the jsonb expression `col` names the author whose sha256 hex is `$1` as an
/// author (`author`, `author.name`, `authors[]` / `authors`, at any depth).
pub fn author_sql(col: &str) -> String {
    format!(
        "EXISTS (SELECT 1 FROM jsonb_path_query({col}, 'lax $.** ? (@.type() == \"object\")') AS o(v)
           WHERE encode(sha256(convert_to(CASE jsonb_typeof(o.v->'author')
                     WHEN 'string' THEN o.v->>'author'
                     WHEN 'object' THEN o.v->'author'->>'name' END, 'UTF8')), 'hex') = $1
              OR (jsonb_typeof(o.v->'authors') = 'string'
                  AND encode(sha256(convert_to(o.v->>'authors', 'UTF8')), 'hex') = $1)
              OR (jsonb_typeof(o.v->'authors') = 'array' AND EXISTS (
                     SELECT 1 FROM jsonb_array_elements(o.v->'authors') AS a(e)
                     WHERE jsonb_typeof(a.e) = 'string'
                       AND encode(sha256(convert_to(a.e #>> '{{}}', 'UTF8')), 'hex') = $1)))"
    )
}

/// Matches strings against the author's hash.
#[derive(Debug, Clone)]
pub struct Matcher {
    hex: String,
}

impl Matcher {
    /// A matcher for a (hashed or plain) selector value.
    pub fn new(author: &str) -> Self {
        Matcher {
            hex: digest(author),
        }
    }

    /// Whether `s` is the author's name.
    pub fn is(&self, s: &str) -> bool {
        sha256_hex(s) == self.hex
    }

    /// The object is a post by the author: its `author` (or `author.name`) is the author, or
    /// its `attribution` names them (the dump shape).
    fn attributed(&self, m: &serde_json::Map<String, Value>) -> bool {
        let named = |v: Option<&Value>| match v {
            Some(Value::String(s)) => self.is(s),
            Some(Value::Object(o)) => o
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|s| self.is(s)),
            _ => false,
        };
        named(m.get("author"))
            || matches!(m.get("attribution"), Some(Value::Object(a)) if named(a.get("author")))
    }

    /// Whether `v` holds anything attributed to the author (see [`Self::redact_data`]).
    pub fn mentions(&self, v: &Value) -> bool {
        let mut c = v.clone();
        self.redact_data(&mut c)
    }

    /// Redacts the author's posts in `v`: every string of an object attributed to the author
    /// becomes `[removed]`, and matching `authors` entries too. Returns whether anything changed.
    pub fn redact_data(&self, v: &mut Value) -> bool {
        match v {
            Value::Object(m) => {
                if self.attributed(m) {
                    let mut changed = false;
                    for x in m.values_mut() {
                        changed |= blank_strings(x);
                    }
                    return changed;
                }
                let mut changed = false;
                match m.get_mut("authors") {
                    Some(Value::String(s)) if self.is(s) => {
                        *s = REMOVED.to_string();
                        changed = true;
                    }
                    Some(Value::Array(a)) => {
                        for e in a.iter_mut() {
                            if e.as_str().is_some_and(|s| self.is(s)) {
                                *e = Value::String(REMOVED.into());
                                changed = true;
                            }
                        }
                    }
                    _ => {}
                }
                for (k, x) in m.iter_mut() {
                    if k != "authors" {
                        changed |= self.redact_data(x);
                    }
                }
                changed
            }
            Value::Array(a) => {
                let mut changed = false;
                for x in a {
                    changed |= self.redact_data(x);
                }
                changed
            }
            _ => false,
        }
    }

    /// Replaces each whole-word occurrence of the name in `s` (a substring that starts and ends
    /// at word boundaries; at most 256 bytes) with `[removed]`.
    pub fn redact_text(&self, s: &str) -> Option<String> {
        const MAX: usize = 256;
        let alnum = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
        let starts: Vec<usize> = s
            .char_indices()
            .filter(|(i, c)| !(alnum(s[..*i].chars().next_back()) && c.is_alphanumeric()))
            .map(|(i, _)| i)
            .collect();
        let is_end = |j: usize| {
            j == s.len() || !(alnum(s[..j].chars().next_back()) && alnum(s[j..].chars().next()))
        };
        let mut out = String::with_capacity(s.len());
        let mut done = 0usize;
        let mut changed = false;
        for &i in &starts {
            if i < done {
                continue;
            }
            let hit = s[i..]
                .char_indices()
                .map(|(k, c)| i + k + c.len_utf8())
                .take_while(|j| j - i <= MAX)
                .filter(|&j| is_end(j))
                .find(|&j| self.is(&s[i..j]));
            if let Some(j) = hit {
                out.push_str(&s[done..i]);
                out.push_str(REMOVED);
                done = j;
                changed = true;
            }
        }
        if !changed {
            return None;
        }
        out.push_str(&s[done..]);
        Some(out)
    }

    /// [`Self::redact_text`] over every string of a JSON value.
    pub fn redact_json_text(&self, v: &mut Value) -> bool {
        match v {
            Value::String(s) => match self.redact_text(s) {
                Some(n) => {
                    *s = n;
                    true
                }
                None => false,
            },
            Value::Array(a) => a
                .iter_mut()
                .fold(false, |c, x| self.redact_json_text(x) | c),
            Value::Object(m) => m
                .values_mut()
                .fold(false, |c, x| self.redact_json_text(x) | c),
            _ => false,
        }
    }
}

fn blank_strings(v: &mut Value) -> bool {
    match v {
        Value::String(s) if s != REMOVED => {
            *s = REMOVED.to_string();
            true
        }
        Value::Array(a) => a.iter_mut().fold(false, |c, x| blank_strings(x) | c),
        Value::Object(m) => m.values_mut().fold(false, |c, x| blank_strings(x) | c),
        _ => false,
    }
}

/// What an author purge covers, fixed at the index stage (`purge_requests.scope`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorScope {
    /// Matched records.
    pub record_ids: Vec<i64>,
    /// Captures that produced a version of a matched record.
    pub capture_ids: Vec<String>,
    /// URLs (hex url keys) whose captures may hold the author's posts.
    pub url_keys: Vec<String>,
}

/// Finds the scope (before anything is scrubbed).
pub async fn find_scope(pool: &PgPool, author: &str) -> anyhow::Result<AuthorScope> {
    let hex = digest(author);
    let record_ids: Vec<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT DISTINCT v.record_id FROM record_versions v WHERE {} ORDER BY 1",
        author_sql("v.data")
    )))
    .bind(&hex)
    .fetch_all(pool)
    .await?;
    let capture_ids: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT capture_id FROM record_versions WHERE record_id = ANY($1)
         UNION SELECT DISTINCT capture_id FROM observations WHERE record_id = ANY($1)
         ORDER BY 1",
    )
    .bind(&record_ids)
    .fetch_all(pool)
    .await?;
    let url_keys: Vec<String> = sqlx::query_scalar(
        "SELECT encode(url_key, 'hex') FROM records WHERE record_id = ANY($1)
         UNION SELECT encode(c.url_key, 'hex') FROM captures c WHERE c.capture_id = ANY($2)
         ORDER BY 1",
    )
    .bind(&record_ids)
    .bind(&capture_ids)
    .fetch_all(pool)
    .await?;
    Ok(AuthorScope {
        record_ids,
        capture_ids,
        url_keys,
    })
}

/// The scope of request `id`: the stored one, else found now and stored.
pub async fn scope(pool: &PgPool, id: i64, author: &str) -> anyhow::Result<AuthorScope> {
    let stored: Option<Value> =
        sqlx::query_scalar("SELECT scope FROM purge_requests WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    if let Some(v) = stored {
        return Ok(serde_json::from_value(v)?);
    }
    let s = find_scope(pool, author).await?;
    sqlx::query("UPDATE purge_requests SET scope = $2 WHERE id = $1 AND scope IS NULL")
        .bind(id)
        .bind(serde_json::to_value(&s)?)
        .execute(pool)
        .await?;
    Ok(s)
}

/// Index stage of an author purge: tombstone the matched records, record the hashed standing
/// exclusion, drop held records and scrub. Returns `(records tombstoned, rows scrubbed)`.
pub async fn index_stage(
    pool: &PgPool,
    id: i64,
    author: &str,
    reason: TombstoneReason,
    reason_s: &str,
) -> anyhow::Result<(u64, u64)> {
    let sc = scope(pool, id, author).await?;
    let ix = vc_index::PgIndex::new(pool.clone());
    let mut n = 0;
    for r in &sc.record_ids {
        n += ix
            .tombstone(&PurgeSelector::Record(RecordId(*r)), reason)
            .await?;
    }
    sqlx::query(
        "INSERT INTO tombstones (selector_type, selector, reason) VALUES ('author', $1, $2)
         ON CONFLICT (selector_type, selector) DO NOTHING",
    )
    .bind(hashed(author))
    .bind(reason_s)
    .execute(pool)
    .await?;
    let scrubbed = scrub(pool, author, &sc).await?;
    Ok((n, scrubbed))
}

/// The committed captures of the scope.
pub async fn captures(pool: &PgPool, sc: &AuthorScope) -> anyhow::Result<Vec<DoomedCapture>> {
    let rows = sqlx::query(
        "SELECT c.capture_id, c.captured_at, s.segment_id, s.path, s.cdxj_path
         FROM captures c JOIN segments s ON s.segment_id = c.segment_id
         WHERE s.purged_at IS NULL
           AND (c.capture_id = ANY($1)
                OR c.url_key = ANY(ARRAY(SELECT decode(k, 'hex') FROM unnest($2::text[]) k)))
         ORDER BY s.segment_id, c.capture_id",
    )
    .bind(&sc.capture_ids)
    .bind(&sc.url_keys)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|r| {
            Ok(DoomedCapture {
                capture: vc_core::CaptureRef {
                    capture_id: r.get::<String, _>("capture_id").parse()?,
                    captured_at: r.get("captured_at"),
                },
                segment_id: r.get("segment_id"),
                path: r.get("path"),
                cdxj_path: r.get("cdxj_path"),
            })
        })
        .collect()
}

/// The output chunks of author-keeping dump adapters that hold a post attributed to the author
/// (each chunk is read once per run; a chunk whose object is gone is skipped).
pub async fn dump_captures(
    pool: &PgPool,
    store: &dyn vc_core::store::BlobStore,
    author: &str,
) -> anyhow::Result<Vec<DoomedCapture>> {
    let m = Matcher::new(author);
    let rows = sqlx::query(
        "SELECT c.capture_id, c.captured_at, s.segment_id, s.path, s.cdxj_path,
                c.warc_offset, c.warc_length
         FROM captures c JOIN segments s ON s.segment_id = c.segment_id
         WHERE s.purged_at IS NULL AND c.fetch_mode = ANY($1)
         ORDER BY s.segment_id, c.capture_id",
    )
    .bind(AUTHOR_DUMP_ADAPTERS)
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let path: String = r.get("path");
        let (off, len): (i64, i64) = (r.get("warc_offset"), r.get("warc_length"));
        let recs =
            match vc_store::read_capture(store, &path, u64::try_from(off)?, u64::try_from(len)?)
                .await
            {
                Ok(recs) => recs,
                Err(vc_core::Error::NotFound(_)) => continue,
                Err(e) => return Err(e.into()),
            };
        let hit = recs.iter().any(|w| {
            w.warc_type() == Some("resource")
                && serde_json::from_slice::<Value>(&w.block).is_ok_and(|v| m.mentions(&v))
        });
        if hit {
            out.push(DoomedCapture {
                capture: vc_core::CaptureRef {
                    capture_id: r.get::<String, _>("capture_id").parse()?,
                    captured_at: r.get("captured_at"),
                },
                segment_id: r.get("segment_id"),
                path,
                cdxj_path: r.get("cdxj_path"),
            });
        }
    }
    Ok(out)
}

/// Scrubs the author from the matched records (and only them), their observations, held
/// records and cached LLM outputs. Returns the rows changed.
pub async fn scrub(pool: &PgPool, author: &str, sc: &AuthorScope) -> anyhow::Result<u64> {
    let m = Matcher::new(author);
    let hex = digest(author);
    let ids = &sc.record_ids;
    let mut tx = pool.begin().await?;
    let mut n = 0u64;

    let versions = sqlx::query(
        "SELECT record_id, version, created_at, data, attribution, title, excerpt
         FROM record_versions WHERE record_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    for v in versions {
        let mut data: Value = v.get("data");
        let data_changed = m.redact_data(&mut data);
        let redact = |s: Option<String>| -> (Option<String>, bool) {
            match s.as_deref().and_then(|x| m.redact_text(x)) {
                Some(r) => (Some(r), true),
                None => (s, false),
            }
        };
        let (attribution, a_changed) = redact(v.get("attribution"));
        let (title, t_changed) = redact(v.get("title"));
        let excerpt: Option<String> = v.get("excerpt");
        // The excerpt quotes the record's content: drop it where the author's posts were.
        let (excerpt, e_changed) = if data_changed && excerpt.is_some() {
            (None, true)
        } else {
            redact(excerpt)
        };
        if !(data_changed || a_changed || t_changed || e_changed) {
            continue;
        }
        n += sqlx::query(
            "UPDATE record_versions SET data = $4, data_hash = $5, attribution = $6, title = $7,
                    excerpt = $8
             WHERE record_id = $1 AND version = $2 AND created_at = $3",
        )
        .bind(v.get::<i64, _>("record_id"))
        .bind(v.get::<i32, _>("version"))
        .bind(v.get::<chrono::DateTime<chrono::Utc>, _>("created_at"))
        .bind(&data)
        .bind(vc_index::data_hash(&data))
        .bind(attribution)
        .bind(title)
        .bind(excerpt)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }

    let records = sqlx::query(
        "SELECT r.record_id, r.kind, r.licence, r.title,
                (SELECT v.data_hash FROM record_versions v
                  WHERE v.record_id = r.record_id AND v.version = r.current_version
                  ORDER BY v.created_at DESC LIMIT 1) AS cur_hash
         FROM records r WHERE r.record_id = ANY($1) ORDER BY r.record_id",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    for r in records {
        let id: i64 = r.get("record_id");
        let mut licence: Option<Value> = r.get("licence");
        if let Some(l) = licence.as_mut() {
            m.redact_json_text(l);
        }
        let title: Option<String> = r.get("title");
        let title = match title.as_deref().and_then(|t| m.redact_text(t)) {
            Some(t) => Some(t),
            None => title,
        };
        n += sqlx::query(
            "UPDATE records SET licence = $2, title = $3,
                    current_data_hash = coalesce($4, current_data_hash),
                    search_text = CASE WHEN state = 'tombstoned' THEN NULL ELSE search_text END,
                    tsv = CASE WHEN state = 'tombstoned' THEN NULL ELSE tsv END,
                    updated_at = now()
             WHERE record_id = $1",
        )
        .bind(id)
        .bind(licence)
        .bind(title)
        .bind(r.get::<Option<Vec<u8>>, _>("cur_hash"))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        sqlx::query(
            "INSERT INTO changes (owner_type, owner_id, op, kind) VALUES ('record', $1, 'tombstone', $2)",
        )
        .bind(id.to_string())
        .bind(r.get::<String, _>("kind"))
        .execute(&mut *tx)
        .await?;
    }
    let texts: Vec<String> = ids.iter().map(i64::to_string).collect();
    n += sqlx::query("DELETE FROM embeddings WHERE owner_type = 'record' AND owner_id = ANY($1)")
        .bind(&texts)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    n += sqlx::query("DELETE FROM entity_claims WHERE record_id = ANY($1)")
        .bind(ids)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let obs = sqlx::query(
        "SELECT series_key, observed_at, source_id, value FROM observations
         WHERE record_id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    for o in obs {
        let mut value: Value = o.get("value");
        if !m.redact_data(&mut value) {
            continue;
        }
        n += sqlx::query(
            "UPDATE observations SET value = $4
             WHERE series_key = $1 AND observed_at = $2 AND source_id = $3",
        )
        .bind(o.get::<String, _>("series_key"))
        .bind(o.get::<chrono::DateTime<chrono::Utc>, _>("observed_at"))
        .bind(o.get::<String, _>("source_id"))
        .bind(value)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }

    n += sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM held_records h WHERE {}",
        author_sql("h.record")
    )))
    .bind(&hex)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    n += sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM llm_cache c WHERE {}",
        author_sql("c.output")
    )))
    .bind(&hex)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
}

/// The distinct ids as a set (tests, diagnostics).
pub fn id_set(sc: &AuthorScope) -> BTreeSet<i64> {
    sc.record_ids.iter().copied().collect()
}
