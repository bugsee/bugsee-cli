//! NDK / native debug-symbol identification — per-library, build-id keyed.
//!
//! The caller hands the CLI an already-packaged `native-debug-symbols.zip`
//! (the artifact AGP writes under `build/outputs/native-debug-symbols/
//! <variant>/`) or a directory of libraries. We open/walk it and, for EACH `.so` (or the `.so.dbg` / `.so.sym`
//! variants, depending on `ndk.debugSymbolLevel`), read the GNU build-id
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
//! The input is either that zip or a directory, walked recursively for the same
//! entry names (AGP's `merged_native_libs` intermediates folder) — libraries
//! are read in place, so nothing is re-compressed just to be unpacked again.

use sha1::{Digest as _, Sha1};
use std::io::Read;
use std::path::{Path, PathBuf};
use symbolic_debuginfo::Archive;

use super::suffix::ExtraSuffixes;

/// SHA-1 hex of a file's bytes — the wire `hash` for a per-`.so` upload.
pub fn sha1_hex_of_file(path: &Path) -> std::io::Result<String> {
    // Streamed: the hashed file is the packed archive, which can be tens of MB.
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest: [u8; 20] = hasher.finalize().into();
    Ok(hex::encode(digest))
}

/// One native library found in an AGP `native-debug-symbols.zip` or a scanned directory.
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
    /// File holding the library bytes, packed into its own upload: a temp copy
    /// extracted from a zip, or the original file when scanned from a directory.
    pub path: PathBuf,
    /// How much this file can symbolicate: (DWARF debug info, symbol table,
    /// byte size), compared in that order by [`keep_richest_per_build_id`].
    richness: (bool, bool, u64),
}

/// Extract every ELF `.so` from `archive_path` into `out_dir`, reading each
/// library's GNU build-id. An entry counts as a library when its name ends in
/// a built-in suffix (see [`is_native_lib_entry`]) or one of `extra`. The wire `code_id` is produced by the same
/// `symbolic` crate family (major 13) the worker uses, so the identifier is
/// byte-identical producer↔consumer.
pub fn extract_libs(
    archive_path: &Path,
    out_dir: &Path,
    extra: &ExtraSuffixes,
) -> std::io::Result<Vec<ElfLib>> {
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
        if !(is_native_lib_entry(&name) || extra.matches(&name)) {
            continue;
        }

        let base = Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("lib.so");
        // Entries across ABIs can share a basename — prefix with the index.
        let out_path = out_dir.join(format!("{i}_{base}"));
        // Stream the entry to disk, then read its identity from the file: an
        // inflated unstripped library is 100+ MB and was held whole in memory.
        {
            let mut out = std::fs::File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out)?;
        }
        let ElfIdentity {
            build_id,
            arch,
            has_debug_info,
            has_symbols,
        } = parse_elf_identity(&super::mapped::map_file(&out_path)?);
        let richness = (
            has_debug_info,
            has_symbols,
            std::fs::metadata(&out_path)?.len(),
        );

        libs.push(ElfLib {
            name,
            build_id,
            arch,
            path: out_path,
            richness,
        });
    }
    Ok(libs)
}

/// Collect native libraries from `input`: a directory is walked recursively
/// (see [`scan_dir`]), anything else is read as a `native-debug-symbols.zip`
/// and extracted into `out_dir` (see [`extract_libs`]).
pub fn collect_libs(
    input: &Path,
    out_dir: &Path,
    extra: &ExtraSuffixes,
) -> std::io::Result<Vec<ElfLib>> {
    if input.is_dir() {
        scan_dir(input, extra)
    } else {
        extract_libs(input, out_dir, extra)
    }
}

