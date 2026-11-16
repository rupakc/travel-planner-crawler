//! Text helpers: decoding, whitespace, truncation, the CPU deadline, gzip.

use std::io::Read;
use std::time::{Duration, Instant};

use encoding_rs::{Encoding, UTF_8};
use vc_core::{Error, Result};

/// Max anchor / title length kept, in characters (vc-core `Link::anchor`).
pub(crate) const MAX_ANCHOR_CHARS: usize = 200;

/// Max decompressed bytes read from a gzip body (the sitemap protocol limit is 50 MB).
pub(crate) const MAX_GUNZIP_BYTES: u64 = 50 * 1024 * 1024;

/// The gzip magic.
pub(crate) fn is_gzip(body: &[u8]) -> bool {
    body.starts_with(&[0x1f, 0x8b])
}

/// Decompresses one gzip layer, capped at `cap` bytes (more is an error: no silent truncation
/// of XML, which would only produce a parse error later).
pub(crate) fn gunzip(body: &[u8], cap: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(body)
        .take(cap + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::Invalid(format!("gzip: {e}")))?;
    if out.len() as u64 > cap {
        return Err(Error::Invalid(format!(
            "gzip body exceeds {cap} bytes decompressed"
        )));
    }
    Ok(out)
}

/// Strips one gzip layer if present.
pub(crate) fn maybe_gunzip(body: &[u8]) -> Result<std::borrow::Cow<'_, [u8]>> {
    if is_gzip(body) {
        Ok(std::borrow::Cow::Owned(gunzip(body, MAX_GUNZIP_BYTES)?))
    } else {
        Ok(std::borrow::Cow::Borrowed(body))
    }
}

/// Decodes bytes as text: BOM, then a declared charset (`declared`), else UTF-8 (lossy).
pub(crate) fn decode(body: &[u8], declared: Option<&str>) -> String {
    if let Some((enc, bom)) = Encoding::for_bom(body) {
        return enc.decode_without_bom_handling(&body[bom..]).0.into_owned();
    }
    let enc = declared
        .and_then(|l| Encoding::for_label(l.trim().as_bytes()))
        // A page cannot declare itself UTF-16 in ASCII-compatible markup (WHATWG rule).
        .map(|e| if e.is_single_byte() || e == UTF_8 { e } else { e.output_encoding() })
        .unwrap_or(UTF_8);
    enc.decode_without_bom_handling(body).0.into_owned()
}

/// Collapses runs of whitespace into one space and trims.
pub(crate) fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Truncates to at most `max` bytes on a char boundary.
pub(crate) fn truncate_bytes(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// Collapses whitespace and keeps at most `MAX_ANCHOR_CHARS` characters.
pub(crate) fn short_text(s: &str) -> String {
    let c = collapse_ws(s);
    match c.char_indices().nth(MAX_ANCHOR_CHARS) {
        Some((i, _)) => c[..i].trim_end().to_string(),
        None => c,
    }
}

/// The per-capture CPU budget (`Limits::cpu_time`), checked between stages.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deadline {
    start: Instant,
    budget: Duration,
}

impl Deadline {
    pub(crate) fn new(budget: Duration) -> Self {
        Deadline {
            start: Instant::now(),
            budget,
        }
    }

    /// Whether the budget is spent.
    pub(crate) fn expired(&self) -> bool {
        self.start.elapsed() > self.budget
    }

    /// Fails with a permanent error once the budget is spent.
    pub(crate) fn check(&self, stage: &str) -> Result<()> {
        if self.expired() {
            Err(Error::Invalid(format!(
                "parse cpu budget of {:?} exceeded at {stage}",
                self.budget
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_bytes("héllo".into(), 2), "h");
        assert_eq!(truncate_bytes("abc".into(), 10), "abc");
        assert_eq!(short_text("  a \n b  "), "a b");
        assert_eq!(
            short_text(&"é".repeat(300)).chars().count(),
            MAX_ANCHOR_CHARS
        );
    }

    #[test]
    fn decode_bom_and_label() {
        assert_eq!(decode(b"\xEF\xBB\xBFhi", None), "hi");
        assert_eq!(decode(b"caf\xE9", Some("latin1")), "café");
        assert_eq!(decode(b"caf\xC3\xA9", Some("utf-16")), "café");
        assert_eq!(decode(b"x", Some("bogus")), "x");
    }

    #[test]
    fn gunzip_is_capped() {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(&[b'a'; 10_000]).unwrap();
        let gz = e.finish().unwrap();
        assert_eq!(gunzip(&gz, 10_000).unwrap().len(), 10_000);
        assert!(gunzip(&gz, 9_999).is_err());
        assert!(gunzip(b"\x1f\x8bjunk", 100).is_err());
    }
}
