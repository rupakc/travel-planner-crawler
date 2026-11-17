//! Streaming dump runner (spec §5.10): a permit-gated byte stream is decompressed, split into
//! units (lines, CSV records or XML elements) and filtered **on the fly**; kept units are
//! batched into `resource` records, and a `dump_manifest` `metadata` record names the dump
//! exactly (URL, size, sha256, dump date, filter version). Memory is bounded by one chunk, one
//! unit (`max_unit_bytes`) and one output batch (`batch_bytes`); the dump itself is never held.
//!
//! The lease is extended every `extend_every` (adapter clock) between chunks; without that the
//! permit client aborts the stream when the lease expires.

use std::time::Duration;

use bytes::Bytes;
use chrono::Timelike;
use futures::StreamExt;
use vc_core::fetch::{FetchError, FetchRequest, RequestPurpose};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{append_manifest, append_resource};
use crate::sha256::{Sha256, hex};

/// Run-time limits of a dump adapter (from `[sources]`, see [`vc_config::SourcesConfig`]).
#[derive(Debug, Clone)]
pub struct DumpSettings {
    /// Cap on decoded stream bytes (the permit client enforces it).
    pub max_bytes: u64,
    /// Extend the lease every this long while streaming.
    pub extend_every: Duration,
    /// UTC hour window `[start, end)` in which dumps may run (`start > end` wraps midnight;
    /// `start == end` means always).
    pub window_utc: (u8, u8),
    /// Target size of one `resource` record.
    pub batch_bytes: usize,
    /// Largest single unit (page, row) accepted.
    pub max_unit_bytes: usize,
}

impl Default for DumpSettings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl DumpSettings {
    /// The defaults (equal to `from_config(&SourcesConfig::default())`).
    pub const DEFAULT: DumpSettings = DumpSettings {
        max_bytes: 2 * 1024 * 1024 * 1024,
        extend_every: Duration::from_secs(60),
        window_utc: (2, 5),
        batch_bytes: 8 * 1024 * 1024,
        max_unit_bytes: 32 * 1024 * 1024,
    };

    /// Settings from the `[sources]` section.
    pub fn from_config(c: &vc_config::SourcesConfig) -> Self {
        DumpSettings {
            max_bytes: c.dump_max_bytes,
            extend_every: Duration::from_secs(c.dump_extend_every_s.max(1)),
            window_utc: c.dump_window_utc,
            batch_bytes: 8 * 1024 * 1024,
            max_unit_bytes: 32 * 1024 * 1024,
        }
    }

    /// Whether `hour` (UTC) is inside the dump window.
    pub fn in_window(&self, hour: u32) -> bool {
        let (s, e) = (u32::from(self.window_utc.0), u32::from(self.window_utc.1));
        match s.cmp(&e) {
            std::cmp::Ordering::Equal => true,
            std::cmp::Ordering::Less => (s..e).contains(&hour),
            std::cmp::Ordering::Greater => hour >= s || hour < e,
        }
    }
}

/// Compression of the dump file itself (not HTTP `Content-Encoding`, which the permit client
/// already removed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    /// Plain.
    Identity,
    /// gzip (multi-member).
    Gzip,
    /// bzip2 (multi-stream).
    Bzip2,
    /// A ZIP archive: one member is read ([`DumpFilter::zip_member`]).
    Zip,
}

impl Compression {
    /// From the URL path suffix.
    pub(crate) fn from_url(u: &CanonicalUrl) -> Self {
        let p = u.as_url().path();
        if p.ends_with(".bz2") {
            Compression::Bzip2
        } else if p.to_ascii_lowercase().ends_with(".zip") {
            Compression::Zip
        } else if p.ends_with(".gz") {
            Compression::Gzip
        } else {
            Compression::Identity
        }
    }
}

/// Push decoder: compressed chunks in, decoded bytes appended to `out`.
pub(crate) struct Decoder {
    kind: Compression,
    gz: Option<flate2::write::MultiGzDecoder<Vec<u8>>>,
    bz: bzip2::Decompress,
    bz_ended: bool,
    bz_started: bool,
    zip: Option<super::zip_stream::ZipStream>,
}

