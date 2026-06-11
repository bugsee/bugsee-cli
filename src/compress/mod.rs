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

/// A single entry to embed in the archive.
#[derive(Debug, Clone, Copy)]
pub struct ZipEntry<'a> {
    /// Entry name within the archive (no leading slash).
    pub name: &'a str,
    /// Path to the file on disk whose bytes go into this entry.
    pub source: &'a Path,
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
    pack_entries(
        &[ZipEntry {
            name: entry_name,
            source: input,
        }],
        output,
        strategy,
    )
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
    let options = strategy.file_options();

    let mut buf = [0u8; 64 * 1024];
    for entry in entries {
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
                ZipEntry {
                    name: "mapping.txt",
                    source: &mapping_path,
                },
                ZipEntry {
                    name: "icon.png",
                    source: &icon_path,
                },
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
}
