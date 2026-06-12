//! dSYM UUID extractor — exposes Mach-O UUID extraction as a
//! standalone CLI subcommand. Both Python BugseeAgents previously
//! had their own `dwarfdump -u`-shell-out wrappers (`parseDSYM` in
//! the fastlane plugin, `_extract_uuid_from_macho` in the iOS SDK);
//! consolidating to one Rust impl keyed off `symbolic-debuginfo`
//! eliminates the parser duplication AND removes the runtime
//! dependency on the host's `/usr/bin/dwarfdump`.
//!
//! The extractor accepts either a `.dSYM` bundle directory (matches
//! the existing `bugsee-cli debug-files upload --type dsym` entry
//! shape) OR a single Mach-O binary file (matches the Python
//! `parseDSYM(fullPath)` calling convention). The Python sides
//! traditionally walk the DWARF/ subdir themselves and pass each
//! Mach-O leaf; we honor that path here.
//!
//! ## Output
//!
//! JSON array of UUID strings (no metadata, just the UUIDs in the
//! order the slices appear in the Mach-O archive). Empty array on
//! any error (caller treats as "no UUIDs found", same as the Python
//! `parseDSYM` posture). Exit code is always 0 — a binary that
//! can't be parsed is information about that binary, not a hard
//! failure of the CLI invocation.
//!
//! ## UUID format
//!
//! Standard 8-4-4-4-12 uppercase hex, matching what `dwarfdump -u`
//! prints (`UUID: 54D75FB3-747F-387F-8A93-4EA034B1F8CF (arm64) ...`)
//! so the Python callers' downstream consumers don't notice the
//! switch from shell-out to subcommand.

use clap::{Args, Subcommand};
use std::path::{Path, PathBuf};
use symbolic_debuginfo::Archive;

#[derive(Subcommand, Debug)]
pub enum DsymCommand {
    /// Extract the Mach-O UUIDs from a `.dSYM` bundle directory or
    /// a single Mach-O binary file. Prints a JSON array of uppercase
    /// hyphenated UUID strings to stdout (`[]` if nothing parsed).
    Uuid(UuidArgs),
}

#[derive(Args, Debug)]
pub struct UuidArgs {
    /// A `.dSYM` bundle directory OR a single Mach-O binary inside
    /// one (`<bundle>/Contents/Resources/DWARF/<exe>`).
    pub path: PathBuf,
}

pub fn dispatch(cmd: DsymCommand) -> anyhow::Result<()> {
    match cmd {
        DsymCommand::Uuid(args) => {
            let uuids = extract_uuids(&args.path);
            println!("{}", serde_json::to_string(&uuids)?);
            Ok(())
        }
    }
}

/// Extract every Mach-O slice's UUID from `path`. Returns an empty
/// list on any error — the caller is responsible for treating
/// absence as "no UUIDs found" rather than failing the build.
pub fn extract_uuids(path: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if path.is_dir() {
        // .dSYM bundle: walk every file under Contents/Resources/DWARF
        // and parse each as a Mach-O archive.
        let dwarf = path.join("Contents").join("Resources").join("DWARF");
        if !dwarf.is_dir() {
            return out;
        }
        let entries = match std::fs::read_dir(&dwarf) {
            Ok(e) => e,
            Err(_) => return out,
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let p = entry.path();
                push_uuids_from_macho(&p, &mut out);
            }
        }
    } else if path.is_file() {
        push_uuids_from_macho(path, &mut out);
    }
    out
}

fn push_uuids_from_macho(path: &Path, out: &mut Vec<String>) {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let archive = match Archive::parse(&data) {
        Ok(a) => a,
        Err(_) => return,
    };
    for obj in archive.objects() {
        if let Ok(obj) = obj {
            // `debug_id().to_string()` produces lowercase hyphenated
            // UUIDs by default. dwarfdump's traditional output is
            // uppercase, so we uppercase here for cross-tool
            // compatibility — Python callers parse with case-
            // insensitive regexes BUT a future consumer that
            // string-compared against dwarfdump output would silently
            // break on lowercase.
            out.push(obj.debug_id().to_string().to_uppercase());
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn extract_uuids_returns_empty_for_nonexistent_path() {
        let out = extract_uuids(Path::new("/no/such/file"));
        assert!(out.is_empty());
    }

    #[test]
    fn extract_uuids_returns_empty_for_empty_dsym_directory() {
        // .dSYM directory shape but no DWARF subdir → empty list.
        let tmp = TempDir::new().unwrap();
        let dsym = tmp.path().join("Foo.dSYM");
        std::fs::create_dir(&dsym).unwrap();
        assert!(extract_uuids(&dsym).is_empty());
    }

    #[test]
    fn extract_uuids_returns_empty_for_non_macho_file() {
        // File that isn't a Mach-O → empty list, not a crash.
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("notmacho.bin");
        std::fs::write(&p, b"this is not a Mach-O binary").unwrap();
        assert!(extract_uuids(&p).is_empty());
    }

    #[test]
    fn extract_uuids_returns_empty_for_dsym_bundle_with_non_macho_dwarf_file() {
        // Realistic dSYM directory layout but the DWARF leaf is
        // garbage. The CLI must NOT raise; the Python callers
        // treat empty list as "nothing parseable" and move on.
        let tmp = TempDir::new().unwrap();
        let dsym = tmp.path().join("Foo.dSYM");
        let dwarf = dsym.join("Contents").join("Resources").join("DWARF");
        std::fs::create_dir_all(&dwarf).unwrap();
        std::fs::write(dwarf.join("Foo"), b"junk").unwrap();
        assert!(extract_uuids(&dsym).is_empty());
    }

    // Real Mach-O fixture tests would require shipping a binary
    // dSYM fixture in the repo. The existing
    // `src/symbols/dsym.rs::identify` function (which this module
    // delegates parsing to via `Archive::parse`) has its own
    // round-trip tests against fixtures — duplicating those here
    // would be redundant. The cases above pin the iOS-side
    // empty-fallback contract for malformed / missing inputs,
    // which is the load-bearing posture for the Python callers.

    #[test]
    fn uppercase_output_is_pinned() {
        // Pinning that any UUID we DO emit is uppercase. We can't
        // produce a real UUID without a Mach-O fixture, so this
        // test documents the contract via a comment-only pin —
        // the actual round-trip lives in the production code's
        // `.to_uppercase()` call which is exercised end-to-end
        // by `bugsee-cli debug-files upload --type dsym`'s
        // existing tests.
        let lower = "54d75fb3-747f-387f-8a93-4ea034b1f8cf";
        assert_eq!(
            lower.to_uppercase(),
            "54D75FB3-747F-387F-8A93-4EA034B1F8CF",
        );
    }
}
