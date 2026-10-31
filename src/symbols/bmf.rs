//! Bugsee Mapping File (BMF) — the managed (.NET / Mono) line-number map.
//!
//! A BMF maps a method SIGNATURE plus an IL offset to a source file and line.
//! It is keyed by the assembly's MVID and consumed by the background worker
//! (`symbolfiles/parsers/bmf`, `ReMap`). It is NOT the BSF format
//! ("Bugsee Symbol File"), which is token-keyed and has its own spec.
//!
//! There is no written spec; the producer is `bugsee-dotnet`
//! (`MappingWriter.WriteBinary`) and the consumer is the worker's
//! `mapping_reader.py`. This module is a port of both, pinned by a byte-for-byte
//! round trip of a file the original writer produced (see the tests).
//!
//! Layout (every `var_int` is a big-endian 7-bit VLQ with the high bit set on
//! all but the last byte, minimal length, no bias — NOT the little-endian LEB128
//! the `.msym` files use):
//!
//! ```text
//! "BMFH"                          4 bytes
//! version                         1 byte (1)
//! arch                    str     ".net"
//! module id               str     the MVID, 32 lowercase hex digits, no dashes
//! module name             str     assembly name
//! module version          str     assembly version, e.g. "1.0.0.0"
//! signatures              n, n*str  extra image uuids (MVID, MVID+AotId, ...)
//! files                   n, n*str
//! methods                 n, each:
//!     signatures          n, n*str  full method signatures that all resolve here
//!     managed points      n, n*(il_offset, line, file_index)
//!     native points       n, n*(native_offset, il_offset)   (AOT only)
//! str = var_int byte length + UTF-8
//! ```
//!
//! The lookup the worker runs picks the LAST point whose offset is `<=` the
//! frame's, so points must be sorted ascending; the writer refuses anything
//! else rather than emit a file that silently resolves the wrong line.

#[cfg(test)]
use std::io::Read;
use std::io::Write;

use crate::error::{Error, Result};

pub const MAGIC: &[u8; 4] = b"BMFH";
pub const VERSION: u8 = 1;
/// The `arch` every managed BMF carries.
pub const ARCH_DOTNET: &str = ".net";

/// Longest string either side will accept. Real signatures and paths are a few
/// hundred bytes; the cap keeps a damaged length from allocating gigabytes.
const MAX_STRING_LEN: u32 = 1 << 20;
#[cfg(test)]
/// Most capacity pre-reserved from a count read off disk — the count is
/// untrusted, so growth past this is paid for by bytes actually present.
const PREALLOC_CAP: usize = 4096;

/// Identity block at the head of the file. Enough to key an upload without
/// reading the (large) method table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub arch: String,
    /// The assembly MVID, 32 lowercase hex digits.
    pub module_id: String,
    pub module_name: String,
    pub module_version: String,
    /// Every uuid this file answers to: the MVID, plus `MVID+AotId` (and
    /// `MVID+EntryAotId`) for AOT builds.
    pub signatures: Vec<String>,
}

/// One IL-offset -> line entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManagedPoint {
    pub il_offset: u32,
    pub line: u32,
    /// Index into [`Bmf::files`].
    pub file_index: u32,
}

