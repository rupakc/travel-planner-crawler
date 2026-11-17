//! A push-based, streaming reader of every entry of a ZIP archive (`gtfs_static`; `zip_stream` reads one member for the dump runner; PKWARE APPNOTE §4.3): the bytes of a
//! download go in chunk by chunk, each entry's **local** header is read as it arrives and the
//! entry's content is handed to an [`EntrySink`] (stored or deflated; zip64 sizes; data
//! descriptors). The archive is never held: memory is one header, one 64 KiB inflate window
//! and the caller's chunk. Entries the sink does not want are skipped without inflating when
//! their size is known, else inflated and discarded (deflate is self-delimiting). The central
//! directory ends the walk; a stream that ends before it is a truncated download.
//!
//! Not supported (an error, never a guess): encrypted entries, and entries whose size is only
//! in a trailing data descriptor and whose method is not deflate.

use flate2::{Decompress, FlushDecompress, Status};
use vc_core::{Error, Result};

/// Receives the entries of a [`ZipStream`].
pub(crate) trait EntrySink {
    /// A new entry: return whether its content is wanted.
    fn begin(&mut self, name: &str) -> Result<bool>;
    /// The next bytes of the wanted entry's (uncompressed) content.
    fn data(&mut self, bytes: &[u8]) -> Result<()>;
    /// The wanted entry ended.
    fn end(&mut self) -> Result<()>;
}

const LOCAL: u32 = 0x0403_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;
/// Central directory, zip64 end records, end of central directory, digital signature.
const TRAILERS: [u32; 5] = [
    0x0201_4b50,
    0x0606_4b50,
    0x0706_4b50,
    0x0605_4b50,
    0x0505_4b50,
];
const MAX_HEADER: usize = 30 + 2 * 65_535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    Stored,
    Deflate,
    /// Any other method (skipped when the size is known).
    Other,
}

#[derive(Debug)]
enum State {
    Header,
    Data {
        method: Method,
        /// Compressed bytes left (`None`: only the deflate stream's end tells).
        remaining: Option<u64>,
        wanted: bool,
        descriptor: bool,
        zip64: bool,
    },
    Descriptor {
        zip64: bool,
    },
    Done,
}

/// The reader. See the module docs.
pub(crate) struct ZipStream {
    buf: Vec<u8>,
    state: State,
    inflater: Decompress,
    out: Vec<u8>,
    inflated: u64,
    max_inflated: u64,
    entries: u32,
}