impl Decoder {
    pub(crate) fn new(kind: Compression) -> Self {
        Decoder {
            kind,
            gz: (kind == Compression::Gzip).then(|| flate2::write::MultiGzDecoder::new(Vec::new())),
            bz: bzip2::Decompress::new(false),
            bz_ended: false,
            bz_started: false,
            zip: None,
        }
    }

    /// A decoder that reads the member `pick` names when the dump is a ZIP archive.
    pub(crate) fn with_zip_member(kind: Compression, pick: super::zip_stream::MemberPick) -> Self {
        let mut d = Decoder::new(kind);
        if kind == Compression::Zip {
            d.zip = Some(super::zip_stream::ZipStream::new(pick));
        }
        d
    }

    pub(crate) fn push(&mut self, mut input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        match self.kind {
            Compression::Identity => out.extend_from_slice(input),
            Compression::Zip => self
                .zip
                .get_or_insert_with(|| super::zip_stream::ZipStream::new(&[]))
                .push(input, out)?,
            Compression::Gzip => {
                use std::io::Write;
                let gz = self
                    .gz
                    .as_mut()
                    .ok_or_else(|| Error::Invalid("gzip".into()))?;
                gz.write_all(input)
                    .map_err(|e| Error::Invalid(format!("gzip: {e}")))?;
                out.append(gz.get_mut());
            }
            Compression::Bzip2 => {
                let mut stalls = 0;
                while !input.is_empty() {
                    if self.bz_ended {
                        self.bz = bzip2::Decompress::new(false);
                        self.bz_ended = false;
                    }
                    self.bz_started = true;
                    out.reserve(256 * 1024);
                    let (in0, out0) = (self.bz.total_in(), self.bz.total_out());
                    let st = self
                        .bz
                        .decompress_vec(input, out)
                        .map_err(|e| Error::Invalid(format!("bzip2: {e:?}")))?;
                    let used = usize::try_from(self.bz.total_in() - in0).unwrap_or(usize::MAX);
                    input = &input[used.min(input.len())..];
                    if st == bzip2::Status::StreamEnd {
                        self.bz_ended = true;
                    }
                    if used == 0 && self.bz.total_out() == out0 && st != bzip2::Status::StreamEnd {
                        stalls += 1;
                        if stalls > 2 {
                            return Err(Error::Invalid("bzip2: decoder stalled".into()));
                        }
                    } else {
                        stalls = 0;
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, out: &mut Vec<u8>) -> Result<()> {
        match self.kind {
            Compression::Identity => Ok(()),
            Compression::Zip => self
                .zip
                .get_or_insert_with(|| super::zip_stream::ZipStream::new(&[]))
                .finish(),
            Compression::Gzip => {
                use std::io::Write;
                if let Some(gz) = self.gz.as_mut() {
                    gz.flush()
                        .map_err(|e| Error::Invalid(format!("gzip: {e}")))?;
                    out.append(gz.get_mut());
                }
                if let Some(gz) = self.gz.take() {
                    let rest = gz
                        .finish()
                        .map_err(|e| Error::Invalid(format!("gzip: {e}")))?;
                    out.extend_from_slice(&rest);
                }
                Ok(())
            }
            Compression::Bzip2 => {
                if self.bz_started && !self.bz_ended {
                    return Err(Error::Invalid("bzip2: truncated stream".into()));
                }
                Ok(())
            }
        }
    }
}

/// How the decoded byte stream is cut into units.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Split {
    /// One unit per `\n`-terminated line (`\r` stripped).
    Lines,
    /// One unit per CSV record (newlines inside quotes do not end a record).
    Csv,
    /// One unit per `<open …>…</close>` element; bytes between elements are skipped.
    Element {
        /// The start tag prefix, e.g. `<page>`.
        open: &'static [u8],
        /// The end tag, e.g. `</page>`.
        close: &'static [u8],
    },
}

/// Incremental splitter with a bounded buffer.
pub(crate) struct Splitter {
    split: Split,
    buf: Vec<u8>,
    scan: usize,
    in_quotes: bool,
    max_unit: usize,
    discard: usize,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() || from > hay.len() - needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = from;
    while i + needle.len() <= hay.len() {
        let p = hay[i..hay.len() - needle.len() + 1]
            .iter()
            .position(|&b| b == first)?;
        let at = i + p;
        if &hay[at..at + needle.len()] == needle {
            return Some(at);
        }
        i = at + 1;
    }
    None
}

impl Splitter {
    pub(crate) fn new(split: Split, max_unit: usize) -> Self {
        Splitter {
            split,
            buf: Vec::new(),
            scan: 0,
            in_quotes: false,
            max_unit,
            discard: 0,
        }
    }

    /// Appends decoded bytes and calls `f` for every complete unit.
    pub(crate) fn push(
        &mut self,
        data: &[u8],
        f: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        self.buf.extend_from_slice(data);
        let mut consumed = 0;
        self.discard = 0;
        while let Some((s, e, next)) = self.next_unit(consumed)? {
            f(&self.buf[s..e])?;
            consumed = next;
            self.scan = next;
        }
        consumed = consumed.max(self.discard);
        if consumed > 0 {
            self.buf.drain(..consumed);
            self.scan -= consumed.min(self.scan);
        }
        if self.buf.len() > self.max_unit {
            return Err(Error::Invalid(format!(
                "dump unit exceeds {} bytes",
                self.max_unit
            )));
        }
        Ok(())
    }

    /// Flushes a trailing unit without a terminator (lines and CSV only).
    pub(crate) fn finish(&mut self, f: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if matches!(self.split, Split::Lines | Split::Csv) {
            let rest = std::mem::take(&mut self.buf);
            let rest = trim_cr(&rest);
            if !rest.iter().all(u8::is_ascii_whitespace) {
                f(rest)?;
            }
        }
        self.buf.clear();
        self.scan = 0;
        Ok(())
    }

    /// `(start, end, next)` of the next unit starting at `from`, if complete.
    fn next_unit(&mut self, from: usize) -> Result<Option<(usize, usize, usize)>> {
        match self.split {
            Split::Lines => {
                let start = self.scan.max(from);
                match self.buf[start..].iter().position(|&b| b == b'\n') {
                    Some(p) => {
                        let end = start + p;
                        let e = if end > from && self.buf[end - 1] == b'\r' {
                            end - 1
                        } else {
                            end
                        };
                        Ok(Some((from, e, end + 1)))
                    }
                    None => {
                        self.scan = self.buf.len();
                        Ok(None)
                    }
                }
            }
            Split::Csv => {
                let mut i = self.scan.max(from);
                while i < self.buf.len() {
                    match self.buf[i] {
                        b'"' => self.in_quotes = !self.in_quotes,
                        b'\n' if !self.in_quotes => {
                            let e = if i > from && self.buf[i - 1] == b'\r' {
                                i - 1
                            } else {
                                i
                            };
                            return Ok(Some((from, e, i + 1)));
                        }
                        _ => {}
                    }
                    i += 1;
                }
                self.scan = self.buf.len();
                Ok(None)
            }
            Split::Element { open, close } => {
                let Some(s) = find(&self.buf, open, from) else {
                    // Nothing but filler: drop it, keeping a tail that could start `open`.
                    self.discard = self.buf.len().saturating_sub(open.len() - 1).max(from);
                    self.scan = self.discard;
                    return Ok(None);
                };
                let search = self.scan.max(s + open.len());
                match find(&self.buf, close, search) {
                    Some(c) => Ok(Some((s, c + close.len(), c + close.len()))),
                    None => {
                        self.scan = self.buf.len().saturating_sub(close.len()).max(s);
                        Ok(None)
                    }
                }
            }
        }
    }
}

fn trim_cr(b: &[u8]) -> &[u8] {
    let b = b.strip_suffix(b"\n").unwrap_or(b);
    b.strip_suffix(b"\r").unwrap_or(b)
}

/// What a filter makes of one unit.
#[derive(Debug)]
pub(crate) enum Unit {
    /// The CSV header (repeated at the top of every output batch).
    Header(Vec<u8>),
    /// Keep: a JSON object or a CSV line (no newline).
    Keep(Vec<u8>),
    /// Drop.
    Drop,
}

/// Output format of the filtered records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputKind {
    /// A JSON array of objects (`application/json`).
    JsonArray,
    /// CSV with the header on top (`text/csv`).
    Csv,
}

/// A per-adapter dump filter.
pub(crate) trait DumpFilter: Send {
    /// Filter version (recorded in the manifest; bump on any change).
    fn version(&self) -> &'static str;
    /// How to split.
    fn split(&self) -> Split;
    /// Output format.
    fn output(&self) -> OutputKind;
    /// One unit in, at most one out.
    fn filter(&mut self, unit: &[u8]) -> Result<Unit>;
    /// For a ZIP dump: the member to read (base file names; empty = the first member).
    fn zip_member(&self) -> super::zip_stream::MemberPick {
        &[]
    }
}

struct Batcher {
    kind: OutputKind,
    header: Option<Vec<u8>>,
    buf: Vec<u8>,
    n: usize,
    limit: usize,
}

impl Batcher {
    fn add(&mut self, unit: Unit, ready: &mut Vec<Bytes>, kept: &mut u64) {
        match unit {
            Unit::Drop => {}
            Unit::Header(h) => self.header = Some(h),
            Unit::Keep(item) => {
                *kept += 1;
                if self.n == 0 {
                    match self.kind {
                        OutputKind::JsonArray => self.buf.push(b'['),
                        OutputKind::Csv => {
                            if let Some(h) = &self.header {
                                self.buf.extend_from_slice(h);
                                self.buf.push(b'\n');
                            }
                        }
                    }
                } else if self.kind == OutputKind::JsonArray {
                    self.buf.push(b',');
                }
                self.buf.extend_from_slice(&item);
                if self.kind == OutputKind::Csv {
                    self.buf.push(b'\n');
                }
                self.n += 1;
                if self.buf.len() >= self.limit {
                    ready.extend(self.flush());
                }
            }
        }
    }

    fn flush(&mut self) -> Option<Bytes> {
        if self.n == 0 {
            return None;
        }
        if self.kind == OutputKind::JsonArray {
            self.buf.push(b']');
        }
        self.n = 0;
        Some(Bytes::from(std::mem::take(&mut self.buf)))
    }
}

/// Streams `url` through `filter` (see the module docs). Writes resource batches as they fill
/// and the manifest last; on a stream error the manifest records the partial run
/// (`complete: false`) and the error is returned.
pub(crate) async fn run_dump(
    ctx: &mut AdapterCtx<'_>,
    adapter: &'static str,
    url: CanonicalUrl,
    filter: &mut dyn DumpFilter,
    settings: &DumpSettings,
) -> Result<AdapterReport> {
    let mut report = AdapterReport::default();
    let started = ctx.clock.now();
    if !settings.in_window(started.hour()) {
        tracing::info!(adapter, "outside the dump window; not started");
        return Ok(report);
    }
    let mut req = FetchRequest::get(url.clone(), RequestPurpose::Dump);
    req.max_body_bytes = settings.max_bytes;
    report.requests += 1;
    let stream = ctx.client.stream(ctx.permit, req).await?;
    let head = stream.head;
    let mut body = stream.body;
    let header = |n: &str| {
        head.headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let status = head.status.as_u16();
    let mut manifest = serde_json::json!({
        "type": "dump_manifest",
        "adapter": adapter,
        "url": url.as_str(),
        "final_url": head.url.as_str(),
        "status": status,
        "last_modified": header("last-modified"),
        "etag": header("etag"),
        "content_length": header("content-length"),
        "filter_version": filter.version(),
        "started_at": started.to_rfc3339(),
    });
    let (media, content_type) = match filter.output() {
        OutputKind::JsonArray => (MediaType::Json, "application/json"),
        OutputKind::Csv => (MediaType::Csv, "text/csv"),
    };

    if !(200..300).contains(&status) {
        drop(body);
        manifest["complete"] = false.into();
        manifest["error"] = format!("http {status}").into();
        append_manifest(ctx, &url, manifest, &mut report).await?;
        return Err(match status {
            401 | 403 => Error::Fetch(FetchError::Challenge { status }),
            404 | 410 => Error::NotFound(format!("http {status}")),
            429 | 503 => Error::Fetch(FetchError::RateLimited {
                status,
                retry_after: header("retry-after")
                    .and_then(|v| super::common::parse_retry_after(&v, chrono::Utc::now())),
            }),
            500..=599 => Error::Fetch(FetchError::ServerError { status }),
            _ => Error::Fetch(FetchError::Transport(format!("http {status}"))),
        });
    }

    let mut sha = Sha256::new();
    let mut decoder = Decoder::with_zip_member(Compression::from_url(&url), filter.zip_member());
    let mut splitter = Splitter::new(filter.split(), settings.max_unit_bytes);
    let mut batcher = Batcher {
        kind: filter.output(),
        header: None,
        buf: Vec::new(),
        n: 0,
        limit: settings.batch_bytes.max(1),
    };
    let (mut seen, mut kept, mut resources) = (0u64, 0u64, 0u64);
    let mut decoded = Vec::new();
    let mut last_extend = started;
    let extend_every = chrono::Duration::from_std(settings.extend_every)
        .unwrap_or_else(|_| chrono::Duration::seconds(60));

    let mut failure: Option<Error> = None;
    loop {
        let next = body.next().await;
        let chunk = match next {
            None => break,
            Some(Err(e)) => {
                failure = Some(Error::Fetch(e));
                break;
            }
            Some(Ok(c)) => c,
        };
        sha.update(&chunk);
        report.bytes += chunk.len() as u64;
        decoded.clear();
        let mut ready = Vec::new();
        let step = decoder.push(&chunk, &mut decoded).and_then(|()| {
            splitter.push(&decoded, &mut |u| {
                seen += 1;
                batcher.add(filter.filter(u)?, &mut ready, &mut kept);
                Ok(())
            })
        });
        if let Err(e) = step {
            failure = Some(e);
            break;
        }
        for b in ready {
            append_resource(ctx, &url, media, content_type, b, &mut report).await?;
            resources += 1;
        }
        let now = ctx.clock.now();
        if now - last_extend >= extend_every {
            // Extend by the time consumed since the last extension: the headroom the claim
            // started with is kept, never grown.
            let by = (now - last_extend)
                .to_std()
                .unwrap_or(settings.extend_every);
            let (frontier, permit) = (ctx.frontier, ctx.permit);
            if let Err(e) = frontier.extend(permit, by).await {
                failure = Some(Error::LeaseLost(e));
                break;
            }
            last_extend = now;
        }
    }
    drop(body);

    if failure.is_none() {
        decoded.clear();
        let mut ready = Vec::new();
        let tail = decoder.finish(&mut decoded).and_then(|()| {
            let mut add = |u: &[u8]| -> Result<()> {
                seen += 1;
                batcher.add(filter.filter(u)?, &mut ready, &mut kept);
                Ok(())
            };
            splitter.push(&decoded, &mut add)?;
            splitter.finish(&mut add)
        });
        if let Err(e) = tail {
            failure = Some(e);
        }
        ready.extend(batcher.flush());
        for b in ready {
            append_resource(ctx, &url, media, content_type, b, &mut report).await?;
            resources += 1;
        }
    }

    let complete = failure.is_none();
    manifest["size"] = report.bytes.into();
    manifest["sha256"] = if complete {
        hex(&sha.finish()).into()
    } else {
        serde_json::Value::Null
    };
    manifest["units_seen"] = seen.into();
    manifest["units_kept"] = kept.into();
    manifest["resource_records"] = resources.into();
    manifest["complete"] = complete.into();
    manifest["ended_at"] = ctx.clock.now().to_rfc3339().into();
    if let Some(e) = &failure {
        manifest["error"] = format!("{:?}", e.class()).into();
    }
    append_manifest(ctx, &url, manifest, &mut report).await?;
    report.complete = complete;
    match failure {
        Some(e) => Err(e),
        None => Ok(report),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(split: Split, chunks: &[&[u8]]) -> Vec<String> {
        let mut s = Splitter::new(split, 1 << 20);
        let mut out = Vec::new();
        for c in chunks {
            s.push(c, &mut |u| {
                out.push(String::from_utf8_lossy(u).into_owned());
                Ok(())
            })
            .unwrap();
        }
        s.finish(&mut |u| {
            out.push(String::from_utf8_lossy(u).into_owned());
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn splits_across_chunk_boundaries() {
        assert_eq!(
            units(Split::Lines, &[b"a\r\nb", b"c\n", b"d"]),
            vec!["a", "bc", "d"]
        );
        assert_eq!(
            units(Split::Csv, &[b"h1,h2\n1,\"x\ny", b"\"\n2,", b"z\n"]),
            vec!["h1,h2", "1,\"x\ny\"", "2,z"]
        );
        let el = Split::Element {
            open: b"<page>",
            close: b"</page>",
        };
        assert_eq!(
            units(el, &[b"<x><pa", b"ge>1</pa", b"ge>  <page>2</page></x>"]),
            vec!["<page>1</page>", "<page>2</page>"]
        );
    }

    #[test]
    fn unit_cap_is_enforced() {
        let mut s = Splitter::new(Split::Lines, 8);
        assert!(s.push(b"0123456789", &mut |_| Ok(())).is_err());
    }

    #[test]
    fn bzip2_multistream_and_truncation() {
        use std::io::Write;
        let mut all = Vec::new();
        for part in [&b"hello "[..], b"world"] {
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
            e.write_all(part).unwrap();
            all.extend(e.finish().unwrap());
        }
        let mut d = Decoder::new(Compression::Bzip2);
        let mut out = Vec::new();
        for c in all.chunks(7) {
            d.push(c, &mut out).unwrap();
        }
        d.finish(&mut out).unwrap();
        assert_eq!(out, b"hello world");

        let mut d = Decoder::new(Compression::Bzip2);
        let mut out = Vec::new();
        d.push(&all[..all.len() / 3], &mut out).unwrap();
        assert!(d.finish(&mut out).is_err());
    }

    #[test]
    fn gzip_roundtrip() {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(b"a,b\n1,2\n").unwrap();
        let gz = e.finish().unwrap();
        let mut d = Decoder::new(Compression::Gzip);
        let mut out = Vec::new();
        for c in gz.chunks(5) {
            d.push(c, &mut out).unwrap();
        }
        d.finish(&mut out).unwrap();
        assert_eq!(out, b"a,b\n1,2\n");
    }

    #[test]
    fn default_matches_config() {
        let c = DumpSettings::from_config(&vc_config::SourcesConfig::default());
        let d = DumpSettings::DEFAULT;
        assert_eq!(
            (
                c.max_bytes,
                c.extend_every,
                c.window_utc,
                c.batch_bytes,
                c.max_unit_bytes
            ),
            (
                d.max_bytes,
                d.extend_every,
                d.window_utc,
                d.batch_bytes,
                d.max_unit_bytes
            )
        );
    }

    #[test]
    fn window_wraps() {
        let mut s = DumpSettings {
            window_utc: (2, 5),
            ..DumpSettings::default()
        };
        assert!(s.in_window(2) && s.in_window(4) && !s.in_window(5) && !s.in_window(1));
        s.window_utc = (22, 3);
        assert!(s.in_window(23) && s.in_window(0) && !s.in_window(3) && !s.in_window(12));
        s.window_utc = (0, 0);
        assert!(s.in_window(13));
    }
}
