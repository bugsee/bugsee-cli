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
    if !ends_with_dsym(dsym_path) {
        return Err(Error::InputInvalid(format!(
            "path does not end in `.dSYM`: {}",
            dsym_path.display()
        )));
    }

    let dwarf_dir = dsym_path.join("Contents").join("Resources").join("DWARF");
    if !dwarf_dir.is_dir() {
        return Err(Error::InputInvalid(format!(
            "dSYM bundle is missing Contents/Resources/DWARF: {}",
            dsym_path.display()
        )));
    }

    let mut slices = Vec::new();
    for entry in std::fs::read_dir(&dwarf_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let bin_path = entry.path();
        let data = std::fs::read(&bin_path)?;
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
            slices.push(DsymSlice {
                uuid: obj.debug_id().to_string(),
                arch: obj.arch().name().to_string(),
            });
        }
    }

    if slices.is_empty() {
        return Err(Error::InputInvalid(format!(
            "dSYM bundle contains no Mach-O slices: {}",
            dsym_path.display()
        )));
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

fn ends_with_dsym(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.ends_with(".dSYM"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn rejects_non_dsym_paths() {
        let dir = tempfile::tempdir().unwrap();
        let plain_dir = dir.path().join("not-a-dsym");
        fs::create_dir(&plain_dir).unwrap();
        let err = identify(&plain_dir).unwrap_err();
        match err {
            Error::InputInvalid(msg) => assert!(msg.contains(".dSYM")),
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
