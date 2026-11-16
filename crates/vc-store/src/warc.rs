//! WARC/1.1 writer and reader (spec §5.5, r5 Q2). `vc-store::warc` is the only serialiser of
//! capture records.
//!
//! * Bodies are stored **content-decoded**: `Content-Encoding` and `Transfer-Encoding` are
//!   dropped, `Content-Length` is rewritten to the decoded length, and the original encoding is
//!   kept in `WARC-Voyager-Original-Encoding`. `WARC-Payload-Digest` is `sha256:<hex>` of the
//!   decoded body.
//! * A [`SpoolPayload::Response`] becomes a `request` + `response` pair, a
//!   [`SpoolPayload::Revisit`] a `request` + `revisit` pair (profile `server-not-modified` for a
//!   304, `identical-payload-digest` otherwise), [`SpoolPayload::Resource`] one `resource`
//!   record and [`SpoolPayload::Metadata`] one `metadata` record.
//! * Record ids are derived from the capture id, so re-encoding the same capture yields the same
//!   bytes (replays are byte-identical, which keeps uploads idempotent).
//!
//! The reader is defensive (it parses untrusted bytes from storage): every length is checked
//! against the remaining input, and malformed input is an [`Error::Invalid`], never a panic.

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use http::HeaderMap;
use sha2::{Digest, Sha256};
use vc_core::store::{SpoolPayload, SpoolRecord};
use vc_core::{CaptureId, CaptureRef, Error, PayloadHash, Result};

/// The WARC version line we write.
pub const WARC_VERSION: &str = "WARC/1.1";
/// Revisit profile for a `304 Not Modified`.
pub const PROFILE_SERVER_NOT_MODIFIED: &str =
    "http://netpreserve.org/warc/1.1/revisit/server-not-modified";
/// Revisit profile for a body identical to an earlier capture's.
pub const PROFILE_IDENTICAL_PAYLOAD: &str =
    "http://netpreserve.org/warc/1.1/revisit/identical-payload-digest";
/// Header carrying the original `Content-Encoding` of a decoded body.
pub const H_ORIGINAL_ENCODING: &str = "WARC-Voyager-Original-Encoding";
/// Header carrying the capture id.
pub const H_CAPTURE_ID: &str = "WARC-Voyager-Capture-Id";
/// Header carrying one followed redirect hop (`<status> <url>`), repeated per hop.
pub const H_REDIRECT: &str = "WARC-Voyager-Redirect";

/// Largest header section we accept when reading (64 KiB).
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// SHA-256 of a decoded payload.
pub fn payload_hash(body: &[u8]) -> PayloadHash {
    let d = Sha256::digest(body);
    let mut out = [0u8; 32];
    out.copy_from_slice(&d);
    PayloadHash(out)
}

