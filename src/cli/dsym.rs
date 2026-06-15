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
//! Two output shapes, depending on the subcommand:
//!
//! - `dsym uuid <path>` — JSON array of UUID strings (no metadata,
//!   just the UUIDs in the order the slices appear in the Mach-O
//!   archive). This is the `parseDSYM(fullPath)` shape — uppercase
//!   hyphenated, one entry per slice.
//! - `dsym slices <path>` — JSON array of
//!   `{"uuid": "...", "arch": "..."}` objects (same UUID shape;
//!   `arch` is `symbolic-debuginfo`'s short name —
//!   `arm64` / `arm64e` / `x86_64` / `arm64-simulator` / …). This
//!   is the `get_main_executable_uuid(app_path)` shape — the iOS
//!   SDK's BugseeAgent needs per-slice arch to pick the preferred
//!   `LC_UUID` on a fat .app binary.
//!
//! Both subcommands return an empty array on any error (caller
//! treats as "no UUIDs found", same as the Python `parseDSYM`
//! posture). Exit code is always 0 — a binary that can't be parsed
//! is information about that binary, not a hard failure of the CLI
//! invocation.
//!
//! ## UUID format
//!
//! Standard 8-4-4-4-12 uppercase hex, matching what `dwarfdump -u`
//! prints (`UUID: 54D75FB3-747F-387F-8A93-4EA034B1F8CF (arm64) ...`)
//! so the Python callers' downstream consumers don't notice the
//! switch from shell-out to subcommand.

use clap::{Args, Subcommand};
use serde::Serialize;
use std::path::{Path, PathBuf};
use symbolic_common::DebugId;
use symbolic_debuginfo::Archive;

#[derive(Subcommand, Debug)]
pub enum DsymCommand {
    /// Extract the Mach-O UUIDs from a `.dSYM` bundle directory or
    /// a single Mach-O binary file. Prints a JSON array of uppercase
    /// hyphenated UUID strings to stdout (`[]` if nothing parsed).
    Uuid(UuidArgs),
    /// Same input as `uuid` but emits an arch-aware view: a JSON array
    /// of `{"uuid": "...", "arch": "..."}` objects, one per Mach-O
    /// slice, preserving the order the slices appear in the archive.
    /// Consumed by `get_main_executable_uuid` in the iOS SDK's
    /// BugseeAgent, which needs the per-slice arch to pick the right
    /// `LC_UUID` on a fat .app binary.
    Slices(SlicesArgs),
}

#[derive(Args, Debug)]
pub struct UuidArgs {
    /// A `.dSYM` bundle directory OR a single Mach-O binary inside
    /// one (`<bundle>/Contents/Resources/DWARF/<exe>`).
    pub path: PathBuf,
}

#[derive(Args, Debug)]
pub struct SlicesArgs {
    /// A `.dSYM` bundle directory OR a single Mach-O binary inside
    /// one (`<bundle>/Contents/Resources/DWARF/<exe>`).
    pub path: PathBuf,
}

/// Arch-aware per-slice view returned by `dsym slices`. `uuid` is
/// uppercase hyphenated (same shape as `dsym uuid`); `arch` is the
/// canonical short name `symbolic-debuginfo` reports
/// (`arm64` / `arm64e` / `x86_64` / `arm64-simulator` / …).
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DsymSliceView {
    pub uuid: String,
    pub arch: String,
}

pub fn dispatch(cmd: DsymCommand) -> anyhow::Result<()> {
    match cmd {
        DsymCommand::Uuid(args) => {
            let uuids = extract_uuids(&args.path);
            println!("{}", serde_json::to_string(&uuids)?);
            Ok(())
        }
        DsymCommand::Slices(args) => {
            let slices = extract_slices(&args.path);
            println!("{}", serde_json::to_string(&slices)?);
            Ok(())
        }
    }
}

/// Extract every Mach-O slice's UUID from `path`. Returns an empty
/// list on any error — the caller is responsible for treating
/// absence as "no UUIDs found" rather than failing the build.
pub fn extract_uuids(path: &Path) -> Vec<String> {
    extract_slices(path).into_iter().map(|s| s.uuid).collect()
}

