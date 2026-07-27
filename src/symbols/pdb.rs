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
const MSF7_MAGIC: &[u8] = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS";
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
    let mut header = [0u8; MSF2_MAGIC.len()];
    let Ok(read) = file.read(&mut header) else {
        return false;
    };
    let header = &header[..read];
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
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.pdb");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        (dir, path)
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
        // check would misclassify this.
        let (_d, p) = write_temp(b"Microsoft Corporation readme, not debug info\x00\x00");
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
}