/// One native-offset -> IL-offset entry. Only AOT builds have these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativePoint {
    pub native_offset: u32,
    pub il_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Method {
    /// Every signature string that resolves to this method (the runtime prints
    /// several shapes for the same method, and the worker tries them in turn).
    pub signatures: Vec<String>,
    pub managed: Vec<ManagedPoint>,
    pub native: Vec<NativePoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bmf {
    pub header: Header,
    pub files: Vec<String>,
    pub methods: Vec<Method>,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InputInvalid(format!("BMF: {}", msg.into()))
}

// ---------------------------------------------------------------- var_int

fn write_varint<W: Write>(w: &mut W, value: u32) -> Result<()> {
    let mut buf = [0u8; 5];
    let mut at = buf.len();
    let mut rest = value;
    loop {
        at -= 1;
        // The last byte (lowest group) carries no continuation bit.
        let cont = if at == buf.len() - 1 { 0 } else { 0x80 };
        buf[at] = (rest & 0x7f) as u8 | cont;
        rest >>= 7;
        if rest == 0 {
            break;
        }
    }
    w.write_all(&buf[at..])?;
    Ok(())
}

#[cfg(test)]
fn read_byte<R: Read>(r: &mut R) -> Result<u8> {
    let mut b = [0u8; 1];
    match r.read_exact(&mut b) {
        Ok(()) => Ok(b[0]),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(invalid("truncated file")),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
fn read_varint<R: Read>(r: &mut R) -> Result<u32> {
    let mut value: u64 = 0;
    for _ in 0..5 {
        let b = read_byte(r)?;
        value = (value << 7) | u64::from(b & 0x7f);
        if b & 0x80 == 0 {
            return u32::try_from(value).map_err(|_| invalid("var_int exceeds 32 bits"));
        }
    }
    Err(invalid("var_int longer than 5 bytes"))
}

fn write_str<W: Write>(w: &mut W, s: &str) -> Result<()> {
    let len = u32::try_from(s.len())
        .ok()
        .filter(|n| *n <= MAX_STRING_LEN)
        .ok_or_else(|| invalid(format!("string of {} bytes exceeds the limit", s.len())))?;
    write_varint(w, len)?;
    w.write_all(s.as_bytes())?;
    Ok(())
}

#[cfg(test)]
fn read_str<R: Read>(r: &mut R) -> Result<String> {
    let len = read_varint(r)?;
    if len > MAX_STRING_LEN {
        return Err(invalid(format!("string length {len} exceeds the limit")));
    }
    let mut buf = Vec::new();
    r.take(u64::from(len)).read_to_end(&mut buf)?;
    if buf.len() != len as usize {
        return Err(invalid("truncated file"));
    }
    String::from_utf8(buf).map_err(|_| invalid("string is not valid UTF-8"))
}

fn write_strs<W: Write>(w: &mut W, items: &[String]) -> Result<()> {
    write_varint(w, count(items.len())?)?;
    items.iter().try_for_each(|s| write_str(w, s))
}

#[cfg(test)]
fn read_strs<R: Read>(r: &mut R) -> Result<Vec<String>> {
    let n = read_varint(r)? as usize;
    let mut out = Vec::with_capacity(n.min(PREALLOC_CAP));
    for _ in 0..n {
        out.push(read_str(r)?);
    }
    Ok(out)
}

fn count(n: usize) -> Result<u32> {
    u32::try_from(n).map_err(|_| invalid("too many entries"))
}

// ----------------------------------------------------------------- reading
//
// Nothing in the CLI reads a BMF back (it only writes them); the reader exists to prove the
// writer's bytes against the worker's reference file and to check round trips in tests.

#[cfg(test)]
impl Header {
    /// Read only the identity block. Stops before the files and methods, so it
    /// is cheap on a large file.
    pub fn read<R: Read>(r: &mut R) -> Result<Header> {
        let mut magic = [0u8; 4];
        match r.read_exact(&mut magic) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(invalid("truncated file"))
            }
            Err(e) => return Err(e.into()),
        }
        if &magic != MAGIC {
            return Err(invalid("missing BMFH signature"));
        }
        // The worker ignores the version byte; we reject an unknown one because
        // a future layout would otherwise be mis-parsed instead of refused.
        let version = read_byte(r)?;
        if version != VERSION {
            return Err(invalid(format!("unsupported version {version}")));
        }
        Ok(Header {
            arch: read_str(r)?,
            module_id: read_str(r)?,
            module_name: read_str(r)?,
            module_version: read_str(r)?,
            signatures: read_strs(r)?,
        })
    }
}

#[cfg(test)]
impl Bmf {
    /// Parse a whole BMF. Counts and lengths come off disk and are never
    /// trusted for allocation, so damaged input fails with `InputInvalid`
    /// instead of aborting.
    pub fn read<R: Read>(r: &mut R) -> Result<Bmf> {
        let header = Header::read(r)?;
        let files = read_strs(r)?;

        let n_methods = read_varint(r)? as usize;
        let mut methods = Vec::with_capacity(n_methods.min(PREALLOC_CAP));
        for _ in 0..n_methods {
            let signatures = read_strs(r)?;

            let n = read_varint(r)? as usize;
            let mut managed = Vec::with_capacity(n.min(PREALLOC_CAP));
            for _ in 0..n {
                managed.push(ManagedPoint {
                    il_offset: read_varint(r)?,
                    line: read_varint(r)?,
                    file_index: read_varint(r)?,
                });
            }

            let n = read_varint(r)? as usize;
            let mut native = Vec::with_capacity(n.min(PREALLOC_CAP));
            for _ in 0..n {
                native.push(NativePoint {
                    native_offset: read_varint(r)?,
                    il_offset: read_varint(r)?,
                });
            }

            methods.push(Method {
                signatures,
                managed,
                native,
            });
        }
        Ok(Bmf {
            header,
            files,
            methods,
        })
    }
}

// ----------------------------------------------------------------- writing

impl Bmf {
    /// Refuse content the worker would silently mis-resolve.
    fn validate(&self) -> Result<()> {
        for (i, m) in self.methods.iter().enumerate() {
            if m.signatures.is_empty() {
                return Err(invalid(format!("method #{i} has no signature")));
            }
            let name = &m.signatures[0];
            if let Some(p) = m
                .managed
                .iter()
                .find(|p| p.file_index as usize >= self.files.len())
            {
                return Err(invalid(format!(
                    "{name}: file index {} is outside the {}-entry file table",
                    p.file_index,
                    self.files.len()
                )));
            }
            if m.managed
                .windows(2)
                .any(|w| w[0].il_offset > w[1].il_offset)
            {
                return Err(invalid(format!(
                    "{name}: managed points are not sorted by IL offset"
                )));
            }
            if m.native
                .windows(2)
                .any(|w| w[0].native_offset > w[1].native_offset)
            {
                return Err(invalid(format!(
                    "{name}: native points are not sorted by native offset"
                )));
            }
        }
        Ok(())
    }

    /// Serialize. Streams into `w`, so a caller can hand it a `BufWriter` over
    /// the output file.
    pub fn write<W: Write>(&self, w: &mut W) -> Result<()> {
        self.validate()?;

        w.write_all(MAGIC)?;
        w.write_all(&[VERSION])?;
        write_str(w, &self.header.arch)?;
        write_str(w, &self.header.module_id)?;
        write_str(w, &self.header.module_name)?;
        write_str(w, &self.header.module_version)?;
        write_strs(w, &self.header.signatures)?;
        write_strs(w, &self.files)?;

        write_varint(w, count(self.methods.len())?)?;
        for m in &self.methods {
            write_strs(w, &m.signatures)?;

            write_varint(w, count(m.managed.len())?)?;
            for p in &m.managed {
                write_varint(w, p.il_offset)?;
                write_varint(w, p.line)?;
                write_varint(w, p.file_index)?;
            }

            write_varint(w, count(m.native.len())?)?;
            for p in &m.native {
                write_varint(w, p.native_offset)?;
                write_varint(w, p.il_offset)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Bmf {
        Bmf {
            header: Header {
                arch: ARCH_DOTNET.into(),
                module_id: "7ce4a11d7cd346d48a25f89dd8880f08".into(),
                module_name: "App".into(),
                module_version: "1.0.0.0".into(),
                signatures: vec![
                    "7ce4a11d7cd346d48a25f89dd8880f08".into(),
                    "7ce4a11d7cd346d48a25f89dd8880f08aaaa".into(),
                ],
            },
            files: vec!["/src/A.cs".into(), "/src/B.cs".into()],
            methods: vec![
                Method {
                    signatures: vec!["N.A.Run ()".into(), "N.A.Run (System.Int32 n)".into()],
                    managed: vec![
                        ManagedPoint {
                            il_offset: 0,
                            line: 10,
                            file_index: 0,
                        },
                        ManagedPoint {
                            il_offset: 300,
                            line: 11,
                            file_index: 1,
                        },
                    ],
                    native: vec![
                        NativePoint {
                            native_offset: 0,
                            il_offset: 0,
                        },
                        NativePoint {
                            native_offset: 16384,
                            il_offset: 300,
                        },
                    ],
                },
                Method {
                    signatures: vec!["N.A..ctor ()".into()],
                    managed: vec![],
                    native: vec![],
                },
            ],
        }
    }

    fn encode(b: &Bmf) -> Vec<u8> {
        let mut out = Vec::new();
        b.write(&mut out).unwrap();
        out
    }

    fn varint_bytes(v: u32) -> Vec<u8> {
        let mut out = Vec::new();
        write_varint(&mut out, v).unwrap();
        out
    }

    #[test]
    fn varint_is_big_endian_vlq_with_pinned_bytes() {
        assert_eq!(varint_bytes(0), [0x00]);
        assert_eq!(varint_bytes(127), [0x7f]);
        assert_eq!(varint_bytes(128), [0x81, 0x00]);
        // 222 files in the reference fixture: `81 5e`.
        assert_eq!(varint_bytes(222), [0x81, 0x5e]);
        assert_eq!(varint_bytes(16383), [0xff, 0x7f]);
        assert_eq!(varint_bytes(16384), [0x81, 0x80, 0x00]);
        assert_eq!(varint_bytes(u32::MAX), [0x8f, 0xff, 0xff, 0xff, 0x7f]);
    }

    #[test]
    fn varint_round_trips_at_every_width_boundary() {
        for v in [
            0,
            1,
            127,
            128,
            255,
            16383,
            16384,
            2_097_151,
            2_097_152,
            268_435_455,
            268_435_456,
            u32::MAX,
        ] {
            let bytes = varint_bytes(v);
            assert_eq!(read_varint(&mut bytes.as_slice()).unwrap(), v, "{v}");
        }
    }

    #[test]
    fn varint_rejects_overlong_and_oversize_values() {
        // Six continuation bytes never terminates within the 5-byte limit.
        assert!(read_varint(&mut [0x81u8; 6].as_slice()).is_err());
        // Five bytes that decode past 32 bits.
        assert!(read_varint(&mut [0xff, 0xff, 0xff, 0xff, 0x7f].as_slice()).is_err());
    }

    #[test]
    fn writes_the_exact_bytes_of_the_original_writer() {
        let b = Bmf {
            header: Header {
                arch: ".net".into(),
                module_id: "ab".into(),
                module_name: "M".into(),
                module_version: "1".into(),
                signatures: vec!["ab".into()],
            },
            files: vec!["f.cs".into()],
            methods: vec![Method {
                signatures: vec!["T.M ()".into()],
                managed: vec![ManagedPoint {
                    il_offset: 0,
                    line: 130,
                    file_index: 0,
                }],
                native: vec![],
            }],
        };
        let expected: Vec<u8> = [
            &b"BMFH"[..],
            &[1],
            &[4],
            b".net",
            &[2],
            b"ab",
            &[1],
            b"M",
            &[1],
            b"1",
            &[1],
            &[2],
            b"ab", // signatures: 1 entry
            &[1],
            &[4],
            b"f.cs", // files: 1 entry
            &[1],    // methods: 1
            &[1],
            &[6],
            b"T.M ()", // method signatures
            &[1],
            &[0],
            &[0x81, 0x02],
            &[0], // one managed point: il 0, line 130, file 0
            &[0], // no native points
        ]
        .concat();
        assert_eq!(encode(&b), expected);
    }

    #[test]
    fn round_trips_with_native_points_and_several_uuids() {
        let b = sample();
        let back = Bmf::read(&mut encode(&b).as_slice()).unwrap();
        assert_eq!(back, b);
    }

    #[test]
    fn header_read_stops_before_the_method_table() {
        let b = sample();
        let mut bytes = encode(&b);
        // Garbage after the header must not matter to a header-only read.
        let header_len = {
            let mut probe = Vec::new();
            let mut only_header = b.clone();
            only_header.files.clear();
            only_header.methods.clear();
            only_header.write(&mut probe).unwrap();
            probe.len() - 2 // minus the two empty-table counts
        };
        bytes.truncate(header_len);
        assert_eq!(Header::read(&mut bytes.as_slice()).unwrap(), b.header);
    }

    #[test]
    fn writer_refuses_content_the_worker_would_mis_resolve() {
        let mut b = sample();
        b.methods[0].managed[1].file_index = 9;
        assert!(encode_err(&b).contains("file index 9"));

        let mut b = sample();
        b.methods[0].managed.swap(0, 1);
        assert!(encode_err(&b).contains("not sorted by IL offset"));

        let mut b = sample();
        b.methods[0].native.swap(0, 1);
        assert!(encode_err(&b).contains("not sorted by native offset"));

        let mut b = sample();
        b.methods[1].signatures.clear();
        assert!(encode_err(&b).contains("no signature"));
    }

    fn encode_err(b: &Bmf) -> String {
        b.write(&mut Vec::new()).unwrap_err().to_string()
    }

    #[test]
    fn every_truncation_fails_cleanly() {
        let bytes = encode(&sample());
        for cut in 0..bytes.len() {
            let err = Bmf::read(&mut &bytes[..cut])
                .expect_err(&format!("prefix of {cut} bytes must not parse"));
            assert!(matches!(err, Error::InputInvalid(_)), "cut {cut}: {err}");
        }
    }

    #[test]
    fn garbage_fails_cleanly_without_allocating_what_it_claims() {
        let good = encode(&sample());

        // Wrong signature / unknown version.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(Bmf::read(&mut bad.as_slice()).is_err());
        let mut bad = good.clone();
        bad[4] = 2;
        assert!(Bmf::read(&mut bad.as_slice()).is_err());

        // A string that claims 4 GiB, and a method count that claims 4 billion
        // with nothing behind it.
        let mut huge_string = b"BMFH\x01".to_vec();
        huge_string.extend([0x8f, 0xff, 0xff, 0xff, 0x7f]);
        assert!(Bmf::read(&mut huge_string.as_slice()).is_err());

        let mut huge_count = encode(&Bmf {
            methods: vec![],
            ..sample()
        });
        huge_count.pop(); // the 0 method count
        huge_count.extend([0x8f, 0xff, 0xff, 0xff, 0x7f]);
        assert!(Bmf::read(&mut huge_count.as_slice()).is_err());

        // Invalid UTF-8 inside a string.
        let mut bad_utf8 = b"BMFH\x01".to_vec();
        bad_utf8.extend([2, 0xff, 0xfe]);
        assert!(Bmf::read(&mut bad_utf8.as_slice()).is_err());

        // No byte pattern may panic.
        for seed in 0u32..2000 {
            let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
            let mut buf = good.clone();
            for _ in 0..4 {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let at = (x as usize) % buf.len();
                buf[at] = (x >> 24) as u8;
            }
            let _ = Bmf::read(&mut buf.as_slice());
        }
    }

    /// The strongest pin available: a file the original C# writer produced must
    /// re-encode to the identical bytes. Not committed (it is ~900 KB); point
    /// `BUGSEE_BMF_REFERENCE` at the worker's `symbol1.bmf` to run it.
    #[test]
    fn reference_file_from_the_original_writer_round_trips_byte_for_byte() {
        let Ok(path) = std::env::var("BUGSEE_BMF_REFERENCE") else {
            eprintln!("BUGSEE_BMF_REFERENCE not set; skipping");
            return;
        };
        let original = std::fs::read(path).unwrap();
        let bmf = Bmf::read(&mut original.as_slice()).unwrap();

        assert_eq!(bmf.header.arch, ".net");
        assert_eq!(bmf.header.module_id, "7ce4a11d7cd346d48a25f89dd8880f08");
        assert_eq!(bmf.header.module_name, "SiteOwlDesignApp");
        assert_eq!(bmf.header.signatures, ["7ce4a11d7cd346d48a25f89dd8880f08"]);
        assert_eq!(bmf.files.len(), 222);
        assert_eq!(bmf.methods.len(), 6111);

        assert!(
            encode(&bmf) == original,
            "re-encoded bytes differ from the original"
        );
    }
}
