//! Compression helpers.
//!
//! Default wire format: ZIP container with Zstd-compressed entries (the Bugsee worker
//! accepts ZIP files with `Z_STANDARD` (compression method 93) entries today).
//!
//! Zstd compression level defaults to 11. Level 9 is the floor for production uploads;
//! level 11 trades a small amount of CPU for noticeably better ratios on large debug
//! artifacts (libil2cpp.so, Flutter Dart `.symbols`, dSYMs).
//!
//! The `--no-zstd` flag falls back to DEFLATE for diagnostic purposes only.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::config_invalid;

/// Default Zstd compression level used for symbol uploads.
pub const DEFAULT_ZSTD_LEVEL: i64 = 11;

/// Minimum acceptable compression level for production builds.
pub const MIN_PRODUCTION_ZSTD_LEVEL: i64 = 9;

/// Compression strategy for a single ZIP archive.
#[derive(Debug, Clone, Copy)]
pub enum Strategy {
    /// ZIP entry compressed with Zstd at the given level (range 1..=22).
    Zstd(i64),
    /// ZIP entry compressed with DEFLATE (diagnostic / legacy compatibility).
    Deflate,
}

impl Default for Strategy {
    fn default() -> Self {
        Strategy::Zstd(DEFAULT_ZSTD_LEVEL)
    }
}

impl Strategy {
    fn file_options(self) -> SimpleFileOptions {
        match self {
            Strategy::Zstd(level) => SimpleFileOptions::default()
                .compression_method(CompressionMethod::Zstd)
                .compression_level(Some(level)),
            Strategy::Deflate => {
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
            }
        }
    }
}

/// Resolve a compression [`Strategy`] from the shared `--no-zstd` / `--zstd-level`
/// flags. Centralised here so every upload subcommand applies the SAME
/// production floor — drifting the floor across subcommands would let a
/// caller silently ship under-compressed (or accidentally DEFLATE) payloads
/// the worker has to handle differently.
///
/// - `--no-zstd` selects DEFLATE (diagnostic only) and is incompatible with
///   an explicit level.
/// - Otherwise the level defaults to [`DEFAULT_ZSTD_LEVEL`], must be in
///   `1..=22`, and is rejected below [`MIN_PRODUCTION_ZSTD_LEVEL`].
pub fn resolve_strategy(no_zstd: bool, zstd_level: Option<i64>) -> anyhow::Result<Strategy> {
    if no_zstd {
        if zstd_level.is_some() {
            return Err(config_invalid(
                "--zstd-level is incompatible with --no-zstd",
            ));
        }
        return Ok(Strategy::Deflate);
    }
    let level = zstd_level.unwrap_or(DEFAULT_ZSTD_LEVEL);
    if !(1..=22).contains(&level) {
        return Err(config_invalid(format!(
            "--zstd-level must be in 1..=22, got {level}"
        )));
    }
    if level < MIN_PRODUCTION_ZSTD_LEVEL {
        return Err(config_invalid(format!(
            "--zstd-level {level} is below the production floor of {MIN_PRODUCTION_ZSTD_LEVEL}; \
             pass --no-zstd if intentional"
        )));
    }
    Ok(Strategy::Zstd(level))
}

/// A single entry to embed in the archive.
#[derive(Debug, Clone, Copy)]
pub struct ZipEntry<'a> {
    /// Entry name within the archive (no leading slash).
    pub name: &'a str,
    /// Path to the file on disk whose bytes go into this entry.
    pub source: &'a Path,
    /// When `true`, this entry is written with STORE (method 0) regardless of
    /// the archive [`Strategy`] — the wire-format contract's "STORE-93 for
    /// already-compressed entries" rule. Re-compressing an entry that is
    /// itself a compressed container (`.aab`/`.apk`/`.ipa`, `.png`/`.mp4`)
    /// only burns CPU and can grow the entry, so such entries are stored verbatim.
    pub store: bool,
}

impl<'a> ZipEntry<'a> {
    /// An entry compressed with the archive's [`Strategy`]. Use for text /
    /// uncompressed sources (mapping.txt, dependencies.json, DWARF, …).
    pub fn compressed(name: &'a str, source: &'a Path) -> Self {
        Self {
            name,
            source,
            store: false,
        }
    }

    /// An entry written with STORE (method 0). Use for already-compressed
    /// containers where re-compression is wasted work.
    #[allow(dead_code)]
    pub fn stored(name: &'a str, source: &'a Path) -> Self {
        Self {
            name,
            source,
            store: true,
        }
    }
}

/// Pack a single file as one ZIP entry. Thin wrapper around [`pack_entries`].
///
/// Retained as a convenience for future symbol types whose archive is a single file
/// (e.g., raw `.so` upload), and used directly by the compression unit test.
#[allow(dead_code)]
pub fn pack_single_entry(
    input: &Path,
    entry_name: &str,
    output: &Path,
    strategy: Strategy,
) -> std::io::Result<u64> {
    pack_entries(&[ZipEntry::compressed(entry_name, input)], output, strategy)
}

