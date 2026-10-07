//! Apple Mach-O dSYM bundle identification and packaging.
//!
//! A dSYM is a directory bundle:
//!
//! ```text
//! Foo.dSYM/
//!     Contents/
//!         Info.plist
//!         Resources/
//!             DWARF/
//!                 Foo       <-- Mach-O (often a FAT archive of multiple arches)
//! ```
//!
//! On the wire (see `BugseeAgent`):
//!   - The metadata POST body has ONLY `version` + `build`. No `uuid`, no
//!     `hash`. The server extracts Mach-O UUIDs from the uploaded zip itself
//!     via `symbolic.debuginfo.Archive.iter_objects()` and stores one entry
//!     per architecture slice in the symbol document's `images` array.
//!     Crash symbolication then matches the runtime's Mach-O UUID against
//!     `images[].uuid`.
//!   - The zip contains the full dSYM bundle tree with paths relative to
//!     the dSYM's parent directory (so `Foo.dSYM/Contents/Resources/DWARF/Foo`
//!     lands at that exact path inside the archive).
//!
//! Phase 1 scope: a single `.dSYM` bundle input. Multi-bundle directory
//! walk (one zip per build, common for projects with frameworks) is a
//! follow-up.

use std::path::{Path, PathBuf};

use symbolic_debuginfo::Archive;

use crate::error::{Error, Result};

/// Per-architecture slice of a fat Mach-O inside a dSYM bundle.
#[derive(Debug, Clone)]
pub struct DsymSlice {
    /// Stringified Mach-O LC_UUID (canonical lowercase, with dashes).
    /// Matches the value the worker stores in `images[].uuid`.
    pub uuid: String,
    /// Architecture name as `symbolic-debuginfo` reports it (`arm64`,
    /// `arm64e`, `x86_64`, ...). Stored in `images[].arch`.
    pub arch: String,
}

/// Result of parsing a single `.dSYM` bundle: one entry per Mach-O slice.
#[derive(Debug, Clone)]
pub struct DsymIdentity {
    pub slices: Vec<DsymSlice>,
}

/// Verify `dsym_path` looks like a `.dSYM` bundle and extract the UUIDs of
/// every Mach-O slice inside. Reads each binary fully into memory — dSYMs
/// can be hundreds of MB for large iOS apps, but Mach-O parsing requires
/// random access so streaming isn't viable today.
pub fn identify(dsym_path: &Path) -> Result<DsymIdentity> {
    if !dsym_path.is_dir() {
        return Err(Error::InputInvalid(format!(
            "expected a .dSYM bundle directory, got {}",
            dsym_path.display()
        )));
    }
    // No name check here: which names count as a bundle is discovery's policy
    // (`.dSYM`, plus any `--extension` suffix). The DWARF directory below is
    // the structural test every bundle must pass whatever it is called.

    let dwarf_dir = dsym_path.join("Contents").join("Resources").join("DWARF");
    if !dwarf_dir.is_dir() {
        return Err(Error::InputInvalid(format!(
            "dSYM bundle is missing Contents/Resources/DWARF: {}",
            dsym_path.display()
        )));
    }

    let mut slices = Vec::new();
    let mut saw_nil_uuid = false;
    for entry in std::fs::read_dir(&dwarf_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let bin_path = entry.path();
        let data = super::mapped::map_file(&bin_path)?;
        let archive = Archive::parse(&data).map_err(|e| {
            Error::InputInvalid(format!(
                "failed to parse Mach-O at {}: {}",
                bin_path.display(),
                e,
            ))
        })?;
        for obj in archive.objects() {
            let obj = obj.map_err(|e| {
                Error::InputInvalid(format!(
                    "failed to read object in {}: {}",
                    bin_path.display(),
                    e,
                ))
            })?;
            // A slice without an LC_UUID reads as the nil UUID. It can never match a
            // crash report, and every such slice would collide on the same all-zero
            // key on the server, so it is not a symbol: skip it (the same stance
            // `xcode_ipa::select_preferred_uuid` takes for the app binary).
            if obj.debug_id().is_nil() {
                tracing::warn!(
                    path = %bin_path.display(),
                    arch = %obj.arch().name(),
                    "Mach-O slice has no LC_UUID (nil UUID); skipping it"
                );
                saw_nil_uuid = true;
                continue;
            }
            slices.push(DsymSlice {
                uuid: obj.debug_id().to_string(),
                arch: obj.arch().name().to_string(),
            });
        }
    }

    if slices.is_empty() {
        return Err(Error::InputInvalid(if saw_nil_uuid {
            format!(
                "no Mach-O slice in the dSYM bundle carries an LC_UUID: {}",
                dsym_path.display()
            )
        } else {
            format!(
                "dSYM bundle contains no Mach-O slices: {}",
                dsym_path.display()
            )
        }));
    }
    Ok(DsymIdentity { slices })
}

