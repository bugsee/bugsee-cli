//! Windows PDB debug-info files.
//!
//! A `.pdb` is the Windows equivalent of DWARF-in-dSYM: the debug info lives in
//! a standalone MSF container next to the shipped `.exe`/`.dll`. It is what a
//! Rust `*-pc-windows-msvc` build emits alongside the binary.
//!
//! **Identity is the PDB debug id (GUID + age)** — `obj.debug_id()`. This must
//! match what the worker stores (`symbolfiles/pdb.py`, same `symbolic` major)
//! and what a Windows module reports at crash time; a mismatch means uploaded
//! symbols silently resolve nothing. In particular it is NOT the PE `code_id`,
//! which is a timestamp+size describing the *binary* rather than the PDB.

use std::path::Path;

use symbolic_debuginfo::Archive;

use crate::error::{Error, Result};

/// MSF container magics that begin a `.pdb`. "DS" is MSF 7.0 (every modern
/// toolchain, including the Rust MSVC targets); "JG" is the legacy MSF 2.0.
///
/// Byte-for-byte the constants the `pdb` crate matches on (MSF 7.0 includes the
/// three trailing NULs that pad the magic to 32 bytes; MSF 2.0 is matched on its
/// 42-byte prefix), so the sniff classifies exactly what the parser will accept
/// — a shorter signature would let `identify` be handed files it must then
/// reject.
const MSF7_MAGIC: &[u8] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\x00\x00\x00";
const MSF2_MAGIC: &[u8] = b"Microsoft C/C++ program database 2.00\r\n\x1aJG";

/// One object inside a PDB (in practice exactly one).
#[derive(Debug, Clone)]
pub struct PdbSlice {
    /// Stringified debug id (GUID + age) — the symbol's identity.
    pub uuid: String,
    /// Architecture as `symbolic-debuginfo` reports it (`x86_64`, `arm64`, …).
    pub arch: String,
}

/// Result of parsing a single `.pdb`.
#[derive(Debug, Clone)]
pub struct PdbIdentity {
    pub slices: Vec<PdbSlice>,
}