/// Pack one or more files into a ZIP archive. Each entry streams through a
/// 64 KiB buffer to keep peak memory low; ordering matches the input slice
/// (matters for archives where layout is part of the contract).
///
/// Returns the compressed archive size in bytes.
pub fn pack_entries(
    entries: &[ZipEntry<'_>],
    output: &Path,
    strategy: Strategy,
) -> std::io::Result<u64> {
    let out_file = File::create(output)?;
    let mut zip = ZipWriter::new(BufWriter::new(out_file));
    let compressed_options = strategy.file_options();
    let stored_options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let mut buf = [0u8; 64 * 1024];
    for entry in entries {
        let options = if entry.store {
            stored_options
        } else {
            compressed_options
        };
        zip.start_file(entry.name, options)?;
        let mut reader = BufReader::new(File::open(entry.source)?);
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])?;
        }
    }

    let writer = zip.finish()?;
    let inner = writer.into_inner().map_err(|e| e.into_error())?;
    inner.sync_all()?;
    Ok(inner.metadata()?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::ZipArchive;

    #[test]
    fn zstd_roundtrip_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let input_path = dir.path().join("mapping.txt");
        let output_path = dir.path().join("packed.zip");

        let mut payload = String::new();
        for i in 0..100 {
            payload.push_str(&format!("class com.example.Foo{i} -> a.b.C{i}:\n"));
        }
        {
            let mut f = File::create(&input_path).unwrap();
            f.write_all(payload.as_bytes()).unwrap();
        }

        pack_single_entry(&input_path, "mapping.txt", &output_path, Strategy::Zstd(11)).unwrap();

        let file = File::open(&output_path).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_name("mapping.txt").unwrap();
        assert_eq!(entry.compression(), CompressionMethod::Zstd);
        let mut got = String::new();
        entry.read_to_string(&mut got).unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn multi_entry_pack_preserves_order_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let mapping_path = dir.path().join("mapping.txt");
        let icon_path = dir.path().join("icon.png");
        let output_path = dir.path().join("packed.zip");

        std::fs::write(&mapping_path, b"# mapping content\n").unwrap();
        // A fake icon — 8 bytes of PNG-ish noise; the packer doesn't inspect the bytes.
        std::fs::write(&icon_path, b"\x89PNG\r\n\x1a\n").unwrap();

        pack_entries(
            &[
                ZipEntry::compressed("mapping.txt", &mapping_path),
                ZipEntry::compressed("icon.png", &icon_path),
            ],
            &output_path,
            Strategy::Zstd(11),
        )
        .unwrap();

        let mut archive = ZipArchive::new(File::open(&output_path).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);

        // Iteration order matches insertion order.
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["mapping.txt", "icon.png"]);

        let mut mapping_bytes = Vec::new();
        archive
            .by_name("mapping.txt")
            .unwrap()
            .read_to_end(&mut mapping_bytes)
            .unwrap();
        assert_eq!(mapping_bytes, b"# mapping content\n");

        let mut icon_bytes = Vec::new();
        archive
            .by_name("icon.png")
            .unwrap()
            .read_to_end(&mut icon_bytes)
            .unwrap();
        assert_eq!(icon_bytes, b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn stored_entry_uses_method_zero_and_mixes_with_zstd() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("dependencies.json");
        let blob_path = dir.path().join("already.zip");
        let output_path = dir.path().join("packed.zip");

        // Highly compressible text → zstd entry.
        let json = "{\"deps\":[".to_string() + &"\"a\",".repeat(200) + "\"z\"]}";
        std::fs::write(&json_path, json.as_bytes()).unwrap();
        // Pretend-already-compressed container → stored entry.
        std::fs::write(&blob_path, b"PK\x03\x04 pretend zip bytes").unwrap();

        pack_entries(
            &[
                ZipEntry::compressed("dependencies.json", &json_path),
                ZipEntry::stored("already.zip", &blob_path),
            ],
            &output_path,
            Strategy::Zstd(11),
        )
        .unwrap();

        let mut archive = ZipArchive::new(File::open(&output_path).unwrap()).unwrap();

        let json_entry = archive.by_name("dependencies.json").unwrap();
        assert_eq!(json_entry.compression(), CompressionMethod::Zstd);
        drop(json_entry);

        let mut stored = archive.by_name("already.zip").unwrap();
        assert_eq!(stored.compression(), CompressionMethod::Stored);
        let mut bytes = Vec::new();
        stored.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"PK\x03\x04 pretend zip bytes");
    }

    #[test]
    fn resolve_strategy_applies_production_floor_and_flag_rules() {
        // Default is zstd at the documented default level.
        assert!(matches!(
            resolve_strategy(false, None).unwrap(),
            Strategy::Zstd(level) if level == DEFAULT_ZSTD_LEVEL
        ));
        // Explicit in-range level is honoured.
        assert!(matches!(
            resolve_strategy(false, Some(15)).unwrap(),
            Strategy::Zstd(15)
        ));
        // --no-zstd selects DEFLATE.
        assert!(matches!(
            resolve_strategy(true, None).unwrap(),
            Strategy::Deflate
        ));
        // Below the production floor is rejected.
        assert!(resolve_strategy(false, Some(MIN_PRODUCTION_ZSTD_LEVEL - 1)).is_err());
        // At the floor is allowed.
        assert!(resolve_strategy(false, Some(MIN_PRODUCTION_ZSTD_LEVEL)).is_ok());
        // Out of the 1..=22 range is rejected.
        assert!(resolve_strategy(false, Some(0)).is_err());
        assert!(resolve_strategy(false, Some(23)).is_err());
        // --no-zstd with an explicit level is a contradiction.
        assert!(resolve_strategy(true, Some(11)).is_err());
    }
}
