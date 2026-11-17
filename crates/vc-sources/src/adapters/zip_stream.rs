//! A streaming ZIP reader for dumps: local file headers are read in order and **one** member
//! (the first whose file name a [`MemberPick`] accepts) is decoded; every other member is
//! skipped, and the rest of the archive after the chosen member is ignored. Only `stored` (0)
//! and `deflate` (8) members are read. A member written with a data descriptor (general-purpose
//! flag bit 3, sizes unknown up front) is read to the end of its deflate stream. The central
//! directory is never needed, so memory stays bounded by one input chunk plus one header.

use vc_core::{Error, Result};

/// Which archive member a dump reads: base file names (no directory), compared ASCII
/// case-insensitively. An empty list takes the first member.
pub(crate) type MemberPick = &'static [&'static str];

const LOCAL: u32 = 0x0403_4b50;
const CENTRAL: u32 = 0x0201_4b50;
const END: u32 = 0x0605_4b50;
const DESCRIPTOR: u32 = 0x0807_4b50;

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

enum State {
    /// Waiting for a local header (or the central directory).
    Header,
    /// Inside a member's data.
    Data {
        wanted: bool,
        descriptor: bool,
        /// Compressed bytes left, when the header states them.
        remaining: Option<u64>,
        /// Raw-deflate decoder (`None` for a stored member).
        inflate: Option<Box<flate2::Decompress>>,
    },
    /// After a member written with a data descriptor.
    Descriptor { wanted: bool },
    /// The chosen member is done, or the central directory began: ignore the rest.
    Done,
}

/// The decoder state.
pub(crate) struct ZipStream {
    pick: MemberPick,
    buf: Vec<u8>,
    state: State,
    found: bool,
}

fn picks(pick: MemberPick, name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    pick.is_empty() || pick.iter().any(|p| p.eq_ignore_ascii_case(base))
}

impl ZipStream {
    pub(crate) fn new(pick: MemberPick) -> Self {
        ZipStream {
            pick,
            buf: Vec::new(),
            state: State::Header,
            found: false,
        }
    }

    /// Appends archive bytes; decoded bytes of the chosen member go to `out`.
    pub(crate) fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<()> {
        if matches!(self.state, State::Done) {
            return Ok(());
        }
        self.buf.extend_from_slice(input);
        let mut pos = 0;
        loop {
            let progressed = self.step(&mut pos, out)?;
            if !progressed || matches!(self.state, State::Done) {
                break;
            }
        }
        self.buf.drain(..pos.min(self.buf.len()));
        if matches!(self.state, State::Done) {
            self.buf = Vec::new();
        }
        Ok(())
    }

