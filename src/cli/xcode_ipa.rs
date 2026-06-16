//! `.app` → synthetic `.ipa` packaging + main-executable `LC_UUID` extraction —
//! the Rust port of the iOS BugseeAgent's `package_app_as_ipa` and
//! `get_main_executable_uuid`.
//!
//! The synthetic `.ipa` mirrors Apple's real layout — the `.app` placed under
//! `Payload/` — which is exactly what the back-end's IPA analyser walks
//! (`Payload/*.app`). We omit only the code-signing + iTunesMetadata that App
//! Store Connect needs; size analysis runs off the Mach-O contents and resource
//! tree, neither of which cares about signing.
//!
//! Determinism: entry order is sorted by archive name and every entry carries a
//! fixed DOS-epoch mtime, so two packs of identical bytes hash identically — the
//! artefact upload is chunk-deduplicated, so identical inputs MUST produce
//! byte-identical archives across runs.
//!
//! `LC_UUID`: the main executable's Mach-O UUID is the SAME identifier the
//! runtime SDK reports with every crash (`BGSCrashReport.m`); sending it as the
//! build record's `uuid` lets the server join `crash.uuid → build`
//! deterministically. The linker assigns a fresh `LC_UUID` per build.

use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use symbolic_debuginfo::Archive;
use uuid::Uuid;
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::cli::build_env;
use crate::error::{Error, Result};

/// Filename extensions stored verbatim (STORE, no recompression) — already-
/// compressed formats where re-wrapping only burns CPU and can grow the entry.
/// Lowercased suffix match. Mirrors the agent's `_IPA_STORE_EXTENSIONS`.
const STORE_EXTENSIONS: &[&str] = &[
    // Raster images with native compression.
    ".png", ".jpg", ".jpeg", ".heic", ".heif", ".webp", // PDF uses its own stream compression.
    ".pdf",  // Audio / video.
    ".mp3", ".mp4", ".mov", ".aac", ".m4a",
    ".m4v", // Container formats we should never re-wrap.
    ".zip", ".gz", // Web fonts ship compressed.
    ".woff", ".woff2",
];

/// Architecture preference order for selecting the build `uuid` from a fat
/// Mach-O. `arm64` is the shipping device arch (deterministic on-device crash →
/// build lookup); fall back through `arm64e`/`x86_64`, then the simulator
/// arches, then the first slice (exotic configurations). Names match
/// `symbolic`'s `Arch::name()` — verbatim parity with the agent's
/// `_PREFERRED_MACHO_ARCHS`. The `-simulator` entries make a fat simulator-only
/// build pick `arm64-simulator` over `x86_64-simulator` deterministically
/// (matching the simulator host's reported slice on Apple Silicon) rather than
/// whichever slice the fat header happens to list first; they are harmless if a
/// given `symbolic` build doesn't surface the `-simulator` suffix.
const PREFERRED_ARCHS: &[&str] = &[
    "arm64",
    "arm64e",
    "x86_64",
    "arm64-simulator",
    "x86_64-simulator",
];

/// Fixed ZIP entry timestamp (DOS epoch, 1980-01-01). Pins byte determinism
/// independent of the `zip` crate's optional `time` feature (which would
/// otherwise stamp the wall clock). Mirrors the agent's `_IPA_FIXED_MTIME` and
/// `compress::fixed_mtime`.
fn fixed_mtime() -> zip::DateTime {
    zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0)
        .expect("1980-01-01 00:00:00 is a valid DOS timestamp")
}

fn is_store_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    STORE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

