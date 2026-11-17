//! Shared plumbing for adapters: spooling raw exchanges (raw first), building capture
//! metadata, status handling, secret redaction and enqueueing follow-up adapter rows.

use bytes::Bytes;
use vc_core::fetch::{Exchange, FetchError, FetchRequest, RequestPurpose};
use vc_core::frontier::{NewUrl, Validators};
use vc_core::ids::PayloadHash;
use vc_core::media::{FetchMode, MediaType};
use vc_core::sources::{AdapterCtx, AdapterReport};
use vc_core::store::{CaptureMeta, CaptureRole, SpoolPayload, SpoolRecord};
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_core::{CanonicalUrl, CaptureId, Error, PolitenessKey, Result};

use crate::sha256::Sha256;

/// SHA-256 of a payload.
pub(crate) fn payload_hash(body: &[u8]) -> PayloadHash {
    PayloadHash(Sha256::digest(body))
}

/// Capture metadata for a record written by an adapter.
pub(crate) fn meta(
    ctx: &AdapterCtx<'_>,
    url: &CanonicalUrl,
    role: CaptureRole,
    media_type: MediaType,
    status: Option<u16>,
    body: &[u8],
    validators: Validators,
) -> CaptureMeta {
    let now = vc_core::store::capture_time(ctx.clock.now());
    CaptureMeta {
        capture_id: CaptureId::at(now),
        captured_at: now,
        epoch: ctx.permit.epoch(),
        orphan_epoch: false,
        url_key: url.key(),
        url: url.clone(),
        key: ctx.permit.key().clone(),
        source_id: Some(ctx.source.id.clone()),
        role,
        fetch_mode: ctx.url.fetch_mode.clone(),
        status,
        media_type,
        payload_hash: Some(payload_hash(body)),
        payload_len: body.len() as u64,
        validators,
        revisit_of: None,
        use_signals: UseSignals::default(),
        use_policy_fetch: UsePolicy::ALLOW_ALL,
        raw_expires_at: None,
        next_fetch_at: None,
    }
}

/// Removes the named query parameters (API keys) from a URL.
pub(crate) fn redact_query(url: &CanonicalUrl, names: &[&str]) -> CanonicalUrl {
    if names.is_empty() || url.as_url().query().is_none() {
        return url.clone();
    }
    let mut u = url.as_url().clone();
    let kept: Vec<(String, String)> = u
        .query_pairs()
        .filter(|(k, _)| !names.iter().any(|n| k.eq_ignore_ascii_case(n)))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if kept.is_empty() {
        u.set_query(None);
    } else {
        u.query_pairs_mut().clear().extend_pairs(kept);
    }
    CanonicalUrl::from_url(u).unwrap_or_else(|_| url.clone())
}

/// Strips secret query parameters from URLs carried by a fetch error (they end up in logs).
pub(crate) fn redact_fetch_error(e: FetchError, names: &[&str]) -> FetchError {
    if names.is_empty() {
        return e;
    }
    match e {
        FetchError::CrossKeyRedirect(u) => FetchError::CrossKeyRedirect(redact_query(&u, names)),
        FetchError::OutOfScope(u) => FetchError::OutOfScope(redact_query(&u, names)),
        FetchError::RobotsDisallowed(u) => FetchError::RobotsDisallowed(redact_query(&u, names)),
        other => other,
    }
}

/// Options for [`fetch_raw`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Fetch<'a> {
    /// `Accept` header.
    pub accept: Option<&'a str>,
    /// Media-type hint.
    pub hint: Option<MediaType>,
    /// Query parameters to strip from the stored URL (secrets).
    pub redact: &'a [&'a str],
}

/// GETs `url` through the permit client and **spools the raw response (fsync) before
/// returning it**: everything an adapter derives comes after this call. Any HTTP status is
/// spooled; [`check_status`] decides what it means.
pub(crate) async fn fetch_raw(
    ctx: &mut AdapterCtx<'_>,
    url: CanonicalUrl,
    opts: &Fetch<'_>,
    report: &mut AdapterReport,
) -> Result<Exchange> {
    let mut req = FetchRequest::get(url, RequestPurpose::Api);
    req.accept = opts.accept.map(str::to_string);
    report.requests += 1;
    let mut ex = ctx
        .client
        .fetch(ctx.permit, req)
        .await
        .map_err(|e| redact_fetch_error(e, opts.redact))?;
    if !opts.redact.is_empty() {
        ex.url = redact_query(&ex.url, opts.redact);
        for h in &mut ex.redirects {
            h.url = redact_query(&h.url, opts.redact);
        }
    }
    let (media, mismatch) = MediaType::detect(opts.hint, ex.content_type(), &ex.body);
    if mismatch {
        tracing::debug!(adapter = ?ctx.url.fetch_mode.to_string(), "media type sniff mismatch");
    }
    let m = meta(
        ctx,
        &ex.url,
        CaptureRole::Primary,
        media,
        Some(ex.status.as_u16()),
        &ex.body,
        ex.validators(),
    );
    report.bytes += ex.body.len() as u64;
    ctx.sink
        .append(SpoolRecord {
            meta: m,
            payload: SpoolPayload::Response(ex.clone()),
        })
        .await?;
    report.records += 1;
    Ok(ex)
}