/// Enumerate every file inside the dSYM bundle as (zip_entry_name, source_path).
/// `zip_entry_name` is the path relative to the dSYM's parent directory, using
/// forward-slash separators (zip spec requires forward slashes regardless of host
/// OS). This mirrors what `BugseeAgent` produces for server-side compatibility.
pub fn enumerate_bundle_entries(dsym_path: &Path) -> Result<Vec<(String, PathBuf)>> {
    let parent = dsym_path.parent().ok_or_else(|| {
        Error::InputInvalid(format!(
            "dSYM bundle has no parent directory: {}",
            dsym_path.display()
        ))
    })?;

    let mut entries = Vec::new();
    walk_dir_collect(dsym_path, parent, &mut entries)?;
    Ok(entries)
}

fn walk_dir_collect(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            walk_dir_collect(&path, root, out)?;
        } else if file_type.is_file() {
            let rel = path.strip_prefix(root).map_err(|e| {
                Error::InputInvalid(format!(
                    "path {} is not within dSYM root {}: {}",
                    path.display(),
                    root.display(),
                    e,
                ))
            })?;
            // Force forward slashes for ZIP entry names — zip spec requires this
            // regardless of host OS, and the worker reads paths verbatim.
            let entry_name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((entry_name, path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    // ── broken Mach-O / DWARF inputs ───────────────────────────────

    const ARM64: u32 = 0x0100_000c;
    const X86_64: u32 = 0x0100_0007;
    const GOOD: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01,
    ];

    fn bundle_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dsym = tmp.path().join("App.dSYM");
        let dwarf = dsym.join("Contents/Resources/DWARF");
        fs::create_dir_all(&dwarf).unwrap();
        fs::write(dwarf.join("App"), bytes).unwrap();
        (tmp, dsym)
    }

    fn invalid(err: Error) -> String {
        match err {
            Error::InputInvalid(m) => m,
            other => panic!("expected InputInvalid, got {other:?}"),
        }
    }

    /// A Mach-O with no `LC_UUID` reads as the nil UUID, which can never match a
    /// crash report and would register every such dSYM under the same all-zero key.
    /// It is not a symbol: the bundle is rejected, and says why.
    #[test]
    fn a_macho_without_a_uuid_is_rejected_not_registered_as_nil() {
        let (_t, dsym) = bundle_with(&crate::symbols::test_macho::thin_macho(ARM64, 0, [0; 16]));
        let msg = invalid(identify(&dsym).unwrap_err());
        assert!(msg.contains("LC_UUID"), "{msg}");
    }

    #[test]
    fn a_nil_slice_in_a_fat_binary_is_skipped_and_the_real_ones_kept() {
        let fat = crate::symbols::test_macho::fat_macho(&[(ARM64, 0, GOOD), (X86_64, 3, [0; 16])]);
        let (_t, dsym) = bundle_with(&fat);
        let id = identify(&dsym).unwrap();
        assert_eq!(id.slices.len(), 1, "{:?}", id.slices);
        assert_eq!(id.slices[0].arch, "arm64");
        assert!(!id.slices[0].uuid.starts_with("00000000-0000-0000"));
    }

    /// Whatever prefix of a valid Mach-O survives, `identify` returns an error or the
    /// TRUE identity — never a panic and never a different UUID.
    #[test]
    fn every_truncation_of_a_macho_is_an_error_or_the_true_identity() {
        let valid = crate::symbols::test_macho::thin_macho(ARM64, 0, GOOD);
        let want = identify(&bundle_with(&valid).1).unwrap().slices;
        assert_eq!(want.len(), 1);
        for len in 0..valid.len() {
            let (_t, dsym) = bundle_with(&valid[..len]);
            if let Ok(id) = identify(&dsym) {
                assert_eq!(
                    id.slices.iter().map(|s| &s.uuid).collect::<Vec<_>>(),
                    want.iter().map(|s| &s.uuid).collect::<Vec<_>>(),
                    "truncated to {len} bytes reported a different identity"
                );
            }
        }
    }

    #[test]
    fn corrupted_headers_never_panic_and_are_invalid_or_identified() {
        let valid = crate::symbols::test_macho::fat_macho(&[(ARM64, 0, GOOD), (X86_64, 3, GOOD)]);
        let mut cases: Vec<(String, Vec<u8>)> = vec![
            ("empty".into(), Vec::new()),
            ("macho magic only".into(), vec![0xcf, 0xfa, 0xed, 0xfe]),
            ("fat magic only".into(), vec![0xca, 0xfe, 0xba, 0xbe]),
            (
                "fat claiming 2^32-1 slices".into(),
                [
                    &[0xca, 0xfe, 0xba, 0xbe, 0xff, 0xff, 0xff, 0xff][..],
                    &[0u8; 100],
                ]
                .concat(),
            ),
            (
                "garbage".into(),
                (0..4000u32).map(|i| (i * 131 % 253) as u8).collect(),
            ),
            ("zeroed".into(), vec![0; valid.len()]),
        ];
        // Every header byte replaced by 0x00 / 0xff / 0x7f / 0x80.
        for i in 0..valid.len().min(96) {
            for v in [0x00u8, 0xff, 0x7f, 0x80] {
                let mut b = valid.clone();
                b[i] = v;
                cases.push((format!("byte {i} = {v:#x}"), b));
            }
        }
        for (label, bytes) in cases {
            let (_t, dsym) = bundle_with(&bytes);
            // The assertion is "returns" — a panic fails the test; both outcomes are fine.
            let _ = identify(&dsym)
                .map(|id| id.slices.len())
                .map_err(|e| e.to_string());
            let _ = label;
        }
    }

    #[test]
    fn a_subdirectory_or_empty_file_in_the_dwarf_folder_does_not_hide_a_good_slice() {
        let (_t, dsym) = bundle_with(&crate::symbols::test_macho::thin_macho(ARM64, 0, GOOD));
        let dwarf = dsym.join("Contents/Resources/DWARF");
        fs::create_dir(dwarf.join("nested")).unwrap();
        fs::write(dwarf.join("junk"), b"").unwrap();
        // The empty file is not a Mach-O: that is an error for the bundle, loudly,
        // rather than a silently partial identity.
        assert!(identify(&dsym).is_err());
    }

    #[test]
    fn rejects_plain_directory_without_dwarf_dir() {
        // Any NAME is accepted here (discovery owns the name policy, so a
        // `--extension` bundle reaches this point), but a directory that is
        // not structurally a bundle is still rejected.
        let dir = tempfile::tempdir().unwrap();
        let plain_dir = dir.path().join("not-a-dsym");
        fs::create_dir(&plain_dir).unwrap();
        let err = identify(&plain_dir).unwrap_err();
        match err {
            Error::InputInvalid(msg) => assert!(msg.contains("Contents/Resources/DWARF")),
            other => panic!("expected InputInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Foo.dSYM");
        fs::write(&file, b"x").unwrap();
        match identify(&file).unwrap_err() {
            Error::InputInvalid(msg) => assert!(msg.contains("bundle directory")),
            other => panic!("expected InputInvalid, got {other:?}"),
        }
    }

    #[test]
    fn rejects_dsym_without_dwarf_dir() {
        let dir = tempfile::tempdir().unwrap();
        let dsym = dir.path().join("Foo.dSYM");
        fs::create_dir_all(dsym.join("Contents")).unwrap();
        let err = identify(&dsym).unwrap_err();
        match err {
            Error::InputInvalid(msg) => {
                assert!(msg.contains("Contents/Resources/DWARF"));
            }
            other => panic!("expected InputInvalid, got {other:?}"),
        }
    }

    #[test]
    fn enumerate_emits_forward_slash_paths_relative_to_parent() {
        // Synthetic dSYM with two leaf files; we don't need real Mach-O for this.
        let dir = tempfile::tempdir().unwrap();
        let dsym = dir.path().join("Foo.dSYM");
        let dwarf = dsym.join("Contents").join("Resources").join("DWARF");
        fs::create_dir_all(&dwarf).unwrap();

        fs::File::create(dwarf.join("Foo"))
            .unwrap()
            .write_all(b"mach-o stand-in")
            .unwrap();
        fs::File::create(dsym.join("Contents").join("Info.plist"))
            .unwrap()
            .write_all(b"<plist/>")
            .unwrap();

        let mut entries = enumerate_bundle_entries(&dsym).unwrap();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Foo.dSYM/Contents/Info.plist",
                "Foo.dSYM/Contents/Resources/DWARF/Foo",
            ],
        );
    }
}