/// Collect every regular file under `app_path` (symlinks skipped, never
/// followed) paired with its in-archive name (`Payload/<App>.app/<rel>`), sorted
/// by archive name for deterministic output.
fn collect_entries(app_path: &Path) -> Vec<(PathBuf, String)> {
    // `rel` is relative to the .app's PARENT, so the archive name starts at the
    // `.app` directory itself (`Payload/<App>.app/...`).
    let parent = app_path.parent().unwrap_or(app_path);
    let mut entries: Vec<(PathBuf, String)> = Vec::new();
    for entry in WalkDir::new(app_path).follow_links(false) {
        // A subtree that can't be read is skipped (mirrors `os.walk`'s default
        // error-swallow), not fatal — best-effort packaging.
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ft = entry.file_type();
        // Skip symlinks (zip can't store them and real IPAs don't carry them)
        // and anything that isn't a regular file.
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let src = entry.path();
        let rel = match src.strip_prefix(parent) {
            Ok(r) => r,
            Err(_) => continue,
        };
        // Build the archive name with forward slashes (ZIP convention) from the
        // path components — correct regardless of host separator.
        let mut arcname = String::from("Payload");
        for comp in rel.components() {
            if let std::path::Component::Normal(os) = comp {
                arcname.push('/');
                arcname.push_str(&os.to_string_lossy());
            }
        }
        entries.push((src.to_path_buf(), arcname));
    }
    entries.sort_by(|a, b| a.1.cmp(&b.1));
    entries
}

/// Zip `<App>.app` as `Payload/<App>.app/...` into `out_ipa`. DEFLATE for
/// text/plists, STORE for already-compressed assets; POSIX permission bits are
/// preserved (on Unix) so the back-end's main-binary detection keeps its
/// executable-bit signal. Port of `package_app_as_ipa`.
pub fn package_app_as_ipa(app_path: &Path, out_ipa: &Path) -> Result<()> {
    if !app_path.is_dir() {
        return Err(Error::InputInvalid(format!(
            "expected a .app bundle directory, got {}",
            app_path.display()
        )));
    }

    // The `zip` crate's errors convert into `std::io::Error` (and from there into
    // our `Error`), so do the archive writing in an `io::Result` body and let the
    // `?` chain lift everything uniformly.
    write_ipa(app_path, out_ipa)?;
    Ok(())
}

fn write_ipa(app_path: &Path, out_ipa: &Path) -> std::io::Result<()> {
    let out_file = std::fs::File::create(out_ipa)?;
    let mut zip = ZipWriter::new(std::io::BufWriter::new(out_file));
    let mtime = fixed_mtime();

    let mut buf = [0u8; 64 * 1024];
    for (src, arcname) in collect_entries(app_path) {
        let method = if is_store_extension(&arcname) {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        #[allow(unused_mut)]
        let mut options = SimpleFileOptions::default()
            .compression_method(method)
            .last_modified_time(mtime);
        // Preserve POSIX permission bits on Unix so the synthetic `.ipa` matches
        // a real IPA's layout (executable bit on the Mach-O). NB: the worker's
        // main-binary detection is path-heuristic, not exec-bit based, so this is
        // for fidelity, not a hard worker dependency. No-op on other hosts — the
        // post-action only runs on macOS, but the crate must compile cross-platform.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&src) {
                options = options.unix_permissions(meta.permissions().mode());
            }
        }

        zip.start_file(&arcname, options)?;
        let mut reader = BufReader::new(std::fs::File::open(&src)?);
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])?;
        }
    }

    let writer = zip.finish()?;
    writer
        .into_inner()
        .map_err(|e| e.into_error())?
        .sync_all()?;
    Ok(())
}

/// From a list of `(arch_name, uuid)` Mach-O slices, pick the build `uuid`:
/// preferred arch order, else the first slice. Returns the canonical wire shape
/// (32 lowercase hex, no dashes — matching the runtime SDK's `LC_UUID`
/// reporting). `None` when there are no slices. Mirrors the agent's
/// arch-preference + `_normalise_build_uuid`.
fn select_preferred_uuid(slices: &[(String, Uuid)]) -> Option<String> {
    if slices.is_empty() {
        return None;
    }
    let selected = PREFERRED_ARCHS
        .iter()
        .find_map(|want| slices.iter().find(|(arch, _)| arch == want))
        .unwrap_or(&slices[0]);
    // `Uuid::simple()` renders 32 lowercase hex chars, no dashes — the canonical
    // shape. A nil (all-zero) id is treated as absent so the caller falls back to
    // a RANDOM uuid. This is a deliberate, justified deviation from the agent
    // (whose `_normalise_build_uuid` would emit `"0"*32`): a nil `LC_UUID` has no
    // crash-join value either way, and emitting a constant `"0"*32` would make
    // every nil-uuid build collide on the same build-record id, whereas a random
    // uuid keeps each build distinct. Unreachable for real linker output (which
    // always assigns a non-nil `LC_UUID`).
    if selected.1.is_nil() {
        return None;
    }
    Some(selected.1.simple().to_string())
}

