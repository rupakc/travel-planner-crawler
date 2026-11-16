//! WARC writer/reader, CDXJ and revisit dedupe.

mod common;

use bytes::Bytes;
use common::*;
use http::{HeaderName, HeaderValue};
use proptest::prelude::*;
use vc_core::store::{CaptureRole, ManifestCapture, SpoolPayload, SpoolRecord};
use vc_core::{CaptureId, CaptureRef};
use vc_store::cdxj::{CdxjEntry, parse_cdxj, write_cdxj};
use vc_store::warc::{
    LastPayload, PROFILE_IDENTICAL_PAYLOAD, PROFILE_SERVER_NOT_MODIFIED, dedupe_payload, encode,
    parse_http, payload_hash, read_records, record_id_for, verify_payload_digest,
};

fn header_name() -> impl Strategy<Value = String> {
    "x-[a-z]{1,12}"
}

fn header_value() -> impl Strategy<Value = String> {
    "[ -~]{0,40}".prop_map(|s| s.trim().to_string())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Any response capture survives encode → read: types, ids, URL, status, headers (minus the
    /// encoding headers, with Content-Length rewritten), body and digests.
    #[test]
    fn warc_roundtrip(
        body in proptest::collection::vec(any::<u8>(), 0..4096),
        status in prop_oneof![Just(200u16), Just(404), Just(500), Just(206)],
        path in "/[a-z0-9/]{0,30}",
        extra in proptest::collection::vec((header_name(), header_value()), 0..6),
        truncated in any::<bool>(),
    ) {
        let u = url(&format!("https://www.example.com{path}"));
        let mut ex = exchange(&u, status, &body);
        ex.truncated = truncated;
        for (n, v) in &extra {
            ex.response_headers.append(
                HeaderName::from_bytes(n.as_bytes()).expect("name"),
                HeaderValue::from_str(v).expect("value"),
            );
        }
        let m = meta(&u, t0(), 3);
        let id = m.capture_id;
        let rec = SpoolRecord { meta: m, payload: SpoolPayload::Response(ex.clone()) };
        let enc = encode(&rec).expect("encode");
        prop_assert_eq!(enc.record_count, 2);
        prop_assert_eq!(&enc.warc_record_id, &record_id_for(id));
        // Deterministic.
        prop_assert_eq!(&encode(&rec).expect("encode").bytes, &enc.bytes);

        let recs = read_records(&enc.bytes).expect("read");
        prop_assert_eq!(recs.len(), 2);
        prop_assert_eq!(recs[0].warc_type(), Some("request"));
        prop_assert_eq!(recs[1].warc_type(), Some("response"));
        prop_assert_eq!(recs[0].header("WARC-Concurrent-To"), recs[1].record_id());
        for r in &recs {
            prop_assert_eq!(r.version.as_str(), "WARC/1.1");
            prop_assert_eq!(r.target_uri(), Some(u.as_str()));
            prop_assert_eq!(r.capture_id(), Some(id));
            let block_digest = format!("sha256:{}", payload_hash(&r.block).to_hex());
            prop_assert_eq!(r.header("WARC-Block-Digest"), Some(block_digest.as_str()));
        }
        prop_assert_eq!(recs[1].header("WARC-Voyager-Original-Encoding"), Some("gzip"));
        prop_assert_eq!(recs[1].header("WARC-Truncated").is_some(), truncated);
        prop_assert!(verify_payload_digest(&recs[1]).expect("digest"));

        let req = parse_http(&recs[0].block).expect("request");
        prop_assert!(req.start_line.starts_with("GET "));
        prop_assert_eq!(req.header("user-agent"), Some(vc_core::USER_AGENT.as_bytes()));

        let resp = parse_http(&recs[1].block).expect("response");
        prop_assert_eq!(resp.status(), Some(status));
        prop_assert_eq!(&resp.body[..], &body[..]);
        prop_assert!(resp.header("content-encoding").is_none());
        let len = body.len().to_string();
        prop_assert_eq!(resp.header("content-length"), Some(len.as_bytes()));
        prop_assert_eq!(resp.header("etag"), Some(&b"\"v1\""[..]));
        for (n, v) in &extra {
            prop_assert!(resp.headers.iter().any(|(hn, hv)| hn == n && hv == v.as_bytes()));
        }
    }

    /// The reader never panics, whatever the bytes (the CI-run counterpart of fuzz_warc_reader).
    #[test]
    fn warc_reader_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = read_records(&bytes);
        let mut prefixed = b"WARC/1.1\r\nContent-Length: 5\r\n\r\n".to_vec();
        prefixed.extend_from_slice(&bytes);
        if let Ok(recs) = read_records(&prefixed) {
            for r in recs { let _ = parse_http(&r.block); let _ = verify_payload_digest(&r); }
        }
    }
}

