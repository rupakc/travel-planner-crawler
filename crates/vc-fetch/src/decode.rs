//! Streaming content decoding with a decoded-byte cap, plus a probe that counts the decoded size
//! of nested gzip payloads (`.gz` files, possibly gzip-in-gzip) without keeping their output.
//!
//! Everything is push-based (`std::io::Write` decoders): each compressed chunk is decoded into a
//! capped sink, so a decompression bomb stops at the cap (plus one internal buffer) instead of
//! inflating fully.

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use flate2::write::{MultiGzDecoder, ZlibDecoder};

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// Why decoding stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The decoded body (or a nested gzip layer) exceeded the cap, or gzip is nested deeper
    /// than allowed.
    Over,
    /// The `Content-Encoding` stream is corrupt.
    Corrupt(String),
}

/// A sink that keeps at most `limit` bytes and fails the write that would exceed it.
#[derive(Debug)]
struct CapSink {
    out: Vec<u8>,
    total: u64,
    limit: u64,
    over: bool,
}

impl Write for CapSink {
    fn write(&mut self, d: &[u8]) -> io::Result<usize> {
        let room = self.limit.saturating_sub(self.total);
        let len = d.len() as u64;
        if len > room {
            let r = usize::try_from(room).unwrap_or(usize::MAX).min(d.len());
            self.out.extend_from_slice(&d[..r]);
            self.total = self.limit;
            self.over = true;
            return Err(io::Error::other("decoded body cap"));
        }
        self.out.extend_from_slice(d);
        self.total += len;
        Ok(d.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

enum Content {
    Identity(CapSink),
    Gzip(Box<MultiGzDecoder<CapSink>>),
    Deflate(Box<ZlibDecoder<CapSink>>),
}

impl Content {
    fn sink(&mut self) -> &mut CapSink {
        match self {
            Content::Identity(s) => s,
            Content::Gzip(d) => d.get_mut(),
            Content::Deflate(d) => d.get_mut(),
        }
    }
}

/// Counts the decoded size of nested gzip layers (output discarded).
struct Probe {
    limit: u64,
    total: u64,
    depth_left: u8,
    head: Vec<u8>,
    state: ProbeState,
    overflow: Arc<AtomicBool>,
}

enum ProbeState {
    Undecided,
    Plain,
    Gz(Box<MultiGzDecoder<Probe>>),
    /// Not valid gzip after all: not our concern (the parser deals with it).
    Dead,
}

impl Probe {
    fn new(limit: u64, depth_left: u8, overflow: Arc<AtomicBool>) -> Self {
        Probe {
            limit,
            total: 0,
            depth_left,
            head: Vec::new(),
            state: ProbeState::Undecided,
            overflow,
        }
    }

    fn over(&self) -> io::Error {
        self.overflow.store(true, Ordering::SeqCst);
        io::Error::other("nested gzip cap")
    }

    fn feed_gz(&mut self, d: &[u8]) -> io::Result<()> {
        if let ProbeState::Gz(dec) = &mut self.state
            && dec.write_all(d).is_err()
        {
            if self.overflow.load(Ordering::SeqCst) {
                return Err(io::Error::other("nested gzip cap"));
            }
            self.state = ProbeState::Dead;
        }
        Ok(())
    }

    fn finish(&mut self) -> io::Result<()> {
        if let ProbeState::Gz(dec) = &mut self.state {
            // Flush this layer's decoder, then the layer below it (write decoders buffer).
            let r = dec.try_finish().and_then(|()| dec.get_mut().finish());
            if r.is_err() && self.overflow.load(Ordering::SeqCst) {
                return Err(io::Error::other("nested gzip cap"));
            }
        }
        Ok(())
    }
}

impl Write for Probe {
    fn write(&mut self, d: &[u8]) -> io::Result<usize> {
        self.total = self.total.saturating_add(d.len() as u64);
        if self.total > self.limit {
            return Err(self.over());
        }
        match self.state {
            ProbeState::Plain | ProbeState::Dead => {}
            ProbeState::Gz(_) => self.feed_gz(d)?,
            ProbeState::Undecided => {
                self.head.extend_from_slice(d);
                if self.head.len() >= GZIP_MAGIC.len() {
                    let head = std::mem::take(&mut self.head);
                    if head[..2] == GZIP_MAGIC {
                        if self.depth_left == 0 {
                            return Err(self.over());
                        }
                        let inner =
                            Probe::new(self.limit, self.depth_left - 1, self.overflow.clone());
                        self.state = ProbeState::Gz(Box::new(MultiGzDecoder::new(inner)));
                        self.feed_gz(&head)?;
                    } else {
                        self.state = ProbeState::Plain;
                    }
                }
            }
        }
        Ok(d.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The supported `Content-Encoding`s (we only advertise these in `Accept-Encoding`).
pub(crate) fn supported_encoding(enc: &str) -> bool {
    matches!(enc, "identity" | "gzip" | "x-gzip" | "deflate")
}

/// A push decoder: content-decodes into a capped buffer and probes nested gzip.
pub(crate) struct BodyDecoder {
    content: Content,
    probe: Option<Probe>,
    overflow: Arc<AtomicBool>,
    fed: u64,
    failed: bool,
}

impl BodyDecoder {
    /// A decoder for `encoding` (lowercase; `None` = identity) with a decoded cap of `limit`
    /// bytes. `nested` = `Some(depth)` probes up to `depth` nested gzip layers of the payload
    /// against the same cap.
    pub(crate) fn new(encoding: Option<&str>, limit: u64, nested: Option<u8>) -> Option<Self> {
        let sink = CapSink {
            out: Vec::new(),
            total: 0,
            limit,
            over: false,
        };
        let content = match encoding.unwrap_or("identity") {
            "identity" => Content::Identity(sink),
            "gzip" | "x-gzip" => Content::Gzip(Box::new(MultiGzDecoder::new(sink))),
            "deflate" => Content::Deflate(Box::new(ZlibDecoder::new(sink))),
            _ => return None,
        };
        let overflow = Arc::new(AtomicBool::new(false));
        let probe = nested.map(|d| Probe::new(limit, d, overflow.clone()));
        Some(BodyDecoder {
            content,
            probe,
            overflow,
            fed: 0,
            failed: false,
        })
    }

    fn classify(&mut self, e: &io::Error) -> DecodeError {
        self.failed = true;
        if self.content.sink().over || self.overflow.load(Ordering::SeqCst) {
            DecodeError::Over
        } else {
            DecodeError::Corrupt(e.to_string())
        }
    }

    fn after_write(&mut self, start: usize) -> Result<(), DecodeError> {
        if let Some(probe) = self.probe.as_mut() {
            let new = self.content.sink().out[start..].to_vec();
            if let Err(e) = probe.write_all(&new) {
                return Err(self.classify(&e));
            }
        }
        Ok(())
    }

    /// Pushes one received (encoded) chunk.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<(), DecodeError> {
        if self.failed {
            return Err(DecodeError::Over);
        }
        self.fed += chunk.len() as u64;
        let start = self.content.sink().out.len();
        let r = match &mut self.content {
            Content::Identity(s) => s.write_all(chunk),
            Content::Gzip(d) => d.write_all(chunk),
            Content::Deflate(d) => d.write_all(chunk),
        };
        if let Err(e) = r {
            // Keep the probe in step with what was decoded before the failure.
            let _ = self.after_write(start);
            return Err(self.classify(&e));
        }
        self.after_write(start)
    }

    /// Flushes the decoder at the end of the body.
    pub(crate) fn finish(&mut self) -> Result<(), DecodeError> {
        if self.failed {
            return Err(DecodeError::Over);
        }
        let start = self.content.sink().out.len();
        if self.fed > 0 {
            let r = match &mut self.content {
                Content::Identity(_) => Ok(()),
                Content::Gzip(d) => d.try_finish(),
                Content::Deflate(d) => d.try_finish(),
            };
            if let Err(e) = r {
                let _ = self.after_write(start);
                return Err(self.classify(&e));
            }
        }
        self.after_write(start)?;
        if let Some(probe) = self.probe.as_mut()
            && let Err(e) = probe.finish()
        {
            return Err(self.classify(&e));
        }
        Ok(())
    }

    /// Takes the decoded bytes produced so far.
    pub(crate) fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.content.sink().out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn gz(d: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::fast());
        e.write_all(d).unwrap();
        e.finish().unwrap()
    }

    fn run(
        enc: Option<&str>,
        limit: u64,
        nested: Option<u8>,
        body: &[u8],
    ) -> Result<Vec<u8>, DecodeError> {
        let mut d = BodyDecoder::new(enc, limit, nested).unwrap();
        for c in body.chunks(1000) {
            d.push(c)?;
        }
        d.finish()?;
        Ok(d.take())
    }

    #[test]
    fn identity_and_gzip_roundtrip() {
        assert_eq!(run(None, 10, None, b"hello").unwrap(), b"hello");
        assert_eq!(
            run(Some("gzip"), 10, None, &gz(b"hello")).unwrap(),
            b"hello"
        );
        assert_eq!(run(None, 4, None, b"hello"), Err(DecodeError::Over));
        assert!(BodyDecoder::new(Some("br"), 1, None).is_none());
    }

    #[test]
    fn truncation_keeps_prefix() {
        let mut d = BodyDecoder::new(None, 3, None).unwrap();
        assert_eq!(d.push(b"hello"), Err(DecodeError::Over));
        assert_eq!(d.take(), b"hel");
    }

    #[test]
    fn corrupt_gzip_is_corrupt() {
        assert!(matches!(
            run(Some("gzip"), 100, None, b"\x1f\x8bnot gzip at all"),
            Err(DecodeError::Corrupt(_))
        ));
    }

    #[test]
    fn nested_probe_depths() {
        let one = gz(b"payload");
        let two = gz(&one);
        let three = gz(&two);
        assert_eq!(run(None, 1000, Some(2), &one).unwrap(), one);
        assert_eq!(run(None, 1000, Some(2), &two).unwrap(), two);
        assert_eq!(run(None, 1000, Some(2), &three), Err(DecodeError::Over));
        assert_eq!(run(None, 1000, Some(0), &one), Err(DecodeError::Over));
        // A payload that only looks like gzip is not an error.
        assert!(run(None, 1000, Some(2), b"\x1f\x8bjunk").is_ok());
        // The inner layer is capped too.
        let bomb = gz(&vec![0u8; 100_000]);
        assert_eq!(run(None, 50_000, Some(2), &bomb), Err(DecodeError::Over));
    }
}