/// Extract the main executable's Mach-O `LC_UUID`, formatted as 32 lowercase hex
/// chars (no dashes) to match the iOS SDK's runtime crash reporting. `None` on
/// any failure (missing `CFBundleExecutable`, binary absent / unparseable, no
/// UUID) — the caller then falls back to a random uuid so the build record still
/// lands, just without the crash-context join. Port of
/// `get_main_executable_uuid`.
pub fn main_executable_uuid(app_path: &Path) -> Option<String> {
    if !app_path.is_dir() {
        return None;
    }
    let plist = app_path.join("Info.plist");
    let map = build_env::read_plist_to_json(&plist);
    let executable_name = map
        .get("CFBundleExecutable")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;

    let binary_path = app_path.join(executable_name);
    if !binary_path.is_file() {
        return None;
    }

    let data = std::fs::read(&binary_path).ok()?;
    let archive = Archive::parse(&data).ok()?;
    let mut slices: Vec<(String, Uuid)> = Vec::new();
    for obj in archive.objects() {
        let obj = match obj {
            Ok(o) => o,
            Err(_) => continue,
        };
        slices.push((obj.arch().name().to_string(), obj.debug_id().uuid()));
    }
    select_preferred_uuid(&slices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use zip::ZipArchive;

    fn make_app(dir: &Path, name: &str) -> PathBuf {
        let app = dir.join(name);
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>MyApp</string>
</dict></plist>"#,
        )
        .unwrap();
        // A fake "Mach-O" main binary (not a real Mach-O — packaging doesn't
        // parse it) and an already-compressed asset.
        std::fs::write(app.join("MyApp"), b"\xcf\xfa\xed\xfe fake macho").unwrap();
        std::fs::write(app.join("icon.png"), b"\x89PNG\r\n\x1a\n fake png").unwrap();
        let sub = app.join("Frameworks").join("Lib.framework");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Lib"), b"\xcf\xfa\xed\xfe lib bytes").unwrap();
        app
    }

    #[test]
    fn packages_app_under_payload_with_per_file_method() {
        let td = tempfile::tempdir().unwrap();
        let app = make_app(td.path(), "MyApp.app");
        let ipa = td.path().join("MyApp.ipa");
        package_app_as_ipa(&app, &ipa).unwrap();

        let mut zip = ZipArchive::new(std::fs::File::open(&ipa).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"Payload/MyApp.app/Info.plist".to_string()));
        assert!(names.contains(&"Payload/MyApp.app/MyApp".to_string()));
        assert!(names.contains(&"Payload/MyApp.app/icon.png".to_string()));
        assert!(names.contains(&"Payload/MyApp.app/Frameworks/Lib.framework/Lib".to_string()));

        // Entry order is sorted by archive name.
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);

        // .png is STORED; Info.plist (text) is DEFLATED.
        assert_eq!(
            zip.by_name("Payload/MyApp.app/icon.png")
                .unwrap()
                .compression(),
            CompressionMethod::Stored
        );
        assert_eq!(
            zip.by_name("Payload/MyApp.app/Info.plist")
                .unwrap()
                .compression(),
            CompressionMethod::Deflated
        );
        // Content round-trips.
        let mut got = Vec::new();
        zip.by_name("Payload/MyApp.app/MyApp")
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert_eq!(got, b"\xcf\xfa\xed\xfe fake macho");
    }

    #[test]
    fn packaging_is_byte_deterministic() {
        let td = tempfile::tempdir().unwrap();
        let app = make_app(td.path(), "MyApp.app");
        let ipa1 = td.path().join("one.ipa");
        let ipa2 = td.path().join("two.ipa");
        package_app_as_ipa(&app, &ipa1).unwrap();
        package_app_as_ipa(&app, &ipa2).unwrap();
        assert_eq!(
            std::fs::read(&ipa1).unwrap(),
            std::fs::read(&ipa2).unwrap(),
            "identical inputs must pack byte-identically (chunk-dedup contract)"
        );
        // Fixed DOS-epoch timestamp, not the wall clock.
        let mut zip = ZipArchive::new(std::fs::File::open(&ipa1).unwrap()).unwrap();
        let entry = zip.by_name("Payload/MyApp.app/Info.plist").unwrap();
        assert_eq!(entry.last_modified().map(|d| d.year()), Some(1980));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped() {
        use std::os::unix::fs::symlink;
        let td = tempfile::tempdir().unwrap();
        let app = make_app(td.path(), "MyApp.app");
        // A symlink inside the bundle (real .app bundles carry CurrentVersion
        // symlinks) must be skipped, not stored or followed.
        symlink("MyApp", app.join("Current")).unwrap();
        let ipa = td.path().join("MyApp.ipa");
        package_app_as_ipa(&app, &ipa).unwrap();
        let mut zip = ZipArchive::new(std::fs::File::open(&ipa).unwrap()).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n.ends_with("/Current")));
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_preserved_on_main_binary() {
        use std::os::unix::fs::PermissionsExt;
        let td = tempfile::tempdir().unwrap();
        let app = make_app(td.path(), "MyApp.app");
        // Mark the main binary executable.
        let bin = app.join("MyApp");
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let ipa = td.path().join("MyApp.ipa");
        package_app_as_ipa(&app, &ipa).unwrap();
        let mut zip = ZipArchive::new(std::fs::File::open(&ipa).unwrap()).unwrap();
        let bin_entry = zip.by_name("Payload/MyApp.app/MyApp").unwrap();
        let mode = bin_entry.unix_mode().expect("unix mode stored");
        assert_ne!(mode & 0o111, 0, "executable bit must survive into the IPA");
    }

    #[test]
    fn missing_app_dir_errors() {
        let td = tempfile::tempdir().unwrap();
        let missing = td.path().join("Nope.app");
        let ipa = td.path().join("out.ipa");
        assert!(package_app_as_ipa(&missing, &ipa).is_err());
    }

    // ── UUID selection ──────────────────────────────────────────────

    fn u(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[test]
    fn uuid_prefers_arm64_over_x86_64() {
        let arm = u(0xAA);
        let slices = vec![("x86_64".to_string(), u(0x11)), ("arm64".to_string(), arm)];
        assert_eq!(
            select_preferred_uuid(&slices),
            Some(arm.simple().to_string())
        );
    }

    #[test]
    fn uuid_arm64e_beats_x86_64() {
        let arm64e = u(0xBB);
        let slices = vec![
            ("arm64e".to_string(), arm64e),
            ("x86_64".to_string(), u(0x22)),
        ];
        assert_eq!(
            select_preferred_uuid(&slices),
            Some(arm64e.simple().to_string())
        );
    }

    #[test]
    fn uuid_falls_back_to_first_slice() {
        // No preferred arch present → first slice wins (deterministic).
        let ppc = u(0xCC);
        let slices = vec![("ppc".to_string(), ppc), ("mips".to_string(), u(0x33))];
        assert_eq!(
            select_preferred_uuid(&slices),
            Some(ppc.simple().to_string())
        );
    }

    #[test]
    fn uuid_simple_form_is_32_hex_no_dashes() {
        let slices = vec![("arm64".to_string(), u(0xAB))];
        let got = select_preferred_uuid(&slices).unwrap();
        assert_eq!(got.len(), 32);
        assert!(got
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert!(!got.contains('-'));
    }

    #[test]
    fn uuid_empty_and_nil_are_none() {
        assert_eq!(select_preferred_uuid(&[]), None);
        // A nil (all-zero) id is meaningless — treated as absent.
        assert_eq!(
            select_preferred_uuid(&[("arm64".to_string(), Uuid::nil())]),
            None
        );
    }

    #[test]
    fn main_executable_uuid_none_when_binary_not_macho() {
        // The fake "MyApp" binary is not a real Mach-O → parse fails → None,
        // no panic.
        let td = tempfile::tempdir().unwrap();
        let app = make_app(td.path(), "MyApp.app");
        assert_eq!(main_executable_uuid(&app), None);
    }

    #[test]
    fn main_executable_uuid_none_when_executable_key_missing() {
        let td = tempfile::tempdir().unwrap();
        let app = td.path().join("Bare.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict></dict></plist>"#,
        )
        .unwrap();
        assert_eq!(main_executable_uuid(&app), None);
    }
}