#[test]
fn reader_rejects_lying_lengths() {
    for bad in [
        &b"WARC/1.1\r\nContent-Length: 99999999999\r\n\r\nabc"[..],
        &b"WARC/1.1\r\nContent-Length: 3\r\n\r\nabc"[..],
        &b"WARC/1.1\r\nContent-Length: -1\r\n\r\n"[..],
        &b"HTTP/1.1 200 OK\r\n\r\n"[..],
        &b"WARC/1.1\r\nContent-Length: 3\r\n\r\nabcXXXX"[..],
    ] {
        assert!(read_records(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn resource_and_metadata_records() {
    let mut r = response("https://example.com/a", t0(), b"x");
    r.payload = SpoolPayload::Resource {
        content_type: "text/html".into(),
        body: Bytes::from_static(b"<html>dom</html>"),
    };
    let recs = read_records(&encode(&r).expect("enc").bytes).expect("read");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].warc_type(), Some("resource"));
    assert_eq!(&recs[0].block[..], b"<html>dom</html>");
    assert!(verify_payload_digest(&recs[0]).expect("digest"));

    r.payload = SpoolPayload::Metadata {
        fields: serde_json::json!({"dump_manifest": {"files": 3}}),
    };
    let recs = read_records(&encode(&r).expect("enc").bytes).expect("read");
    assert_eq!(recs[0].warc_type(), Some("metadata"));
    let v: serde_json::Value = serde_json::from_slice(&recs[0].block).expect("json");
    assert_eq!(v["dump_manifest"]["files"], 3);
}

#[test]
fn cdxj_roundtrip() {
    let mut entries = Vec::new();
    for (i, u) in [
        "https://www.example.com/b?q=1",
        "https://example.com:8443/a",
        "https://a.example.org/",
    ]
    .iter()
    .enumerate()
    {
        let mut r = response(u, t0() + chrono::Duration::seconds(i as i64), b"body");
        r.meta.payload_hash = Some(payload_hash(b"body"));
        r.meta.role = if i == 2 {
            CaptureRole::Robots
        } else {
            CaptureRole::Primary
        };
        r.meta.revisit_of = (i == 1).then(|| CaptureRef {
            capture_id: CaptureId::at(t0()),
            captured_at: t0(),
        });
        entries.push(CdxjEntry {
            capture: ManifestCapture {
                meta: r.meta,
                offset: 100 * i as u64,
                length: 100,
                warc_record_id: format!("<urn:uuid:{i}>"),
            },
            filename: "segments/2026/11/15/X.warc.zst".into(),
            records: 2,
        });
    }
    let text = write_cdxj(&entries).expect("write");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    // Sorted by SURT key; `www.` dropped; non-default port kept.
    assert!(
        lines[0].starts_with("com,example)/b?q=1 20261115120000 {"),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with("com,example:8443)/a 20261115120001 {"),
        "{}",
        lines[1]
    );
    assert!(lines[2].starts_with("org,example,a)/ "), "{}", lines[2]);
    let mut back = parse_cdxj(&text).expect("parse");
    back.sort_by_key(|e| e.capture.offset);
    assert_eq!(back, entries);
    assert!(parse_cdxj("garbage").is_err());
}

#[test]
fn revisit_on_same_payload() {
    let body = b"<html>same</html>";
    let earlier = CaptureRef {
        capture_id: CaptureId::at(t0()),
        captured_at: t0(),
    };
    let last = LastPayload {
        hash: payload_hash(body),
        capture: earlier,
    };

    // Same payload → revisit, identical-payload-digest, body not stored again.
    let r = dedupe_payload(response("https://example.com/p", t0(), body), Some(&last));
    assert!(matches!(r.payload, SpoolPayload::Revisit(_)));
    assert_eq!(r.meta.revisit_of, Some(earlier));
    assert_eq!(r.meta.payload_hash, Some(last.hash));
    let enc = encode(&r).expect("enc");
    let recs = read_records(&enc.bytes).expect("read");
    assert_eq!(recs[1].warc_type(), Some("revisit"));
    assert_eq!(
        recs[1].header("WARC-Profile"),
        Some(PROFILE_IDENTICAL_PAYLOAD)
    );
    assert_eq!(
        recs[1].header("WARC-Refers-To"),
        Some(record_id_for(earlier.capture_id).as_str())
    );
    assert_eq!(
        recs[1].header("WARC-Payload-Digest"),
        Some(last.hash.warc_digest().as_str())
    );
    let head = parse_http(&recs[1].block).expect("http");
    assert!(
        head.body.is_empty(),
        "revisit must not store the body again"
    );
    assert!(!enc.bytes.windows(body.len()).any(|w| w == body));

    // Different payload → unchanged response.
    let r = dedupe_payload(
        response("https://example.com/p", t0(), b"changed"),
        Some(&last),
    );
    assert!(matches!(r.payload, SpoolPayload::Response(_)));
    // No history → unchanged.
    let r = dedupe_payload(response("https://example.com/p", t0(), body), None);
    assert!(matches!(r.payload, SpoolPayload::Response(_)));

    // A 304 is a server-not-modified revisit.
    let mut r = response("https://example.com/p", t0(), b"");
    let SpoolPayload::Response(mut ex) = r.payload else {
        unreachable!()
    };
    ex.status = http::StatusCode::NOT_MODIFIED;
    r.payload = SpoolPayload::Revisit(ex);
    r.meta.status = Some(304);
    r.meta.revisit_of = Some(earlier);
    let recs = read_records(&encode(&r).expect("enc").bytes).expect("read");
    assert_eq!(
        recs[1].header("WARC-Profile"),
        Some(PROFILE_SERVER_NOT_MODIFIED)
    );
    assert_eq!(
        parse_http(&recs[1].block).expect("http").status(),
        Some(304)
    );
}