/// Appends a `resource` record (filtered dump output).
pub(crate) async fn append_resource(
    ctx: &mut AdapterCtx<'_>,
    url: &CanonicalUrl,
    media: MediaType,
    content_type: &str,
    body: Bytes,
    report: &mut AdapterReport,
) -> Result<()> {
    let m = meta(
        ctx,
        url,
        CaptureRole::Primary,
        media,
        None,
        &body,
        Validators::default(),
    );
    ctx.sink
        .append(SpoolRecord {
            meta: m,
            payload: SpoolPayload::Resource {
                content_type: content_type.to_string(),
                body,
            },
        })
        .await?;
    report.records += 1;
    Ok(())
}

/// Appends a `metadata` record (a `dump_manifest`).
pub(crate) async fn append_manifest(
    ctx: &mut AdapterCtx<'_>,
    url: &CanonicalUrl,
    fields: serde_json::Value,
    report: &mut AdapterReport,
) -> Result<()> {
    let body = serde_json::to_vec(&fields)?;
    let m = meta(
        ctx,
        url,
        CaptureRole::Manifest,
        MediaType::Json,
        None,
        &body,
        Validators::default(),
    );
    ctx.sink
        .append(SpoolRecord {
            meta: m,
            payload: SpoolPayload::Metadata { fields },
        })
        .await?;
    report.records += 1;
    Ok(())
}

/// Maps a non-2xx status (already spooled) to an error: 401/403 → challenge (the caller
/// parks the key, never retries with another identity); 404/410 → not found; 429/503 → rate
/// limited with the parsed `Retry-After` (the caller backs the key off); other 5xx → server
/// error (counts toward the key's consecutive-5xx park).
pub(crate) fn check_status(ex: &Exchange) -> Result<()> {
    let s = ex.status.as_u16();
    match s {
        200..=299 => Ok(()),
        401 | 403 => Err(Error::Fetch(FetchError::Challenge { status: s })),
        404 | 410 => Err(Error::NotFound(format!("http {s}"))),
        429 | 503 => Err(Error::Fetch(FetchError::RateLimited {
            status: s,
            retry_after: retry_after(ex),
        })),
        500..=599 => Err(Error::Fetch(FetchError::ServerError { status: s })),
        _ => Err(Error::Invalid(format!("unexpected http {s}"))),
    }
}

/// The `Retry-After` of a response (RFC 9110 §10.2.3: delay-seconds or an HTTP-date, relative
/// to when the response ended; a past date is zero). Anything else is `None`.
pub(crate) fn retry_after(ex: &Exchange) -> Option<std::time::Duration> {
    let v = ex.response_headers.get("retry-after")?.to_str().ok()?;
    parse_retry_after(v, ex.ended_at.max(chrono::Utc::now()))
}

/// Parses a `Retry-After` value relative to `now`.
pub(crate) fn parse_retry_after(
    value: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<std::time::Duration> {
    let v = value.trim();
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        return v.parse::<u64>().ok().map(std::time::Duration::from_secs);
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    Some(
        (at.with_timezone(&chrono::Utc) - now)
            .to_std()
            .unwrap_or_default(),
    )
}

/// Resolves `path_and_query` against the leased URL's origin (follow-ups never leave the
/// adapter's own host).
pub(crate) fn on_origin(base: &CanonicalUrl, path_and_query: &str) -> Result<CanonicalUrl> {
    let mut u = base.as_url().clone();
    u.set_query(None);
    let joined = u
        .join(path_and_query)
        .map_err(|e| Error::Invalid(e.to_string()))?;
    if joined.host_str() != base.as_url().host_str() || joined.port() != base.as_url().port() {
        return Err(Error::Invalid(
            "follow-up URL leaves the source host".into(),
        ));
    }
    CanonicalUrl::from_url(joined).map_err(|e| Error::Invalid(e.to_string()))
}

/// Enqueues follow-up `adapter:<id>` rows for the same source (frontier politeness then
/// applies per row: ≤ 5 per claim, the key's delay between them).
pub(crate) async fn enqueue_adapter_rows(
    ctx: &mut AdapterCtx<'_>,
    adapter: &str,
    urls: Vec<CanonicalUrl>,
    priority: f32,
    report: &mut AdapterReport,
) -> Result<()> {
    if urls.is_empty() {
        return Ok(());
    }
    let batch: Vec<NewUrl> = urls
        .into_iter()
        .map(|url| NewUrl {
            key: match &ctx.source.politeness_key {
                Some(k) => PolitenessKey(k.clone()),
                None => url.politeness_key(),
            },
            source_id: Some(ctx.source.id.clone()),
            discovered_from: Some(ctx.url.url_key),
            priority,
            depth: ctx.url.depth.saturating_add(1),
            expect: Some(MediaType::Json),
            fetch_mode: FetchMode::Adapter(adapter.to_string()),
            not_before: None,
            url,
        })
        .collect();
    let frontier = ctx.frontier;
    for chunk in batch.chunks(500) {
        report.enqueued += frontier.enqueue(chunk).await?;
    }
    Ok(())
}

/// Parses a JSON body, mapping errors to [`Error::Invalid`].
pub(crate) fn json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T> {
    serde_json::from_slice(body).map_err(|e| Error::Invalid(format!("json: {e}")))
}

/// Decodes XML text: predefined entities and character references.
pub(crate) fn xml_unescape(raw: &str) -> String {
    quick_xml::escape::unescape(raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// The value of attribute `name` on an XML start/empty tag, unescaped.
pub(crate) fn xml_attr(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find_map(|a| (a.key.local_name().as_ref() == name).then(|| xml_unescape(&a.value)))
}
