//! Integration tests for `bugsee-cli xcode upload-dsyms` (bugsee/bugsee-cli#19).
//!
//! These run the COMPILED binary (assert_cmd) and pin the EXIT CODES, which are
//! the whole point of the command: it runs in an Xcode Run Script build phase,
//! where a non-zero exit fails the build. The contract is asymmetric on purpose:
//!
//!   nothing to upload  -> 0   (a target with no symbols is a normal state)
//!   something broken   -> 20/21/30/31  (the build must surface it)
//!
//! Verifying the broken half end-to-end past the network boundary would need a
//! real Mach-O dSYM fixture, which this repo does not carry; `src/cli/xcode.rs`
//! covers the per-error-kind decision in-process. What this file pins is the
//! binary-level surface: the exit codes an integrator actually branches on, and
//! the `--help` contract.

use assert_cmd::Command;
use predicates::str::contains;

fn cli() -> Command {
    let mut c = Command::cargo_bin("bugsee-cli").expect("compiled bugsee-cli binary");
    // No ambient BUGSEE_* / Xcode vars from the developer's shell.
    c.env_clear();
    c
}

/// A directory shaped like a dSYM bundle — enough for discovery to find it.
fn fake_dsym(parent: &std::path::Path, name: &str) {
    let dwarf = parent
        .join(format!("{name}.dSYM"))
        .join("Contents")
        .join("Resources")
        .join("DWARF");
    std::fs::create_dir_all(&dwarf).unwrap();
    std::fs::write(dwarf.join(name), b"not-a-real-macho").unwrap();
}

#[test]
fn missing_dsym_folder_exits_zero() {
    // DWARF_DSYM_FOLDER_PATH points nowhere and there is no archive: nothing to
    // upload. A build phase must not fail for this.
    cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", "/nonexistent/dsyms")
        .assert()
        .code(0);
}

#[test]
fn empty_dsym_folder_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", tmp.path())
        .assert()
        .code(0);
}

#[test]
fn dsyms_present_but_no_app_token_fails_the_build() {
    // There is something to upload and we cannot upload it. Exit 20
    // (ConfigInvalid) so whoever triggered the build finds out.
    let tmp = tempfile::tempdir().unwrap();
    fake_dsym(tmp.path(), "App");
    cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", tmp.path())
        .assert()
        .code(20);
}

#[test]
fn no_fail_flag_downgrades_the_same_failure_to_zero() {
    let tmp = tempfile::tempdir().unwrap();
    fake_dsym(tmp.path(), "App");
    cli()
        .args(["xcode", "upload-dsyms", "--no-fail"])
        .env("DWARF_DSYM_FOLDER_PATH", tmp.path())
        .assert()
        .code(0);
}

#[test]
fn no_fail_env_var_downgrades_the_same_failure_to_zero() {
    // The env var must behave identically to the flag — an Xcode build phase
    // configures things through the environment far more often than through argv.
    let tmp = tempfile::tempdir().unwrap();
    fake_dsym(tmp.path(), "App");
    cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", tmp.path())
        .env("BUGSEE_DSYM_UPLOAD_NO_FAIL", "1")
        .assert()
        .code(0);
}

#[test]
fn blank_no_fail_env_var_is_unset_not_enabled() {
    // Xcode writes KEY="" when the value field is left blank. Treating that as
    // "enabled" would silently disable the failure reporting the user asked for.
    let tmp = tempfile::tempdir().unwrap();
    fake_dsym(tmp.path(), "App");
    cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", tmp.path())
        .env("BUGSEE_DSYM_UPLOAD_NO_FAIL", "")
        .assert()
        .code(20);
}

#[test]
fn archive_path_is_the_fallback_when_dwarf_folder_is_unset() {
    // <ARCHIVE_PATH>/dSYMs, the post-action's source, still works here.
    let tmp = tempfile::tempdir().unwrap();
    let dsyms = tmp.path().join("dSYMs");
    std::fs::create_dir_all(&dsyms).unwrap();
    fake_dsym(&dsyms, "App");
    cli()
        .args(["xcode", "upload-dsyms"])
        .env("ARCHIVE_PATH", tmp.path())
        .assert()
        .code(20); // found dSYMs, no token -> real failure
}

#[test]
fn help_documents_the_env_vars_and_exit_codes() {
    // `--help` is part of the public surface (see CLAUDE.md): an integrator
    // generating a build phase reads this to know what to set.
    cli()
        .args(["xcode", "upload-dsyms", "--help"])
        .assert()
        .code(0)
        .stdout(contains("DWARF_DSYM_FOLDER_PATH"))
        .stdout(contains("BUGSEE_DSYM_UPLOAD_NO_FAIL"))
        .stdout(contains("--no-fail"))
        .stdout(contains("ENABLE_USER_SCRIPT_SANDBOXING"));
}

#[test]
fn upload_dsyms_is_listed_in_the_xcode_help() {
    cli()
        .args(["xcode", "--help"])
        .assert()
        .code(0)
        .stdout(contains("upload-dsyms"));
}