    /// One state step over `self.buf[*pos..]`; `false` when more input is needed.
    fn step(&mut self, pos: &mut usize, out: &mut Vec<u8>) -> Result<bool> {
        let avail = &self.buf[*pos..];
        match &mut self.state {
            State::Done => Ok(false),
            State::Header => {
                if avail.len() < 4 {
                    return Ok(false);
                }
                match u32_at(avail, 0) {
                    LOCAL => {}
                    CENTRAL | END => {
                        self.state = State::Done;
                        return Ok(true);
                    }
                    _ => return Err(Error::Invalid("zip: bad local file header".into())),
                }
                if avail.len() < 30 {
                    return Ok(false);
                }
                let flags = u16_at(avail, 6);
                let method = u16_at(avail, 8);
                let csize = u32_at(avail, 18);
                let (n, e) = (
                    usize::from(u16_at(avail, 26)),
                    usize::from(u16_at(avail, 28)),
                );
                if avail.len() < 30 + n + e {
                    return Ok(false);
                }
                if flags & 1 != 0 {
                    return Err(Error::Invalid("zip: encrypted member".into()));
                }
                let name = String::from_utf8_lossy(&avail[30..30 + n]).into_owned();
                let wanted = !self.found && picks(self.pick, &name);
                let descriptor = flags & 0x08 != 0;
                let remaining = (!descriptor && csize != u32::MAX).then_some(u64::from(csize));
                let inflate = match method {
                    0 if remaining.is_none() => {
                        return Err(Error::Invalid("zip: stored member without its size".into()));
                    }
                    0 => None,
                    8 => Some(Box::new(flate2::Decompress::new(false))),
                    m if wanted => {
                        return Err(Error::Invalid(format!(
                            "zip: unsupported compression method {m}"
                        )));
                    }
                    // Another member we skip: its size must be known to skip it.
                    _ if remaining.is_some() => None,
                    m => {
                        return Err(Error::Invalid(format!(
                            "zip: cannot skip a method-{m} member of unknown size"
                        )));
                    }
                };
                if wanted {
                    self.found = true;
                }
                *pos += 30 + n + e;
                self.state = State::Data {
                    wanted,
                    descriptor,
                    remaining,
                    inflate,
                };
                Ok(true)
            }
            State::Data {
                wanted,
                descriptor,
                remaining,
                inflate,
            } => {
                let (wanted, descriptor) = (*wanted, *descriptor);
                // A member of known size that we skip, or a stored member: count bytes.
                if let Some(rem) = remaining.as_mut()
                    && (inflate.is_none() || !wanted)
                {
                    let take = usize::try_from(*rem).unwrap_or(usize::MAX).min(avail.len());
                    if wanted {
                        out.extend_from_slice(&avail[..take]);
                    }
                    *pos += take;
                    *rem -= take as u64;
                    if *rem > 0 {
                        return Ok(false);
                    }
                    self.member_end(wanted, descriptor);
                    return Ok(true);
                }
                let d = inflate
                    .as_mut()
                    .ok_or_else(|| Error::Invalid("zip: no decoder".into()))?;
                let limit = remaining
                    .map(|r| usize::try_from(r).unwrap_or(usize::MAX))
                    .unwrap_or(usize::MAX)
                    .min(avail.len());
                let input = &avail[..limit];
                let mut scratch = Vec::new();
                let sink: &mut Vec<u8> = if wanted { out } else { &mut scratch };
                let mut used_total = 0usize;
                loop {
                    if sink.capacity() - sink.len() < 64 * 1024 {
                        sink.reserve(256 * 1024);
                    }
                    let (in0, out0) = (d.total_in(), d.total_out());
                    let st = d
                        .decompress_vec(&input[used_total..], sink, flate2::FlushDecompress::None)
                        .map_err(|e| Error::Invalid(format!("zip: deflate: {e}")))?;
                    let used = usize::try_from(d.total_in() - in0).unwrap_or(usize::MAX);
                    used_total += used;
                    if !wanted {
                        sink.clear();
                    }
                    if st == flate2::Status::StreamEnd {
                        *pos += used_total;
                        if let Some(r) = remaining.as_mut() {
                            *r = r.saturating_sub(used_total as u64);
                            // Bytes the header counted past the deflate stream's end.
                            let extra = usize::try_from(*r).unwrap_or(usize::MAX);
                            let left = avail.len() - used_total;
                            if extra > left {
                                return Err(Error::Invalid("zip: member size mismatch".into()));
                            }
                            *pos += extra;
                        }
                        self.member_end(wanted, descriptor);
                        return Ok(true);
                    }
                    if used == 0 && d.total_out() == out0 {
                        break; // needs more input
                    }
                }
                *pos += used_total;
                if let Some(r) = remaining.as_mut() {
                    *r -= used_total as u64;
                }
                Ok(false)
            }
            State::Descriptor { wanted } => {
                let wanted = *wanted;
                let need = if avail.len() >= 4 && u32_at(avail, 0) == DESCRIPTOR {
                    16
                } else {
                    12
                };
                if avail.len() < need {
                    return Ok(false);
                }
                *pos += need;
                self.state = if wanted { State::Done } else { State::Header };
                Ok(true)
            }
        }
    }