/// Walk `dir` recursively for native libraries (same name rules as the zip
/// entries), reading each one's identity in place. `name` is the path relative
/// to `dir`. Directory symlinks are not descended, file symlinks are read. Any
/// I/O error (unreadable subdirectory, unreadable library, dangling link) fails
/// the scan: skipping would upload a partial set (a whole ABI missing) and let
/// the build go green, the same way the zip path fails on a read error. Sorted
/// by path so the "first entry wins" tie-break in [`keep_richest_per_build_id`]
/// is stable.
pub fn scan_dir(dir: &Path, extra: &ExtraSuffixes) -> std::io::Result<Vec<ElfLib>> {
    let mut libs = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry?;
        // Directory symlinks are never descended (a walk must stay under the
        // root); a symlink to a FILE is a candidate and `read` follows it.
        let ft = entry.file_type();
        if !(ft.is_file() || ft.is_symlink()) {
            continue;
        }
        let file_name = entry.file_name().to_string_lossy();
        if !(is_native_lib_entry(&file_name) || extra.matches(&file_name)) {
            continue;
        }
        let with_path = |e: std::io::Error| {
            std::io::Error::new(e.kind(), format!("{}: {e}", entry.path().display()))
        };
        // Mapped, not read: identity needs only the headers and notes, so a
        // 100+ MB unstripped library costs a few KB of resident memory.
        let bytes = super::mapped::map_file(entry.path()).map_err(with_path)?;
        let len = bytes.len() as u64;
        let ElfIdentity {
            build_id,
            arch,
            has_debug_info,
            has_symbols,
        } = parse_elf_identity(&bytes);
        let name = entry
            .path()
            .strip_prefix(dir)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        libs.push(ElfLib {
            name,
            build_id,
            arch,
            path: entry.path().to_path_buf(),
            richness: (has_debug_info, has_symbols, len),
        });
    }
    Ok(libs)
}

