//! `bugsee-cli pack` — build the normalized build-upload ZIP locally.
//!
//! Produces the artefact-upload container the Bugsee worker's size-analysis
//! job consumes (`builds.builds_helpers._extract_upload_archive`): the build
//! artefact (`.aab`/`.apk`/`.ipa`) STORED verbatim, plus an optional
//! R8/ProGuard `mapping.txt` compressed with zstd (method 93).
//!
//! This is the single canonical packer. The Gradle plugin shells to it instead
//! of re-implementing ZIP + zstd compression, so the embedded mapping ships
//! zstd-compressed (≈zstd-19) without the producer pulling in `zstd-jni` /
//! `commons-compress`. The worker reads the method-93 `mapping.txt` entry
//! transparently through its zstd `zipfile` shim — no worker change.
//!
//! Local-only: no network I/O. The producer uploads the resulting ZIP itself
//! (single-PUT or chunked), so transport stays with the caller.

use clap::Args;
use std::path::PathBuf;

use crate::compress::{self, ZipEntry};
use crate::error::{config_invalid, input_not_found};

#[derive(Args, Debug)]
pub struct PackArgs {
    /// Build artefact (`.aab` / `.apk` / `.ipa`). STORED verbatim — it is
    /// already a compressed container, so re-compressing only burns CPU and
    /// can grow the entry.
    #[arg(long)]
    pub artifact: PathBuf,

    /// Optional R8/ProGuard `mapping.txt`. Packed as the `mapping.txt` entry,
    /// zstd-compressed. Omit for non-obfuscated builds.
    #[arg(long)]
    pub mapping: Option<PathBuf>,

    /// Output ZIP path.
    #[arg(long)]
    pub out: PathBuf,

    /// Disable Zstd compression (diagnostic only — default is Zstd level 11).
    #[arg(long)]
    pub no_zstd: bool,

    /// Zstd level (1..=22). Defaults to 11; values below 9 are rejected.
    #[arg(long)]
    pub zstd_level: Option<i64>,
}

/// Worker-recognised build-artefact extensions (see
/// `_extract_upload_archive`'s `bundle_extensions`). Kept in sync so a
/// misnamed artefact fails loudly here rather than producing a ZIP the
/// worker treats as a raw bundle (and then never finds the mapping in).
const ARTIFACT_EXTENSIONS: [&str; 3] = [".aab", ".apk", ".ipa"];

pub fn dispatch(args: PackArgs) -> anyhow::Result<()> {
    if !args.artifact.is_file() {
        return Err(input_not_found(format!(
            "artifact does not exist or is not a file: {}",
            args.artifact.display()
        )));
    }
    if let Some(ref m) = args.mapping {
        if !m.is_file() {
            return Err(input_not_found(format!(
                "mapping does not exist or is not a file: {}",
                m.display()
            )));
        }
    }

    // The artefact entry MUST keep its extension: the worker detects the
    // bundle by `.apk`/`.aab`/`.ipa` suffix, and only when it finds a bundle
    // entry does it look for a sibling `mapping.txt`. A misnamed artefact
    // would make the worker treat the whole ZIP as a raw bundle and silently
    // drop the mapping.
    let artifact_name = args
        .artifact
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| config_invalid("artifact path has no usable file name"))?;
    let lower = artifact_name.to_ascii_lowercase();
    if !ARTIFACT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        return Err(config_invalid(format!(
            "artifact must end in .aab/.apk/.ipa, got: {artifact_name}"
        )));
    }

    let strategy = compress::resolve_strategy(args.no_zstd, args.zstd_level)?;

    // Stable order: artefact first (STORED), mapping second (zstd). Order is
    // immaterial to the worker (it scans all entries) but must be stable so
    // packing identical inputs yields byte-identical archives — the artefact
    // upload is chunk-deduplicated.
    let mut entries = vec![ZipEntry::stored(artifact_name, &args.artifact)];
    if let Some(ref m) = args.mapping {
        entries.push(ZipEntry::compressed("mapping.txt", m));
    }

    compress::pack_entries(&entries, &args.out, strategy)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use zip::{CompressionMethod, ZipArchive};

    fn write(path: &std::path::Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
    }

    fn args(artifact: PathBuf, mapping: Option<PathBuf>, out: PathBuf) -> PackArgs {
        PackArgs {
            artifact,
            mapping,
            out,
            no_zstd: false,
            zstd_level: None,
        }
    }

    #[test]
    fn packs_artifact_stored_and_mapping_zstd() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("app.apk");
        let mapping = dir.path().join("mapping.txt");
        let out = dir.path().join("upload.zip");
        write(&artifact, b"PK\x03\x04 fake apk container");
        write(
            &mapping,
            "com.example.Foo -> a.b.C:\n".repeat(300).as_bytes(),
        );

        dispatch(args(artifact, Some(mapping), out.clone())).unwrap();

        let mut zip = ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(zip.len(), 2);
        // Artefact STORED under its original name so the worker detects the bundle.
        let apk = zip.by_name("app.apk").unwrap();
        assert_eq!(apk.compression(), CompressionMethod::Stored);
        drop(apk);
        // Mapping zstd under the exact name the worker keys on.
        let map = zip.by_name("mapping.txt").unwrap();
        assert_eq!(map.compression(), CompressionMethod::Zstd);
    }

    #[test]
    fn mapping_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("app.aab");
        let out = dir.path().join("upload.zip");
        write(&artifact, b"PK\x03\x04 fake aab");

        dispatch(args(artifact, None, out.clone())).unwrap();

        let zip = ZipArchive::new(fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(zip.len(), 1);
    }

    #[test]
    fn rejects_artifact_without_known_extension() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("artifact.bin");
        let out = dir.path().join("upload.zip");
        write(&artifact, b"not a bundle");

        let err = dispatch(args(artifact, None, out)).unwrap_err();
        assert!(err.to_string().contains(".aab/.apk/.ipa"), "got: {err}");
    }

    #[test]
    fn rejects_missing_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("missing.apk");
        let out = dir.path().join("upload.zip");
        let err = dispatch(args(artifact, None, out)).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn rejects_missing_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = dir.path().join("app.apk");
        let mapping = dir.path().join("absent.txt");
        let out = dir.path().join("upload.zip");
        write(&artifact, b"PK\x03\x04");
        let err = dispatch(args(artifact, Some(mapping), out)).unwrap_err();
        assert!(
            err.to_string().contains("mapping does not exist"),
            "got: {err}"
        );
    }
}