/// Whether `path` starts with an MSF container magic.
///
/// Cheap header sniff — used by discovery so a directory walk does not attempt
/// a full parse of every file. Both magics share the `"Microsoft C/C++ "`
/// prefix, so the whole magic is compared rather than a short signature.
pub fn looks_like_pdb(path: &Path) -> bool {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    // Fill the buffer rather than trusting one `read`: a short read is legal
    // even mid-file, and returning early on one would skip a real PDB.
    let mut header = [0u8; MSF2_MAGIC.len()];
    let mut filled = 0;
    while filled < header.len() {
        match file.read(&mut header[filled..]) {
            Ok(0) => break, // EOF — file is shorter than the magic
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    let header = &header[..filled];
    header.starts_with(MSF7_MAGIC) || header.starts_with(MSF2_MAGIC)
}

/// Parse `pdb_path` and extract the debug id + arch of every object inside.
///
/// Reads the file fully into memory: PDB parsing needs random access, and a
/// Rust binary's PDB is typically tens of MB.
pub fn identify(pdb_path: &Path) -> Result<PdbIdentity> {
    if !pdb_path.is_file() {
        return Err(Error::InputInvalid(format!(
            "expected a .pdb file, got {}",
            pdb_path.display()
        )));
    }

    let data = std::fs::read(pdb_path)?;
    let archive = Archive::parse(&data).map_err(|e| {
        Error::InputInvalid(format!(
            "failed to parse PDB at {}: {}",
            pdb_path.display(),
            e,
        ))
    })?;

    let mut slices = Vec::new();
    for obj in archive.objects() {
        let obj = obj.map_err(|e| {
            Error::InputInvalid(format!(
                "failed to read object in {}: {}",
                pdb_path.display(),
                e,
            ))
        })?;
        let debug_id = obj.debug_id();
        if debug_id.is_nil() {
            // A record with no debug id could never be matched by a crashing
            // module — skip rather than upload something unusable.
            tracing::warn!(
                path = %pdb_path.display(),
                "PDB object has no debug id; skipping",
            );
            continue;
        }
        slices.push(PdbSlice {
            uuid: debug_id.to_string(),
            arch: obj.arch().name().to_string(),
        });
    }

    if slices.is_empty() {
        return Err(Error::InputInvalid(format!(
            "PDB contains no identifiable objects: {}",
            pdb_path.display()
        )));
    }
    Ok(PdbIdentity { slices })
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A hand-assembled MSF 7.0 container, so the identity contract can be
    //! pinned without checking a multi-megabyte MSVC artefact into the repo.

    use uuid::Uuid;

    /// `IMAGE_FILE_MACHINE_AMD64` — what a `x86_64-pc-windows-msvc` build writes.
    pub const MACHINE_AMD64: u16 = 0x8664;

    const PAGE: usize = 4096;
    /// Superblock, free-page map, stream-table page list, stream table, and one
    /// page each for streams 1 and 3.
    const PAGES: u32 = 6;

    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn put_u16(buf: &mut [u8], off: usize, v: u16) {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Build a minimal but genuinely parseable MSF 7.0 container carrying the
    /// two streams `symbolic` reads to derive an identity: the PDB info stream
    /// (stream 1 — GUID + age) and the DBI header (stream 3 — age + machine
    /// type). Everything else is zeroed.
    ///
    /// `pdbi_age` and `dbi_age` are separate on purpose: `symbolic` prefers the
    /// DBI age (the one the linker also stamped into the image) and falls back
    /// to the PDB info age only when the DBI has none, and that choice decides
    /// the key we upload.
    pub fn synth_pdb(guid: Uuid, pdbi_age: u32, dbi_age: u32, machine: u16) -> Vec<u8> {
        let mut file = vec![0u8; PAGE * PAGES as usize];

        // --- page 0: superblock -------------------------------------------
        // The page numbers holding the stream-table page list follow the
        // 52-byte header inline; one entry is enough for a table this small.
        let stream_table = build_stream_table();
        file[..super::MSF7_MAGIC.len()].copy_from_slice(super::MSF7_MAGIC);
        put_u32(&mut file, 32, PAGE as u32); // page_size
        put_u32(&mut file, 36, 1); // free_page_map
        put_u32(&mut file, 40, PAGES); // pages_used
        put_u32(&mut file, 44, stream_table.len() as u32); // directory_size
        put_u32(&mut file, 48, 0); // reserved
        put_u32(&mut file, 52, 2); // -> page 2

        // --- page 2: the stream-table page list ---------------------------
        put_u32(&mut file, 2 * PAGE, 3); // -> page 3

        // --- page 3: the stream table -------------------------------------
        file[3 * PAGE..3 * PAGE + stream_table.len()].copy_from_slice(&stream_table);

        // --- page 4: stream 1, the PDB info stream ------------------------
        let (d1, d2, d3, d4) = guid.as_fields();
        let pdbi = 4 * PAGE;
        put_u32(&mut file, pdbi, 20_000_404); // version (VC70)
        put_u32(&mut file, pdbi + 4, 0x1234_5678); // signature
        put_u32(&mut file, pdbi + 8, pdbi_age);
        put_u32(&mut file, pdbi + 12, d1);
        put_u16(&mut file, pdbi + 16, d2);
        put_u16(&mut file, pdbi + 18, d3);
        file[pdbi + 20..pdbi + 28].copy_from_slice(d4);
        put_u32(&mut file, pdbi + 28, 0); // names_size

        // --- page 5: stream 3, the DBI header -----------------------------
        let dbi = 5 * PAGE;
        put_u32(&mut file, dbi, u32::MAX); // signature
        put_u32(&mut file, dbi + 4, 19_990_903); // version (V70)
        put_u32(&mut file, dbi + 8, dbi_age);
        put_u16(&mut file, dbi + 12, 0xffff); // gs_symbols_stream (none)
        put_u16(&mut file, dbi + 16, 0xffff); // ps_symbols_stream (none)
                                              // The global symbol table is not optional to the reader, so point it at
                                              // the empty stream 4 rather than at "none".
        put_u16(&mut file, dbi + 20, 4); // symbol_records_stream
        put_u16(&mut file, dbi + 58, machine);

        file
    }

    /// `stream_count`, then each stream's byte size, then each stream's page
    /// numbers. Only streams 1 and 3 hold data and so occupy a page each; the
    /// rest are empty (stream 4 exists solely to be the global symbol table).
    fn build_stream_table() -> Vec<u8> {
        let mut t = vec![0u8; 32];
        put_u32(&mut t, 0, 5); // stream_count
        put_u32(&mut t, 4, 0); // stream 0: empty
        put_u32(&mut t, 8, 32); // stream 1: PDB info
        put_u32(&mut t, 12, 0); // stream 2: empty
        put_u32(&mut t, 16, 64); // stream 3: DBI header
        put_u32(&mut t, 20, 0); // stream 4: global symbols (empty)
        put_u32(&mut t, 24, 4); // stream 1 -> page 4
        put_u32(&mut t, 28, 5); // stream 3 -> page 5
        t
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{synth_pdb, MACHINE_AMD64};
    use super::*;
    use std::io::Write;
    use uuid::Uuid;

    fn guid() -> Uuid {
        "dfb8e43a-f242-3d73-a453-aeb6a777ef75".parse().unwrap()
    }

    fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pdb");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        (dir, path)
    }

    /// The upload key. This exact string is what the worker re-derives from the
    /// same bytes (`symbolfiles/pdb.py`, same `symbolic` major) and what a
    /// crashing Windows module resolves against — changing the formatting (to
    /// Breakpad's uppercase/dashless form, say, or to the PE code id) breaks
    /// symbolication silently, so it is pinned literally.
    #[test]
    fn identifies_a_pdb_by_its_debug_id() {
        // The DBI age (1) deliberately differs from the PDB info age (7): the
        // DBI one is what the linker also wrote into the image, so it wins.
        let (_d, p) = write_temp(&synth_pdb(guid(), 7, 1, MACHINE_AMD64));

        let id = identify(&p).unwrap();
        assert_eq!(id.slices.len(), 1);
        assert_eq!(
            id.slices[0].uuid, "dfb8e43a-f242-3d73-a453-aeb6a777ef75-1",
            "lowercase dashed GUID + '-' + age",
        );
        assert_eq!(id.slices[0].arch, "x86_64");
    }

    #[test]
    fn falls_back_to_the_pdb_info_age_when_the_dbi_has_none() {
        let (_d, p) = write_temp(&synth_pdb(guid(), 7, 0, MACHINE_AMD64));
        let id = identify(&p).unwrap();
        assert_eq!(id.slices[0].uuid, "dfb8e43a-f242-3d73-a453-aeb6a777ef75-7");
    }

    /// The age is lowercase hex and unpadded — a decimal or zero-padded age
    /// would not match what the worker stores.
    #[test]
    fn formats_the_age_as_unpadded_lowercase_hex() {
        let (_d, p) = write_temp(&synth_pdb(guid(), 0, 26, MACHINE_AMD64));
        let id = identify(&p).unwrap();
        assert_eq!(id.slices[0].uuid, "dfb8e43a-f242-3d73-a453-aeb6a777ef75-1a");
    }

    #[test]
    fn a_synthesized_pdb_is_recognized_by_the_header_sniff() {
        // Ties discovery to parsing: what `looks_like_pdb` admits is exactly
        // what `identify` can read.
        let (_d, p) = write_temp(&synth_pdb(guid(), 1, 1, MACHINE_AMD64));
        assert!(looks_like_pdb(&p));
    }

    #[test]
    fn recognizes_both_msf_generations() {
        let (_d, p) = write_temp(&[MSF7_MAGIC, &[0u8; 64][..]].concat());
        assert!(looks_like_pdb(&p), "MSF 7.0 (modern toolchains)");

        let (_d, p) = write_temp(&[MSF2_MAGIC, &[0u8; 64][..]].concat());
        assert!(looks_like_pdb(&p), "MSF 2.0 (legacy)");
    }

    #[test]
    fn rejects_other_microsoft_prefixed_files() {
        // Both magics share the "Microsoft C/C++ " prefix, so a short signature
        // check would misclassify a file that merely starts with it.
        let (_d, p) = write_temp(b"Microsoft C/C++ Optimizing Compiler Version 19.40\r\n");
        assert!(!looks_like_pdb(&p));
    }

    #[test]
    fn rejects_a_truncated_msf7_magic() {
        // The magic is 32 bytes including three trailing NULs; dropping them is
        // the short-signature bug the full comparison exists to prevent.
        let short = &MSF7_MAGIC[..MSF7_MAGIC.len() - 3];
        let (_d, p) = write_temp(&[short, b"garbage".as_slice()].concat());
        assert!(!looks_like_pdb(&p));
    }

    #[test]
    fn rejects_unrelated_and_short_files() {
        let (_d, p) = write_temp(b"\x7fELF not a pdb");
        assert!(!looks_like_pdb(&p));

        let (_d, p) = write_temp(b"hi");
        assert!(!looks_like_pdb(&p), "a file shorter than the magic");
    }

    #[test]
    fn identify_rejects_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(identify(dir.path()).is_err());
    }

    #[test]
    fn identify_rejects_a_non_pdb_file() {
        let (_d, p) = write_temp(b"definitely not a pdb container");
        assert!(identify(&p).is_err());
    }

    /// MSF 2.0 passes the sniff but no reader supports it — it must surface as a
    /// typed parse error (caller warns and skips), never a panic.
    #[test]
    fn identify_rejects_a_legacy_msf2_container() {
        let mut bytes = MSF2_MAGIC.to_vec();
        bytes.resize(8192, 0);
        let (_d, p) = write_temp(&bytes);
        assert!(looks_like_pdb(&p));
        assert!(matches!(identify(&p), Err(Error::InputInvalid(_))));
    }
}
