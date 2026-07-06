//! NDK / native debug-symbol identification — per-library, build-id keyed.
//!
//! The caller hands the CLI an already-packaged `native-debug-symbols.zip`
//! (the artifact AGP writes under `build/outputs/native-debug-symbols/
//! <variant>/`). We open it and, for EACH `.so`, read the GNU build-id
//! (`code_id`) via `symbolic-debuginfo` — the SAME crate family (major 13) the
//! worker uses, so the identifier is byte-identical producer↔consumer.
//!
//! Each `.so` is then uploaded as its OWN symbol document, keyed by its own
//! build-id (one file → one document → one S3 object). This is the invariant
//! the data model requires: a UUID uniquely identifies one symbol file, and
//! `images[]` within a document is reserved for the per-arch slices of ONE fat
//! binary — a `.so` is a single-arch file, hence one image. The build-level
//! BUILD_UUID (`--uuid`) is NOT reused as the native identity: it belongs to
//! the ProGuard mapping, and sharing it collapsed native + mapping into one
//! record. Keying by the real build-id also unlocks per-library dedup: an
//! unchanged `.so` (same build-id) is skipped before its bytes transfer.
//!
//! A `.so` built without `-Wl,--build-id` has no `code_id`; it can never be
//! matched at crash time, so it is warned about and skipped — never faked with
//! the BUILD_UUID.
//!
//! Out of scope: walking a loose directory of unstripped `.so`s (AGP's
//! intermediate-folder case) — the Gradle plugin pre-zips that before invoking
//! the CLI.

use sha1::{Digest as _, Sha1};
use std::io::Read;
use std::path::{Path, PathBuf};
use symbolic_debuginfo::Archive;

fn sha1_hex(bytes: &[u8]) -> String {
    let digest: [u8; 20] = Sha1::digest(bytes).into();
    hex::encode(digest)
}

/// SHA-1 hex of a file's bytes — the wire `hash` for a per-`.so` upload.
pub fn sha1_hex_of_file(path: &Path) -> std::io::Result<String> {
    Ok(sha1_hex(&std::fs::read(path)?))
}

/// One native library extracted from an AGP `native-debug-symbols.zip`.
///
/// Each `.so` is its OWN symbol: one file → one symbol document → one S3
/// object, keyed by its own GNU build-id. (`images[]` within a single document
/// is reserved for the per-arch slices of ONE fat binary; a `.so` is a
/// single-arch file, hence one image.)
pub struct ElfLib {
    /// Entry path inside the source archive (e.g. `arm64-v8a/libfoo.so`).
    pub name: String,
    /// GNU build-id (`code_id`), lowercase hex — the symbol's identity. `None`
    /// when the library was built without `-Wl,--build-id`; such a library
    /// cannot be matched at crash time and is skipped by the uploader (never
    /// keyed by the build-level UUID, which is the ProGuard mapping's identity).
    pub build_id: Option<String>,
    /// Object architecture (e.g. `arm64`); diagnostic only — the worker
    /// reconciles the canonical arch from the ELF during processing.
    pub arch: String,
    /// Temp file holding the extracted `.so` bytes, packed into its own upload.
    pub path: PathBuf,
}

/// Extract every ELF `.so` from `archive_path` into `out_dir`, reading each
/// library's GNU build-id. The wire `code_id` is produced by the same
/// `symbolic` crate family (major 13) the worker uses, so the identifier is
/// byte-identical producer↔consumer.
pub fn extract_libs(archive_path: &Path, out_dir: &Path) -> std::io::Result<Vec<ElfLib>> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    let mut libs = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        // AGP packs unstripped `.so` (and occasionally `.so.dbg`) debug objects.
        if !(name.ends_with(".so") || name.ends_with(".so.dbg")) {
            continue;
        }

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes)?;
        let (build_id, arch) = parse_elf_identity(&bytes);

        let base = Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("lib.so");
        // Entries across ABIs can share a basename — prefix with the index.
        let out_path = out_dir.join(format!("{i}_{base}"));
        std::fs::write(&out_path, &bytes)?;

        libs.push(ElfLib {
            name,
            build_id,
            arch,
            path: out_path,
        });
    }
    Ok(libs)
}

/// Parse the first ELF object's GNU build-id (`code_id`) + arch. Returns
/// `(None, "unknown")` when the bytes are not a parseable ELF or carry no
/// build-id.
fn parse_elf_identity(bytes: &[u8]) -> (Option<String>, String) {
    let archive = match Archive::parse(bytes) {
        Ok(a) => a,
        Err(_) => return (None, "unknown".to_string()),
    };
    match archive.objects().next() {
        Some(Ok(obj)) => (
            obj.code_id().map(|c| c.as_str().to_owned()),
            obj.arch().name().to_owned(),
        ),
        _ => (None, "unknown".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn sha1_hex_of_file_matches_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(b"abc")
            .unwrap();
        // FIPS-180 SHA-1("abc").
        assert_eq!(
            sha1_hex_of_file(&path).unwrap(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn parse_elf_identity_on_non_elf_returns_none_unknown() {
        let (build_id, arch) = parse_elf_identity(b"this is not an ELF file");
        assert_eq!(build_id, None);
        assert_eq!(arch, "unknown");
    }

    #[test]
    fn parse_elf_identity_reads_real_fixture_build_id_and_arch() {
        // The committed aarch64 fixture — `file(1)` reports
        // BuildID[md5/uuid]=bca64abfec40dbb631bb8f1c37414472. The other unit
        // tests only cover the non-ELF "unknown" fallback; pin that `symbolic`
        // returns BOTH the canonical GNU build-id AND the arch for a real ELF,
        // so a version bump that drifted either surfaces here.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/elf/libsymbol1.so");
        let bytes = std::fs::read(&fixture).unwrap();
        let (build_id, arch) = parse_elf_identity(&bytes);
        assert_eq!(
            build_id.as_deref(),
            Some("bca64abfec40dbb631bb8f1c37414472")
        );
        assert_eq!(arch, "arm64");
    }

    #[test]
    fn extract_libs_collects_so_entries_skips_others_and_extracts_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("native-debug-symbols.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = SimpleFileOptions::default();
            // A `.so` entry (non-ELF content → no build-id, but still collected
            // so the caller can warn+skip it).
            zw.start_file("arm64-v8a/libfoo.so", opts).unwrap();
            zw.write_all(b"not a real elf").unwrap();
            // A non-`.so` entry must be ignored.
            zw.start_file("manifest.json", opts).unwrap();
            zw.write_all(b"{}").unwrap();
            zw.finish().unwrap();
        }
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        let libs = extract_libs(&zip_path, &out).unwrap();
        assert_eq!(libs.len(), 1, "only the .so entry is collected");
        assert_eq!(libs[0].name, "arm64-v8a/libfoo.so");
        assert_eq!(libs[0].build_id, None, "non-ELF content yields no build-id");
        assert!(
            libs[0].path.exists(),
            "the .so bytes were extracted to disk"
        );
        assert_eq!(std::fs::read(&libs[0].path).unwrap(), b"not a real elf");
    }
}