/// Extract every Mach-O slice's `(uuid, arch)` from `path`. Same
/// fallback posture as `extract_uuids` — an empty list on any error
/// is the contract.
pub fn extract_slices(path: &Path) -> Vec<DsymSliceView> {
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
                push_slices_from_macho(&p, &mut out);
            }
        }
    } else if path.is_file() {
        push_slices_from_macho(path, &mut out);
    }
    out
}

/// Hard cap on the size of a single Mach-O file we will read into
/// memory. Real production dSYMs (large iOS apps + Swift stdlib +
/// extensions) typically sit between 200 MB and 1 GB per slice. The
/// cap is a generous-but-bounded safety net against a corrupt or
/// adversarial input (a "dSYM" that is actually a 10 GB random file
/// would otherwise drive the build host into swap). Skip rather than
/// fail — the empty-fallback contract still applies.
const MAX_MACHO_FILE_BYTES: u64 = 1024 * 1024 * 1024; // 1 GiB

fn push_slices_from_macho(path: &Path, out: &mut Vec<DsymSliceView>) {
    // Stat the file first so a runaway-size input is rejected without
    // allocating gigabytes. `std::fs::read` would otherwise sequentially
    // grow the Vec until the kernel says no.
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() > MAX_MACHO_FILE_BYTES {
            return;
        }
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return,
    };
    let archive = match Archive::parse(&data) {
        Ok(a) => a,
        Err(_) => return,
    };
    for obj in archive.objects().flatten() {
        out.push(DsymSliceView {
            uuid: format_uuid(obj.debug_id()),
            arch: obj.arch().name().to_string(),
        });
    }
}