fn uuid_urn(b: [u8; 16]) -> String {
    let h = hex::encode(b);
    format!(
        "<urn:uuid:{}-{}-{}-{}-{}>",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// The `WARC-Record-ID` of a capture's main record (response, revisit, resource or metadata).
pub fn record_id_for(capture: CaptureId) -> String {
    uuid_urn(capture.0.to_bytes())
}

/// The `WARC-Record-ID` of a capture's `request` record.
pub fn request_record_id_for(capture: CaptureId) -> String {
    let mut h = Sha256::new();
    h.update(capture.0.to_bytes());
    h.update(b"/request");
    let d = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    uuid_urn(b)
}

fn warc_date(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// One encoded capture: its WARC record group, uncompressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedCapture {
    /// The record bytes (one or two WARC records).
    pub bytes: Bytes,
    /// Number of WARC records in `bytes`.
    pub record_count: u32,
    /// The main record's `WARC-Record-ID`.
    pub warc_record_id: String,
}

struct RecordBuilder {
    headers: Vec<(String, Vec<u8>)>,
}

impl RecordBuilder {
    fn new(warc_type: &str, id: &str, date: DateTime<Utc>) -> Self {
        let mut b = RecordBuilder {
            headers: Vec::new(),
        };
        b.h("WARC-Type", warc_type);
        b.h("WARC-Record-ID", id);
        b.h("WARC-Date", &warc_date(date));
        b
    }

    fn h(&mut self, name: &str, value: &str) {
        self.headers
            .push((name.to_string(), value.as_bytes().to_vec()));
    }

    fn finish(mut self, content_type: &str, block: &[u8], out: &mut Vec<u8>) {
        self.h("Content-Type", content_type);
        let digest = payload_hash(block);
        self.h("WARC-Block-Digest", &digest.warc_digest());
        self.h("Content-Length", &block.len().to_string());
        out.extend_from_slice(WARC_VERSION.as_bytes());
        out.extend_from_slice(b"\r\n");
        for (n, v) in &self.headers {
            out.extend_from_slice(n.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(&sanitize(v));
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(block);
        out.extend_from_slice(b"\r\n\r\n");
    }
}

/// Header values must not break the line structure.
fn sanitize(v: &[u8]) -> Vec<u8> {
    v.iter()
        .map(|&c| if c == b'\r' || c == b'\n' { b' ' } else { c })
        .collect()
}

fn write_headers(out: &mut Vec<u8>, headers: &HeaderMap, skip: &[&str]) {
    for (n, v) in headers {
        if skip.iter().any(|s| n.as_str().eq_ignore_ascii_case(s)) {
            continue;
        }
        out.extend_from_slice(n.as_str().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(&sanitize(v.as_bytes()));
        out.extend_from_slice(b"\r\n");
    }
}

fn request_block(ex: &vc_core::fetch::Exchange) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(ex.method.as_str().as_bytes());
    b.push(b' ');
    b.extend_from_slice(ex.url.path_and_query().as_bytes());
    b.extend_from_slice(b" HTTP/1.1\r\n");
    if !ex.request_headers.contains_key(http::header::HOST) {
        let u = ex.url.as_url();
        let host = match u.port() {
            Some(p) => format!("{}:{p}", ex.url.host()),
            None => ex.url.host().to_string(),
        };
        b.extend_from_slice(b"host: ");
        b.extend_from_slice(host.as_bytes());
        b.extend_from_slice(b"\r\n");
    }
    write_headers(&mut b, &ex.request_headers, &[]);
    b.extend_from_slice(b"\r\n");
    b
}

/// The response head (status line + headers). With `body_len`, `Content-Length` is rewritten
/// to it; `Content-Encoding` and `Transfer-Encoding` are always dropped.
fn response_head(ex: &vc_core::fetch::Exchange, body_len: Option<usize>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(
        format!(
            "HTTP/1.1 {} {}\r\n",
            ex.status.as_u16(),
            ex.status.canonical_reason().unwrap_or("")
        )
        .as_bytes(),
    );
    write_headers(
        &mut b,
        &ex.response_headers,
        &["content-encoding", "transfer-encoding", "content-length"],
    );
    if let Some(n) = body_len {
        b.extend_from_slice(format!("content-length: {n}\r\n").as_bytes());
    }
    b.extend_from_slice(b"\r\n");
    b
}

fn exchange_common(rb: &mut RecordBuilder, ex: &vc_core::fetch::Exchange, capture: CaptureId) {
    rb.h("WARC-Target-URI", ex.url.as_str());
    if let Some(ip) = ex.peer_ip {
        rb.h("WARC-IP-Address", &ip.to_string());
    }
    rb.h(H_CAPTURE_ID, &capture.to_string());
}

/// Encodes one spool record as its WARC record group. Pure and deterministic.
pub fn encode(r: &SpoolRecord) -> Result<EncodedCapture> {
    let m = &r.meta;
    let id = record_id_for(m.capture_id);
    let mut out = Vec::new();
    let record_count = match &r.payload {
        SpoolPayload::Response(ex) | SpoolPayload::Revisit(ex) => {
            let revisit = matches!(r.payload, SpoolPayload::Revisit(_));
            let req_id = request_record_id_for(m.capture_id);
            let mut rq = RecordBuilder::new("request", &req_id, m.captured_at);
            exchange_common(&mut rq, ex, m.capture_id);
            rq.h("WARC-Concurrent-To", &id);
            rq.finish(
                "application/http;msgtype=request",
                &request_block(ex),
                &mut out,
            );

            let mut rs = RecordBuilder::new(
                if revisit { "revisit" } else { "response" },
                &id,
                m.captured_at,
            );
            exchange_common(&mut rs, ex, m.capture_id);
            for hop in &ex.redirects {
                rs.h(H_REDIRECT, &format!("{} {}", hop.status.as_u16(), hop.url));
            }
            if let Some(enc) = &ex.original_encoding {
                rs.h(H_ORIGINAL_ENCODING, enc);
            }
            let block = if revisit {
                let profile = if ex.status == http::StatusCode::NOT_MODIFIED {
                    PROFILE_SERVER_NOT_MODIFIED
                } else {
                    PROFILE_IDENTICAL_PAYLOAD
                };
                rs.h("WARC-Profile", profile);
                if let Some(of) = &m.revisit_of {
                    rs.h("WARC-Refers-To", &record_id_for(of.capture_id));
                    rs.h("WARC-Refers-To-Date", &warc_date(of.captured_at));
                    rs.h("WARC-Refers-To-Target-URI", ex.url.as_str());
                }
                if let Some(h) = &m.payload_hash {
                    rs.h("WARC-Payload-Digest", &h.warc_digest());
                }
                response_head(ex, None)
            } else {
                rs.h("WARC-Payload-Digest", &payload_hash(&ex.body).warc_digest());
                if ex.truncated {
                    rs.h("WARC-Truncated", "length");
                }
                let mut b = response_head(ex, Some(ex.body.len()));
                b.extend_from_slice(&ex.body);
                b
            };
            rs.finish("application/http;msgtype=response", &block, &mut out);
            2
        }
        SpoolPayload::Resource { content_type, body } => {
            let mut rb = RecordBuilder::new("resource", &id, m.captured_at);
            rb.h("WARC-Target-URI", m.url.as_str());
            rb.h(H_CAPTURE_ID, &m.capture_id.to_string());
            rb.h("WARC-Payload-Digest", &payload_hash(body).warc_digest());
            rb.finish(content_type, body, &mut out);
            1
        }
        SpoolPayload::Metadata { fields } => {
            let mut rb = RecordBuilder::new("metadata", &id, m.captured_at);
            rb.h("WARC-Target-URI", m.url.as_str());
            rb.h(H_CAPTURE_ID, &m.capture_id.to_string());
            let block = serde_json::to_vec(fields)?;
            rb.finish("application/json", &block, &mut out);
            1
        }
    };
    Ok(EncodedCapture {
        bytes: Bytes::from(out),
        record_count,
        warc_record_id: id,
    })
}

/// The last committed payload of a URL (from `url_state`), for revisit dedupe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastPayload {
    /// Its SHA-256.
    pub hash: PayloadHash,
    /// The capture that stored it.
    pub capture: CaptureRef,
}

/// Dedupe (spec §5.5): a `Response` whose decoded payload equals the URL's last payload becomes
/// a `Revisit` (profile `identical-payload-digest`) referring to the earlier capture, so the body
/// is not stored again. Anything else is returned unchanged.
pub fn dedupe_payload(mut r: SpoolRecord, last: Option<&LastPayload>) -> SpoolRecord {
    let Some(last) = last else { return r };
    if let SpoolPayload::Response(ex) = &r.payload {
        let h = payload_hash(&ex.body);
        if h == last.hash && ex.status.is_success() {
            let len = ex.body.len() as u64;
            let SpoolPayload::Response(ex) = r.payload else {
                unreachable!("matched above")
            };
            r.payload = SpoolPayload::Revisit(ex);
            r.meta.payload_hash = Some(h);
            r.meta.payload_len = len;
            r.meta.revisit_of = Some(last.capture);
        }
    }
    r
}

// ---------------------------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------------------------

/// One parsed WARC record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarcRecord {
    /// The version line (`WARC/1.1`, `WARC/1.0`).
    pub version: String,
    /// Header fields in order (names as written).
    pub headers: Vec<(String, String)>,
    /// The content block.
    pub block: Bytes,
}

impl WarcRecord {
    /// The first value of a header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every value of a header (case-insensitive), in order.
    pub fn headers_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.headers
            .iter()
            .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `WARC-Type`.
    pub fn warc_type(&self) -> Option<&str> {
        self.header("WARC-Type")
    }

    /// `WARC-Record-ID`.
    pub fn record_id(&self) -> Option<&str> {
        self.header("WARC-Record-ID")
    }

    /// `WARC-Target-URI`.
    pub fn target_uri(&self) -> Option<&str> {
        self.header("WARC-Target-URI")
    }

    /// The capture id (`WARC-Voyager-Capture-Id`).
    pub fn capture_id(&self) -> Option<CaptureId> {
        self.header(H_CAPTURE_ID).and_then(|v| v.parse().ok())
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::Invalid(format!("warc: {}", msg.into()))
}

fn find_crlfcrlf(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Iterates the records of an uncompressed WARC byte string.
#[derive(Debug, Clone)]
pub struct WarcReader<'a> {
    buf: &'a [u8],
    pos: usize,
    failed: bool,
}

impl<'a> WarcReader<'a> {
    /// A reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        WarcReader {
            buf,
            pos: 0,
            failed: false,
        }
    }

    /// Byte offset of the next record.
    pub fn position(&self) -> usize {
        self.pos
    }

    fn read_one(&mut self) -> Result<WarcRecord> {
        let rest = &self.buf[self.pos..];
        let window = &rest[..rest.len().min(MAX_HEADER_BYTES + 4)];
        let end = find_crlfcrlf(window).ok_or_else(|| invalid("unterminated header"))?;
        let head = std::str::from_utf8(&rest[..end]).map_err(|_| invalid("header not utf-8"))?;
        let mut lines = head.split("\r\n");
        let version = lines.next().unwrap_or_default();
        if !version.starts_with("WARC/") {
            return Err(invalid("missing version line"));
        }
        let mut headers = Vec::new();
        for line in lines {
            let (n, v) = line
                .split_once(':')
                .ok_or_else(|| invalid("header without colon"))?;
            headers.push((n.trim().to_string(), v.trim().to_string()));
        }
        let len: usize = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("Content-Length"))
            .ok_or_else(|| invalid("missing Content-Length"))?
            .1
            .parse()
            .map_err(|_| invalid("bad Content-Length"))?;
        let body_start = end + 4;
        let body_end = body_start
            .checked_add(len)
            .filter(|e| *e <= rest.len())
            .ok_or_else(|| invalid("block exceeds input"))?;
        let trailer = rest
            .get(body_end..body_end + 4)
            .ok_or_else(|| invalid("missing record trailer"))?;
        if trailer != b"\r\n\r\n" {
            return Err(invalid("bad record trailer"));
        }
        let block = Bytes::copy_from_slice(&rest[body_start..body_end]);
        self.pos += body_end + 4;
        Ok(WarcRecord {
            version: version.to_string(),
            headers,
            block,
        })
    }
}