    fn member_end(&mut self, wanted: bool, descriptor: bool) {
        self.state = if descriptor {
            State::Descriptor { wanted }
        } else if wanted {
            State::Done
        } else {
            State::Header
        };
    }

    /// End of the archive stream: fails when the chosen member was not found or was cut off.
    pub(crate) fn finish(&mut self) -> Result<()> {
        match self.state {
            State::Done if self.found => Ok(()),
            State::Done | State::Header if !self.found => {
                Err(Error::Invalid("zip: member not found".into()))
            }
            _ => Err(Error::Invalid("zip: truncated archive".into())),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A minimal archive writer (test only): deflate or stored members, optionally with data
    /// descriptors, plus a central directory stub (never read by the stream reader).
    pub(crate) fn zip(members: &[(&str, &[u8], u16, bool)]) -> Vec<u8> {
        use std::io::Write;
        let mut z = Vec::new();
        for (name, data, method, desc) in members {
            let body = if *method == 8 {
                let mut e =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(data).unwrap();
                e.finish().unwrap()
            } else {
                data.to_vec()
            };
            let flags: u16 = if *desc { 0x08 } else { 0 };
            let (cs, us) = if *desc {
                (0, 0)
            } else {
                (body.len() as u32, data.len() as u32)
            };
            z.extend(LOCAL.to_le_bytes());
            z.extend(20u16.to_le_bytes());
            z.extend(flags.to_le_bytes());
            z.extend(method.to_le_bytes());
            z.extend([0u8; 8]); // time, date, crc
            z.extend(cs.to_le_bytes());
            z.extend(us.to_le_bytes());
            z.extend((name.len() as u16).to_le_bytes());
            z.extend(0u16.to_le_bytes());
            z.extend(name.as_bytes());
            z.extend(&body);
            if *desc {
                z.extend(DESCRIPTOR.to_le_bytes());
                z.extend([0u8; 4]);
                z.extend((body.len() as u32).to_le_bytes());
                z.extend((data.len() as u32).to_le_bytes());
            }
        }
        z.extend(CENTRAL.to_le_bytes());
        z.extend([0u8; 42]);
        z.extend(END.to_le_bytes());
        z.extend([0u8; 18]);
        z
    }

    fn read(z: &[u8], pick: MemberPick, chunk: usize) -> Result<Vec<u8>> {
        let mut s = ZipStream::new(pick);
        let mut out = Vec::new();
        for c in z.chunks(chunk) {
            s.push(c, &mut out)?;
        }
        s.finish()?;
        Ok(out)
    }

    #[test]
    fn picks_one_member_across_chunk_boundaries() {
        let big: Vec<u8> = (0..200_000u32)
            .flat_map(|i| format!("row{i}\n").into_bytes())
            .collect();
        for desc in [false, true] {
            let z = zip(&[
                ("WDICountry.csv", b"a,b\n1,2\n", 8, desc),
                ("data/WDICSV.csv", &big, 8, desc),
                ("WDISeries.csv", b"x\n", 0, false),
            ]);
            for chunk in [1, 7, 4096, 1 << 20] {
                let out = read(&z, &["wdicsv.csv"], chunk).unwrap();
                assert!(out == big, "desc={desc} chunk={chunk}");
            }
            // The first member when nothing is named.
            assert_eq!(read(&z, &[], 3).unwrap(), b"a,b\n1,2\n");
        }
        let stored = zip(&[("x.csv", b"stored body", 0, false)]);
        assert_eq!(read(&stored, &["X.CSV"], 2).unwrap(), b"stored body");
    }

    #[test]
    fn missing_or_truncated_member_fails() {
        let z = zip(&[("a.csv", b"1\n2\n", 8, false)]);
        assert!(read(&z, &["b.csv"], 5).is_err(), "member not found");
        let mut s = ZipStream::new(&["a.csv"]);
        let mut out = Vec::new();
        s.push(&z[..40], &mut out).unwrap();
        assert!(s.finish().is_err(), "truncated");
        assert!(read(b"not a zip at all", &[], 4).is_err(), "bad header");
    }
}