/// Whether a `native-debug-symbols.zip` entry is a native library to upload.
/// The suffix depends on AGP's `ndk.debugSymbolLevel`: `FULL` packs unstripped
/// `.so` (occasionally `.so.dbg`), while `SYMBOL_TABLE` packs `.so.sym` — an
/// ELF that still carries `.note.gnu.build-id`, so it is keyed the same way
/// (function names only; no line info).
fn is_native_lib_entry(name: &str) -> bool {
    [".so", ".so.dbg", ".so.sym"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// Keep ONE library per GNU build-id: the one that symbolicates best.
///
/// The server dedups on the build-id alone, so two entries sharing one are not
/// two symbols — they are the same library in two forms, e.g. a stripped
/// `libfoo.so` next to its GNU split-debug `libfoo.so.debug` (which an
/// `--extension` suffix can add to the set). Uploading both concurrently let
/// whichever registered first win, often the stripped one, leaving crashes
/// matched but without line info. Preference: DWARF debug info, then a symbol
/// table, then the larger file; on a full tie the first entry wins. Entries
/// without a build-id pass through for the caller's warn-and-skip. The order of
/// the kept entries is preserved.
pub fn keep_richest_per_build_id(libs: Vec<ElfLib>) -> Vec<ElfLib> {
    let mut best: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, lib) in libs.iter().enumerate() {
        if let Some(id) = &lib.build_id {
            let b = best.entry(id.clone()).or_insert(i);
            if lib.richness > libs[*b].richness {
                *b = i;
            }
        }
    }
    for (i, lib) in libs.iter().enumerate() {
        if let Some(id) = &lib.build_id {
            let winner = best[id];
            if winner != i {
                tracing::info!(
                    dropped = %lib.name,
                    kept = %libs[winner].name,
                    build_id = %id,
                    "same GNU build-id as a richer entry; uploading only the richer one"
                );
            }
        }
    }
    libs.into_iter()
        .enumerate()
        .filter(|(i, lib)| lib.build_id.as_ref().is_none_or(|id| best[id] == *i))
        .map(|(_, lib)| lib)
        .collect()
}

struct ElfIdentity {
    build_id: Option<String>,
    arch: String,
    has_debug_info: bool,
    has_symbols: bool,
}

/// Parse the first ELF object's GNU build-id (`code_id`), arch, and what it
/// carries for symbolication. No build-id and arch `"unknown"` when the bytes
/// are not a parseable ELF.
fn parse_elf_identity(bytes: &[u8]) -> ElfIdentity {
    let unknown = || ElfIdentity {
        build_id: None,
        arch: "unknown".to_string(),
        has_debug_info: false,
        has_symbols: false,
    };
    let archive = match Archive::parse(bytes) {
        Ok(a) => a,
        Err(_) => return unknown(),
    };
    match archive.objects().next() {
        Some(Ok(obj)) => ElfIdentity {
            build_id: obj.code_id().map(|c| c.as_str().to_owned()),
            arch: obj.arch().name().to_owned(),
            has_debug_info: obj.has_debug_info(),
            has_symbols: obj.has_symbols(),
        },
        _ => unknown(),
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
    fn sha1_hex_of_file_streams_across_buffer_boundaries() {
        // Larger than the 64 KiB read buffer and not a multiple of it, so a
        // chunking bug (dropped or repeated tail) changes the digest.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        let data: Vec<u8> = (0..300_001u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let expected: [u8; 20] = Sha1::digest(&data).into();
        assert_eq!(sha1_hex_of_file(&path).unwrap(), hex::encode(expected));
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            sha1_hex_of_file(&path).unwrap(),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "SHA-1 of the empty input"
        );
    }

    #[test]
    fn scan_dir_handles_an_empty_library_file() {
        // Mapping a zero-length file fails; it must come out as "no build-id"
        // (warn + skip upstream), not as a scan error.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("libempty.so"), b"").unwrap();
        let libs = scan_dir(dir.path(), &ExtraSuffixes::default()).unwrap();
        assert_eq!(names(&libs), ["libempty.so"]);
        assert_eq!(libs[0].build_id, None);
    }

    #[test]
    fn parse_elf_identity_on_non_elf_returns_none_unknown() {
        let id = parse_elf_identity(b"this is not an ELF file");
        assert_eq!(id.build_id, None);
        assert_eq!(id.arch, "unknown");
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
        let id = parse_elf_identity(&bytes);
        assert_eq!(
            id.build_id.as_deref(),
            Some("bca64abfec40dbb631bb8f1c37414472")
        );
        assert_eq!(id.arch, "arm64");
    }

    #[test]
    fn is_native_lib_entry_accepts_every_agp_symbol_level_suffix() {
        assert!(is_native_lib_entry("arm64-v8a/libfoo.so"));
        assert!(is_native_lib_entry("arm64-v8a/libfoo.so.dbg"));
        assert!(is_native_lib_entry("arm64-v8a/libfoo.so.sym"));
        assert!(!is_native_lib_entry("manifest.json"));
        assert!(!is_native_lib_entry("arm64-v8a/libfoo.so.txt"));
        assert!(!is_native_lib_entry("arm64-v8a/libfoo.sym"));
    }

    #[test]
    fn extract_libs_keys_symbol_table_so_sym_by_build_id() {
        // AGP `ndk.debugSymbolLevel = 'SYMBOL_TABLE'` packs ONLY `<abi>/lib*.so.sym`.
        // Before #61 every such entry was dropped and the upload silently sent
        // nothing. A `.so.sym` is an ELF with `.note.gnu.build-id`, so the
        // real fixture's bytes under that name must key by its build-id.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/elf/libsymbol1.so");
        let elf_bytes = std::fs::read(&fixture).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("native-debug-symbols.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = SimpleFileOptions::default();
            zw.start_file("arm64-v8a/libsymbol1.so.sym", opts).unwrap();
            zw.write_all(&elf_bytes).unwrap();
            zw.finish().unwrap();
        }
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        let libs = extract_libs(&zip_path, &out, &ExtraSuffixes::default()).unwrap();
        assert_eq!(libs.len(), 1, "the .so.sym entry is collected");
        assert_eq!(libs[0].name, "arm64-v8a/libsymbol1.so.sym");
        assert_eq!(
            libs[0].build_id.as_deref(),
            Some("bca64abfec40dbb631bb8f1c37414472")
        );
        assert_eq!(libs[0].arch, "arm64");
        assert_eq!(std::fs::read(&libs[0].path).unwrap(), elf_bytes);
    }

    #[test]
    fn extract_libs_collects_extra_suffix_entries() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("native-debug-symbols.zip");
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = SimpleFileOptions::default();
            zw.start_file("arm64-v8a/libfoo.so", opts).unwrap();
            zw.write_all(b"elf").unwrap();
            zw.start_file("arm64-v8a/libbar.so.debug", opts).unwrap();
            zw.write_all(b"elf").unwrap();
            zw.finish().unwrap();
        }
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();

        let names = |extra: &ExtraSuffixes| -> Vec<String> {
            extract_libs(&zip_path, &out, extra)
                .unwrap()
                .into_iter()
                .map(|l| l.name)
                .collect()
        };
        assert_eq!(names(&ExtraSuffixes::default()), ["arm64-v8a/libfoo.so"]);
        let extra = ExtraSuffixes::parse(&["so.debug".to_string()]).unwrap();
        assert_eq!(
            names(&extra),
            ["arm64-v8a/libfoo.so", "arm64-v8a/libbar.so.debug"],
            "extra suffixes ADD to the built-in ones"
        );
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

        let libs = extract_libs(&zip_path, &out, &ExtraSuffixes::default()).unwrap();
        assert_eq!(libs.len(), 1, "only the .so entry is collected");
        assert_eq!(libs[0].name, "arm64-v8a/libfoo.so");
        assert_eq!(libs[0].build_id, None, "non-ELF content yields no build-id");
        assert!(
            libs[0].path.exists(),
            "the .so bytes were extracted to disk"
        );
        assert_eq!(std::fs::read(&libs[0].path).unwrap(), b"not a real elf");
    }

    fn fixture_bytes() -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so"),
        )
        .unwrap()
    }

    #[test]
    fn scan_dir_walks_recursively_in_place_and_skips_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = fixture_bytes();
        for rel in [
            "lib/arm64-v8a/libsymbol1.so",
            "lib/x86_64/libsymbol1.so.sym",
            "lib/x86_64/readme.txt",
        ] {
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, &bytes).unwrap();
        }
        std::fs::write(dir.path().join("lib/x86_64/libbare.so"), b"not elf").unwrap();

        let libs = scan_dir(dir.path(), &ExtraSuffixes::default()).unwrap();
        assert_eq!(
            names(&libs),
            [
                "lib/arm64-v8a/libsymbol1.so",
                "lib/x86_64/libbare.so",
                "lib/x86_64/libsymbol1.so.sym"
            ]
        );
        assert_eq!(
            libs[0].build_id.as_deref(),
            Some("bca64abfec40dbb631bb8f1c37414472")
        );
        assert_eq!(libs[1].build_id, None, "non-ELF yields no build-id");
        assert_eq!(
            libs[0].path,
            dir.path().join("lib/arm64-v8a/libsymbol1.so"),
            "read in place, not copied"
        );

        let extra = ExtraSuffixes::parse(&["txt".to_string()]).unwrap();
        assert_eq!(scan_dir(dir.path(), &extra).unwrap().len(), 4);
    }

    #[test]
    fn collect_libs_dispatches_on_directory_vs_zip() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = fixture_bytes();
        let libdir = dir.path().join("libs");
        std::fs::create_dir_all(&libdir).unwrap();
        std::fs::write(libdir.join("libsymbol1.so"), &bytes).unwrap();
        let zip_path = dir.path().join("n.zip");
        {
            let mut zw = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            zw.start_file("arm64-v8a/libsymbol1.so", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(&bytes).unwrap();
            zw.finish().unwrap();
        }
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let x = ExtraSuffixes::default();
        assert_eq!(
            names(&collect_libs(&libdir, &out, &x).unwrap()),
            ["libsymbol1.so"]
        );
        assert_eq!(
            names(&collect_libs(&zip_path, &out, &x).unwrap()),
            ["arm64-v8a/libsymbol1.so"]
        );
    }

    fn lib(name: &str, build_id: Option<&str>, richness: (bool, bool, u64)) -> ElfLib {
        ElfLib {
            name: name.to_string(),
            build_id: build_id.map(str::to_string),
            arch: "arm64".to_string(),
            path: PathBuf::from(name),
            richness,
        }
    }

    fn names(libs: &[ElfLib]) -> Vec<&str> {
        libs.iter().map(|l| l.name.as_str()).collect()
    }

    #[test]
    fn keep_richest_prefers_debug_info_over_a_stripped_companion() {
        // GNU split-debug layout: the stripped `.so` FIRST in the zip, its
        // `.so.debug` (DWARF) second, same build-id.
        let kept = keep_richest_per_build_id(vec![
            lib("arm64-v8a/libfoo.so", Some("abc"), (false, false, 10_000)),
            lib(
                "arm64-v8a/libfoo.so.debug",
                Some("abc"),
                (true, true, 4_000),
            ),
            lib("arm64-v8a/libbar.so", Some("def"), (false, true, 500)),
        ]);
        assert_eq!(
            names(&kept),
            ["arm64-v8a/libfoo.so.debug", "arm64-v8a/libbar.so"]
        );
    }

    #[test]
    fn keep_richest_ranks_symbols_then_size_and_passes_unkeyed_through() {
        let kept = keep_richest_per_build_id(vec![
            lib("a/libfoo.so", Some("abc"), (false, false, 9_000)),
            lib("a/libfoo.so.sym", Some("abc"), (false, true, 1_000)),
            lib("a/libbig.so", Some("x"), (false, true, 10)),
            lib("a/libbig.so.sym", Some("x"), (false, true, 20)),
            lib("a/nobuildid.so", None, (true, true, 1)),
            lib("b/nobuildid.so", None, (true, true, 1)),
        ]);
        assert_eq!(
            names(&kept),
            [
                "a/libfoo.so.sym",
                "a/libbig.so.sym",
                "a/nobuildid.so",
                "b/nobuildid.so"
            ]
        );
    }

    #[test]
    fn parse_elf_identity_reads_symbolication_content_from_the_elf() {
        // `richness` must come from the object itself, never from the name.
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/elf/libsymbol1.so");
        let id = parse_elf_identity(&std::fs::read(fixture).unwrap());
        assert!(id.has_symbols);
    }
}