impl Iterator for WarcReader<'_> {
    type Item = Result<WarcRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.pos >= self.buf.len() {
            return None;
        }
        let r = self.read_one();
        if r.is_err() {
            self.failed = true;
        }
        Some(r)
    }
}

/// Reads every record of an uncompressed WARC byte string.
pub fn read_records(buf: &[u8]) -> Result<Vec<WarcRecord>> {
    WarcReader::new(buf).collect()
}

/// A parsed HTTP message head + body from a WARC `request`/`response`/`revisit` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpMessage {
    /// The start line (`GET /x HTTP/1.1` or `HTTP/1.1 200 OK`).
    pub start_line: String,
    /// Headers in order (names lowercase as written by us; values as raw bytes).
    pub headers: Vec<(String, Vec<u8>)>,
    /// The body (empty for requests and revisits).
    pub body: Bytes,
}

impl HttpMessage {
    /// The response status, for a response start line.
    pub fn status(&self) -> Option<u16> {
        let mut it = self.start_line.split(' ');
        let proto = it.next()?;
        if !proto.starts_with("HTTP/") {
            return None;
        }
        it.next()?.parse().ok()
    }

    /// First value of a header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }
}

/// Parses an HTTP message block (request or response).
pub fn parse_http(block: &Bytes) -> Result<HttpMessage> {
    let window = &block[..block.len().min(MAX_HEADER_BYTES + 4)];
    let end = find_crlfcrlf(window).ok_or_else(|| invalid("unterminated http head"))?;
    let head = &block[..end];
    let mut lines = head
        .split(|&c| c == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let start_line = std::str::from_utf8(lines.next().unwrap_or_default())
        .map_err(|_| invalid("http start line not utf-8"))?
        .to_string();
    let mut headers = Vec::new();
    for line in lines {
        let colon = line
            .iter()
            .position(|&c| c == b':')
            .ok_or_else(|| invalid("http header without colon"))?;
        let name = std::str::from_utf8(&line[..colon])
            .map_err(|_| invalid("http header name not utf-8"))?
            .trim()
            .to_string();
        let mut v = &line[colon + 1..];
        while let [b' ' | b'\t', r @ ..] = v {
            v = r;
        }
        while let [r @ .., b' ' | b'\t'] = v {
            v = r;
        }
        headers.push((name, v.to_vec()));
    }
    Ok(HttpMessage {
        start_line,
        headers,
        body: block.slice(end + 4..),
    })
}

/// Verifies `WARC-Payload-Digest` of a `response` or `resource` record against its payload.
pub fn verify_payload_digest(r: &WarcRecord) -> Result<bool> {
    let Some(want) = r.header("WARC-Payload-Digest") else {
        return Ok(true);
    };
    let payload = match r.warc_type() {
        Some("response") => parse_http(&r.block)?.body,
        Some("resource") => r.block.clone(),
        _ => return Ok(true),
    };
    Ok(payload_hash(&payload).warc_digest() == want)
}