/// Stringify a `DebugId` in the uppercase hyphenated 8-4-4-4-12 form
/// that `dwarfdump -u` traditionally emits. `DebugId::to_string()`
/// defaults to lowercase; we override here for cross-tool
/// compatibility. Python callers parse with case-insensitive regexes
/// today BUT a future consumer that string-compared against dwarfdump
/// output (the SDK's `BGSCrashReport.m` reports `LC_UUID` lowercase
/// without dashes, so any cross-tool comparison must explicitly
/// normalise) would silently break on lowercase. Extracted to its
/// own function so the uppercase invariant has a behaviour-pinning
/// test rather than the previous tautological pin (which tested
/// `String::to_uppercase` against itself).
pub fn format_uuid(id: DebugId) -> String {
    id.to_string().to_uppercase()
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

    // ─── extract_slices: same empty-fallback posture ──────────────

    #[test]
    fn extract_slices_returns_empty_for_nonexistent_path() {
        assert!(extract_slices(Path::new("/no/such/file")).is_empty());
    }

    #[test]
    fn extract_slices_returns_empty_for_empty_dsym_directory() {
        let tmp = TempDir::new().unwrap();
        let dsym = tmp.path().join("Foo.dSYM");
        std::fs::create_dir(&dsym).unwrap();
        assert!(extract_slices(&dsym).is_empty());
    }

    #[test]
    fn extract_slices_returns_empty_for_non_macho_file() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("notmacho.bin");
        std::fs::write(&p, b"this is not a Mach-O binary").unwrap();
        assert!(extract_slices(&p).is_empty());
    }

    #[test]
    fn extract_slices_returns_empty_for_dsym_bundle_with_non_macho_dwarf_file() {
        let tmp = TempDir::new().unwrap();
        let dsym = tmp.path().join("Foo.dSYM");
        let dwarf = dsym.join("Contents").join("Resources").join("DWARF");
        std::fs::create_dir_all(&dwarf).unwrap();
        std::fs::write(dwarf.join("Foo"), b"junk").unwrap();
        assert!(extract_slices(&dsym).is_empty());
    }

    #[test]
    fn extract_uuids_and_extract_slices_agree_on_uuid_ordering() {
        // Defensive: the two extractors share their walk; an empty
        // input should produce the same empty list on both. Pins the
        // delegation in `extract_uuids` (which is currently just
        // `extract_slices().map(|s| s.uuid)`).
        let tmp = TempDir::new().unwrap();
        let dsym = tmp.path().join("Foo.dSYM");
        let dwarf = dsym.join("Contents").join("Resources").join("DWARF");
        std::fs::create_dir_all(&dwarf).unwrap();
        std::fs::write(dwarf.join("Foo"), b"junk").unwrap();
        let uuids = extract_uuids(&dsym);
        let slices = extract_slices(&dsym);
        assert_eq!(uuids.len(), slices.len());
        for (i, s) in slices.iter().enumerate() {
            assert_eq!(s.uuid, uuids[i]);
        }
    }

    // ─── Real Mach-O round-trip ───────────────────────────────────
    //
    // A synthetic 56-byte Mach-O 64-bit binary with just an LC_UUID
    // load command. Enough for `symbolic-debuginfo`'s `Archive::parse`
    // + `debug_id()` round-trip, so this single fixture pins:
    //   - extract_uuids actually parses Mach-O (not just empty-
    //     fallback paths).
    //   - The uppercase invariant survives the production code path
    //     (a regression that dropped `.to_uppercase()` would surface
    //     here as a lowercase comparison failure).
    //   - format_uuid produces the documented 8-4-4-4-12 shape end-
    //     to-end through `push_slices_from_macho`.
    //
    // Pins the previously-deferred review finding (real Mach-O round-
    // trip absent) without checking in a multi-megabyte dSYM bundle.

    /// Build a minimal valid Mach-O 64-bit binary with a single
    /// LC_UUID load command. UUID layout matches the expected hex
    /// `54D75FB3-747F-387F-8A93-4EA034B1F8CF`.
    fn synthetic_macho_with_uuid() -> Vec<u8> {
        let mut buf = Vec::with_capacity(56);
        // mach_header_64
        buf.extend_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]); // MH_MAGIC_64
        buf.extend_from_slice(&[0x07, 0x00, 0x00, 0x01]); // cputype = x86_64
        buf.extend_from_slice(&[0x03, 0x00, 0x00, 0x00]); // cpusubtype
        buf.extend_from_slice(&[0x0a, 0x00, 0x00, 0x00]); // filetype = MH_DSYM
        buf.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // ncmds = 1
        buf.extend_from_slice(&[0x18, 0x00, 0x00, 0x00]); // sizeofcmds = 24
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // flags
        buf.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // reserved
                                                          // LC_UUID
        buf.extend_from_slice(&[0x1b, 0x00, 0x00, 0x00]); // cmd = LC_UUID
        buf.extend_from_slice(&[0x18, 0x00, 0x00, 0x00]); // cmdsize = 24
        buf.extend_from_slice(&[
            0x54, 0xd7, 0x5f, 0xb3, 0x74, 0x7f, 0x38, 0x7f, 0x8a, 0x93, 0x4e, 0xa0, 0x34, 0xb1,
            0xf8, 0xcf,
        ]);
        buf
    }

    #[test]
    fn extract_uuids_round_trips_synthetic_macho_to_uppercase_hex() {
        // Pins the end-to-end contract: a real (if minimal) Mach-O
        // → extract_uuids returns the LC_UUID in 8-4-4-4-12 uppercase
        // hex form. Any mutation in `push_slices_from_macho` that
        // dropped `.to_uppercase()` (the original tautological pin's
        // failure mode) surfaces here as a case-mismatch.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("synthetic.macho");
        std::fs::write(&path, synthetic_macho_with_uuid()).unwrap();
        let uuids = extract_uuids(&path);
        assert_eq!(uuids, vec!["54D75FB3-747F-387F-8A93-4EA034B1F8CF"]);
    }

    #[test]
    fn extract_slices_round_trips_synthetic_macho_with_arch() {
        // Pair pin for the arch-aware variant. The synthetic Mach-O
        // declares cputype=x86_64; symbolic-debuginfo's arch().name()
        // must surface that as the lowercase short name. A serde
        // rename mutation in DsymSliceView would also fail here.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("synthetic.macho");
        std::fs::write(&path, synthetic_macho_with_uuid()).unwrap();
        let slices = extract_slices(&path);
        assert_eq!(
            slices.len(),
            1,
            "exactly one slice expected, got {:?}",
            slices
        );
        assert_eq!(slices[0].uuid, "54D75FB3-747F-387F-8A93-4EA034B1F8CF");
        assert_eq!(slices[0].arch, "x86_64");
    }

    #[test]
    fn format_uuid_emits_uppercase_hyphenated_8_4_4_4_12() {
        // Behaviour-pinning test for the uppercase invariant the CLI
        // owes its consumers. The previous `uppercase_output_is_pinned`
        // test was tautological — it tested `String::to_uppercase`
        // against itself — and would have passed even if production
        // dropped the `.to_uppercase()` call entirely. This test
        // exercises `format_uuid` (the actual production code path)
        // with a known UUID and pins the shape contract documented
        // at the top of this module: `8-4-4-4-12 uppercase hex`.
        let id: DebugId = "54d75fb3-747f-387f-8a93-4ea034b1f8cf"
            .parse()
            .expect("known-valid debug id");
        assert_eq!(format_uuid(id), "54D75FB3-747F-387F-8A93-4EA034B1F8CF",);
        // Cross-check: explicitly assert the four hyphens are at the
        // canonical positions (8, 13, 18, 23) so a future serde-rename
        // or formatter swap that drops dashes can't slip past.
        let out = format_uuid(id);
        let dash_positions: Vec<usize> = out
            .char_indices()
            .filter(|(_, c)| *c == '-')
            .map(|(i, _)| i)
            .collect();
        assert_eq!(dash_positions, vec![8, 13, 18, 23]);
        assert_eq!(out.len(), 36);
    }

    #[test]
    fn dsym_slice_view_serialises_to_lowercase_field_names() {
        // Cross-language wire-shape pin for `DsymSliceView`. The
        // Python consumer at
        // `ios/sdk/scripts/test_bugsee_agent_dsym_cli.py` looks up
        // `entry.get("uuid")` and `entry.get("arch")` — those exact
        // lowercase field names. An accidental serde rename
        // (`#[serde(rename_all = "PascalCase")]`) would silently
        // produce `{"Uuid":...,"Arch":...}` and the Python parser's
        // `_load_macho_slices_via_cli` would emit an empty dict per
        // entry, so dSYM symbolication would silently degrade. The
        // existing sibling test (`json_round_trips_through_serde...`
        // in vcs_metadata.rs) pins field names that way; this is the
        // dsym-side equivalent.
        let view = DsymSliceView {
            uuid: "54D75FB3-747F-387F-8A93-4EA034B1F8CF".to_string(),
            arch: "arm64".to_string(),
        };
        let json = serde_json::to_string(&view).unwrap();
        // Exact lowercase keys.
        assert!(
            json.contains("\"uuid\":"),
            "missing lowercase uuid key in {json}"
        );
        assert!(
            json.contains("\"arch\":"),
            "missing lowercase arch key in {json}"
        );
        // Negative pin — common accidental renames.
        assert!(
            !json.contains("\"UUID\":"),
            "UUID (uppercase) leaked in {json}"
        );
        assert!(
            !json.contains("\"Uuid\":"),
            "Uuid (PascalCase) leaked in {json}"
        );
        assert!(
            !json.contains("\"Arch\":"),
            "Arch (PascalCase) leaked in {json}"
        );
        // Value round-trip — the field values come through as strings.
        assert!(json.contains("\"54D75FB3-747F-387F-8A93-4EA034B1F8CF\""));
        assert!(json.contains("\"arm64\""));
    }

    #[test]
    fn dsym_slice_view_vec_serialises_to_array_of_objects() {
        // Pair pin for the top-level emit. The CLI prints
        // `serde_json::to_string(&extract_slices(&path))` — a JSON
        // array. Python parses with `json.loads(out)` and checks
        // `isinstance(data, list)`, so an accidental serde-flatten
        // that emitted `{"slices":[...]}` would silently produce a
        // dict the Python parser rejects.
        let views = vec![
            DsymSliceView {
                uuid: "AA000000-0000-0000-0000-000000000001".to_string(),
                arch: "arm64".to_string(),
            },
            DsymSliceView {
                uuid: "BB000000-0000-0000-0000-000000000002".to_string(),
                arch: "x86_64".to_string(),
            },
        ];
        let json = serde_json::to_string(&views).unwrap();
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        // Two slice objects, comma-separated.
        assert!(
            json.contains("},{"),
            "expected 2-object array shape in {json}"
        );
    }
}
