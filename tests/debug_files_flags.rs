//! Flag-combination contract for `debug-files upload`, through the COMPILED
//! binary.
//!
//! `--uuid` overrides the symbol identity. For the types that read their
//! identity out of the file itself, an override could never match what the
//! crashing module reports, so it must be REJECTED rather than silently
//! ignored — a silent ignore looks like a successful upload while producing
//! symbols nothing can resolve.
//!
//! The rejection is a caller-configuration error: exit 20 (`ConfigInvalid`),
//! which integrators are documented not to fall back on.

use assert_cmd::Command;
use predicates::str::contains;
use std::path::PathBuf;

const CONFIG_INVALID: i32 = 20;

fn upload(kind: &str, extra: &[&str]) -> Command {
    let mut c = Command::cargo_bin("bugsee-cli").unwrap();
    c.env_clear().args([
        "--app-token",
        "TKN",
        "debug-files",
        "upload",
        "--type",
        kind,
        "--version",
        "1.0",
        "--build",
        "1",
        "--dry-run",
    ]);
    c.args(extra);
    c
}

#[test]
fn uuid_override_is_rejected_for_pdb() {
    // The identity is the PDB's own debug id (GUID + age).
    upload(
        "pdb",
        &["--uuid", "11111111-2222-3333-4444-555555555555", "."],
    )
    .assert()
    .code(CONFIG_INVALID)
    .stderr(contains("--uuid does not apply to --type pdb"));
}

#[test]
fn uuid_override_is_rejected_for_dsym() {
    upload(
        "dsym",
        &["--uuid", "11111111-2222-3333-4444-555555555555", "."],
    )
    .assert()
    .code(CONFIG_INVALID)
    .stderr(contains("--uuid does not apply to --type dsym"));
}

/// ELF is the inverse: the archive carries no identity of its own, so the
/// caller-supplied UUID is REQUIRED — same exit code, opposite direction.
#[test]
fn uuid_override_is_required_for_elf() {
    upload("elf", &["."])
        .assert()
        .code(CONFIG_INVALID)
        .stderr(contains("--uuid is required when --type elf"));
}

/// `--type rust` resolves to one of dSYM / PDB / build-id ELF, and every one of
/// those carries its own identity — so an override is rejected here too, for
/// the same reason and with the same exit code.
#[test]
fn uuid_override_is_rejected_for_rust() {
    upload(
        "rust",
        &["--uuid", "11111111-2222-3333-4444-555555555555", "."],
    )
    .assert()
    .code(CONFIG_INVALID)
    .stderr(contains("--uuid does not apply to --type rust"));
}

/// Multi-bundle discovery with one UUID list must refuse (exit 11) rather
/// than stamp the same identity onto android+ios maps.
#[test]
fn il2cpp_linemap_refuses_multiple_bundles_under_one_uuid_list() {
    const INPUT_INVALID: i32 = 11;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/il2cpp-linemap");

    upload(
        "il2cpp-linemap",
        &["--uuid", "android-only-id", root.to_str().unwrap()],
    )
    .assert()
    .code(INPUT_INVALID)
    .stderr(contains("found 2 LineNumberMappings.json bundles"))
    .stderr(contains("refuse to apply one UUID list"));
}

/// A single platform fixture still dry-runs successfully.
#[test]
fn il2cpp_linemap_dry_run_accepts_one_bundle() {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/il2cpp-linemap/android");

    upload(
        "il2cpp-linemap",
        &["--uuid", "deadbeefcafebabe", root.to_str().unwrap()],
    )
    .assert()
    .success()
    .stderr(contains("dry-run: not uploading"));
}

/// Missing `--uuid` for il2cpp-linemap is configuration (exit 20), same as elf.
#[test]
fn il2cpp_linemap_missing_uuid_is_config_invalid() {
    let root =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/il2cpp-linemap/android");

    upload("il2cpp-linemap", &[root.to_str().unwrap()])
        .assert()
        .code(CONFIG_INVALID)
        .stderr(contains("--uuid is required"));
}

/// The ergonomic contract: pointing `--type rust` at a directory with no debug
/// symbols must hand back the Cargo settings that produce them, not a bare
/// "nothing found". Exit 10 (`InputNotFound`) is substantive — the caller is
/// documented not to fall back on it.
#[test]
fn rust_with_no_symbols_reports_the_build_settings_to_fix() {
    const INPUT_NOT_FOUND: i32 = 10;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("app.d"), b"app: src/main.rs").unwrap();

    upload("rust", &[tmp.path().to_str().unwrap()])
        .assert()
        .code(INPUT_NOT_FOUND)
        .stderr(contains("debug = 1"))
        .stderr(contains("split-debuginfo"))
        .stderr(contains("--build-id"));
}