fn u16_at(b: &[u8], i: usize) -> u64 {
    u64::from(u16::from_le_bytes([b[i], b[i + 1]]))
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

fn bad(msg: &str) -> Error {
    Error::Invalid(format!("zip: {msg}"))
}

impl ZipStream {
    /// A reader that refuses to inflate more than `max_inflated` bytes in total (zip bombs).
    pub(crate) fn new(max_inflated: u64) -> Self {
        ZipStream {
            buf: Vec::new(),
            state: State::Header,
            inflater: Decompress::new(false),
            out: vec![0; 64 * 1024],
            inflated: 0,
            max_inflated,
            entries: 0,
        }
    }

    /// Local entries seen so far.
    pub(crate) fn entries(&self) -> u32 {
        self.entries
    }

    /// Feeds the next bytes of the archive.
    pub(crate) fn push(&mut self, data: &[u8], sink: &mut dyn EntrySink) -> Result<()> {
        let mut buf = std::mem::take(&mut self.buf);
        buf.extend_from_slice(data);
        let mut pos = 0;
        let res = loop {
            match self.step(&buf, &mut pos, sink) {
                Ok(true) => {}
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
        };
        buf.drain(..pos.min(buf.len()));
        self.buf = buf;
        res
    }

    /// The archive ended: anything but a reached central directory is a truncated download.
    pub(crate) fn finish(&mut self) -> Result<()> {
        match self.state {
            State::Done => Ok(()),
            State::Header if self.entries == 0 && self.buf.is_empty() => {
                Err(bad("empty stream (not a zip archive)"))
            }
            _ => Err(bad("truncated archive (central directory not reached)")),
        }
    }

    /// One step from `pos`; `false` when more input is needed (or the walk is done).
    fn step(&mut self, buf: &[u8], pos: &mut usize, sink: &mut dyn EntrySink) -> Result<bool> {
        let avail = &buf[*pos..];
        match self.state {
            State::Done => {
                *pos = buf.len();
                Ok(false)
            }
            State::Header => {
                if avail.len() < 4 {
                    return Ok(false);
                }
                let sig = u32_at(avail, 0);
                if TRAILERS.contains(&sig) {
                    self.state = State::Done;
                    *pos = buf.len();
                    return Ok(false);
                }
                if sig != LOCAL {
                    return Err(if self.entries == 0 {
                        bad("not a zip archive")
                    } else {
                        bad("bad local header signature")
                    });
                }
                if avail.len() < 30 {
                    return Ok(false);
                }
                let (nlen, xlen) = (u16_at(avail, 26) as usize, u16_at(avail, 28) as usize);
                let len = 30 + nlen + xlen;
                if len > MAX_HEADER {
                    return Err(bad("header too long"));
                }
                if avail.len() < len {
                    return Ok(false);
                }
                let flags = u16_at(avail, 6);
                if flags & 1 != 0 {
                    return Err(bad("encrypted entry"));
                }
                let method = match u16_at(avail, 8) {
                    0 => Method::Stored,
                    8 => Method::Deflate,
                    _ => Method::Other,
                };
                let mut csize = u64::from(u32_at(avail, 18));
                let usize_ = u64::from(u32_at(avail, 22));
                let name = String::from_utf8_lossy(&avail[30..30 + nlen]).into_owned();
                // zip64 extended information (header id 0x0001): the 0xFFFFFFFF sizes, in order.
                let mut zip64 = false;
                let extra = &avail[30 + nlen..len];
                let mut i = 0;
                while i + 4 <= extra.len() {
                    let (id, sz) = (u16_at(extra, i), u16_at(extra, i + 2) as usize);
                    let body = &extra[(i + 4).min(extra.len())..(i + 4 + sz).min(extra.len())];
                    if id == 1 {
                        zip64 = true;
                        let mut j = 0;
                        if usize_ == 0xFFFF_FFFF && body.len() >= j + 8 {
                            j += 8;
                        }
                        if csize == 0xFFFF_FFFF && body.len() >= j + 8 {
                            csize = u64_at(body, j);
                        }
                    }
                    i += 4 + sz;
                }
                let descriptor = flags & 8 != 0;
                let remaining = if descriptor && csize == 0 {
                    None
                } else {
                    Some(csize)
                };
                if remaining.is_none() && method != Method::Deflate {
                    return Err(bad(
                        "entry size only in a data descriptor (unsupported method)",
                    ));
                }
                self.entries += 1;
                let wanted = method != Method::Other && sink.begin(&name)?;
                if method == Method::Deflate {
                    self.inflater.reset(false);
                }
                *pos += len;
                self.state = State::Data {
                    method,
                    remaining,
                    wanted,
                    descriptor,
                    zip64,
                };
                Ok(true)
            }
            State::Data {
                method,
                remaining,
                wanted,
                descriptor,
                zip64,
            } => {
                let after = |s: &mut Self, sink: &mut dyn EntrySink, w: bool| -> Result<()> {
                    if w {
                        sink.end()?;
                    }
                    s.state = if descriptor {
                        State::Descriptor { zip64 }
                    } else {
                        State::Header
                    };
                    Ok(())
                };
                if remaining == Some(0) {
                    after(self, sink, wanted)?;
                    return Ok(true);
                }
                if avail.is_empty() {
                    return Ok(false);
                }
                let inflate = method == Method::Deflate && (wanted || remaining.is_none());
                if !inflate {
                    // Stored, or an unwanted entry of known size: pass through or skip.
                    let r = remaining.unwrap_or(0);
                    let n = usize::try_from(r).unwrap_or(usize::MAX).min(avail.len());
                    if wanted {
                        self.count(n as u64)?;
                        sink.data(&avail[..n])?;
                    }
                    *pos += n;
                    let left = r - n as u64;
                    if left == 0 {
                        after(self, sink, wanted)?;
                    } else {
                        self.state = State::Data {
                            method,
                            remaining: Some(left),
                            wanted,
                            descriptor,
                            zip64,
                        };
                    }
                    return Ok(true);
                }
                let take = remaining
                    .map_or(avail.len(), |r| usize::try_from(r).unwrap_or(usize::MAX))
                    .min(avail.len());
                let input = &avail[..take];
                let (in0, out0) = (self.inflater.total_in(), self.inflater.total_out());
                let st = self
                    .inflater
                    .decompress(input, &mut self.out, FlushDecompress::None)
                    .map_err(|e| bad(&format!("deflate: {e}")))?;
                let used = (self.inflater.total_in() - in0) as usize;
                let made = (self.inflater.total_out() - out0) as usize;
                self.count(made as u64)?;
                if wanted && made > 0 {
                    sink.data(&self.out[..made])?;
                }
                *pos += used;
                let left = remaining.map(|r| r - used as u64);
                if st == Status::StreamEnd {
                    if wanted {
                        sink.end()?;
                    }
                    match left {
                        // Bytes after the deflate end that the header counted: skip them.
                        Some(l) if l > 0 => {
                            self.state = State::Data {
                                method: Method::Stored,
                                remaining: Some(l),
                                wanted: false,
                                descriptor,
                                zip64,
                            }
                        }
                        _ => after(self, sink, false)?,
                    }
                    return Ok(true);
                }
                if left == Some(0) {
                    return Err(bad("deflate stream longer than its entry"));
                }
                self.state = State::Data {
                    method,
                    remaining: left,
                    wanted,
                    descriptor,
                    zip64,
                };
                Ok(used > 0 || made > 0)
            }
            State::Descriptor { zip64 } => {
                let sizes = if zip64 { 16 } else { 8 };
                if avail.len() < 4 {
                    return Ok(false);
                }
                let len = if u32_at(avail, 0) == DESCRIPTOR {
                    8 + sizes
                } else {
                    4 + sizes
                };
                if avail.len() < len {
                    return Ok(false);
                }
                *pos += len;
                self.state = State::Header;
                Ok(true)
            }
        }
    }

    fn count(&mut self, n: u64) -> Result<()> {
        self.inflated += n;
        if self.inflated > self.max_inflated {
            return Err(bad(&format!(
                "more than {} bytes inflated (zip bomb guard)",
                self.max_inflated
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal zip writer for tests: `(name, content, deflate, data_descriptor)`.
    pub(crate) fn zip(entries: &[(&str, &[u8], bool, bool)]) -> Vec<u8> {
        use std::io::Write;
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, content, deflate, dd) in entries {
            let data = if *deflate {
                let mut e =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(content).unwrap();
                e.finish().unwrap()
            } else {
                content.to_vec()
            };
            let crc = crc32(content);
            let offset = out.len() as u32;
            let flags: u16 = if *dd { 8 } else { 0 };
            let method: u16 = if *deflate { 8 } else { 0 };
            let (c, u, h) = if *dd {
                (0, 0, 0)
            } else {
                (data.len() as u32, content.len() as u32, crc)
            };
            out.extend_from_slice(&LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&u.to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&data);
            if *dd {
                out.extend_from_slice(&DESCRIPTOR.to_le_bytes());
                out.extend_from_slice(&crc.to_le_bytes());
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            }
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[0; 24]);
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0; 18]);
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0xEDB8_8320
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    #[derive(Default)]
    struct Collect {
        want: Vec<&'static str>,
        cur: Option<String>,
        got: Vec<(String, Vec<u8>)>,
        seen: Vec<String>,
    }

    impl EntrySink for Collect {
        fn begin(&mut self, name: &str) -> Result<bool> {
            self.seen.push(name.to_string());
            let w = self.want.contains(&name);
            if w {
                self.cur = Some(name.to_string());
                self.got.push((name.to_string(), Vec::new()));
            }
            Ok(w)
        }
        fn data(&mut self, b: &[u8]) -> Result<()> {
            self.got.last_mut().unwrap().1.extend_from_slice(b);
            Ok(())
        }
        fn end(&mut self) -> Result<()> {
            self.cur = None;
            Ok(())
        }
    }

    fn walk(z: &[u8], chunk: usize, want: &[&'static str]) -> (Collect, Result<()>) {
        let mut s = ZipStream::new(1 << 30);
        let mut c = Collect {
            want: want.to_vec(),
            ..Collect::default()
        };
        for part in z.chunks(chunk) {
            if let Err(e) = s.push(part, &mut c) {
                return (c, Err(e));
            }
        }
        let r = s.finish();
        (c, r)
    }

    #[test]
    fn reads_stored_deflated_and_descriptor_entries_in_any_chunking() {
        let big: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let z = zip(&[
            ("agency.txt", b"agency_id,agency_name\n1,A\n", false, false),
            ("stop_times.txt", &big, true, true),
            ("routes.txt", b"route_id\nr1\n", true, false),
            ("stops.txt", b"stop_id\ns1\n", true, true),
        ]);
        for chunk in [1, 7, 4096, z.len()] {
            let (c, r) = walk(&z, chunk, &["agency.txt", "routes.txt", "stops.txt"]);
            r.unwrap();
            assert_eq!(
                c.seen,
                ["agency.txt", "stop_times.txt", "routes.txt", "stops.txt"]
            );
            assert_eq!(c.got[0].1, b"agency_id,agency_name\n1,A\n");
            assert_eq!(c.got[1].1, b"route_id\nr1\n");
            assert_eq!(c.got[2].1, b"stop_id\ns1\n");
        }
    }

    #[test]
    fn truncated_and_non_zip_streams_fail() {
        let z = zip(&[("agency.txt", &[b'x'; 5000], true, false)]);
        // Cut inside the entry's data, and right before the central directory.
        let (_, r) = walk(&z[..z.len() / 3], 64, &["agency.txt"]);
        assert!(r.is_err(), "cut inside an entry: truncated");
        let (_, r) = walk(&z[..z.len() - 22 - 56], 64, &["agency.txt"]);
        assert!(r.is_err(), "no central directory: truncated");
        let (_, r) = walk(b"<html>not a zip</html>", 64, &[]);
        assert!(r.is_err());
        let (_, r) = walk(b"", 64, &[]);
        assert!(r.is_err());
    }

    #[test]
    fn inflate_cap_stops_zip_bombs() {
        let z = zip(&[("stop_times.txt", &vec![b'0'; 1 << 20], true, true)]);
        let mut s = ZipStream::new(64 * 1024);
        let mut c = Collect::default();
        assert!(s.push(&z, &mut c).is_err());
    }
}
