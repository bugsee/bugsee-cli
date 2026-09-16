//! Integration tests for `bugsee-cli xcode upload-dsyms` (bugsee/bugsee-cli#19).
//!
//! These run the COMPILED binary (assert_cmd) and pin the EXIT CODES, which are
//! the whole point of the command: it runs in an Xcode Run Script build phase,
//! where a non-zero exit fails the build. The contract is asymmetric on purpose:
//!
//!   nothing to upload  -> 0   (a target with no symbols is a normal state)
//!   something broken   -> 20/21/30/31  (the build must surface it)
//!
//! The network half is covered in-process in `src/cli/xcode.rs`, against a real
//! clang/dsymutil-built `.dSYM` and a wiremock server (401 -> 21, server error
//! -> 30, plus the success path). What THIS file pins is the binary-level
//! surface: the exit codes an integrator branches on, and the `--help` contract.
//!
//! CAUTION when adding `--no-fail` cases here: on unix that flag daemonizes, so
//! `main` double-forks and the parent `exit(0)`s from `daemon::daemonize`
//! BEFORE any upload runs. A bare `.code(0)` assertion therefore passes no
//! matter what the downgrade does — it observes the fork, not the command. The
//! two tests below compensate by reading the daemon's log.

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

/// Wait for `$log` to contain `needle`, up to ~10s. The daemon is detached, so
/// there is no child to wait on — polling the log is the only join point.
#[cfg(unix)]
fn log_eventually_contains(log: &std::path::Path, needle: &str) -> bool {
    for _ in 0..100 {
        if let Ok(body) = std::fs::read_to_string(log) {
            if body.contains(needle) {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// On unix `--no-fail` DAEMONIZES, so exit 0 proves only that the fork happened.
/// What must actually be true is that the detached child ran, hit the failure,
/// and declined to escalate it. That is only observable in the daemon's log.
#[cfg(unix)]
#[test]
fn no_fail_flag_runs_detached_and_swallows_the_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let dsyms = tmp.path().join("dsyms");
    std::fs::create_dir_all(&dsyms).unwrap();
    fake_dsym(&dsyms, "App");
    let logdir = tmp.path().join("log");
    std::fs::create_dir_all(&logdir).unwrap();

    let out = cli()
        .args(["xcode", "upload-dsyms", "--no-fail"])
        .env("DWARF_DSYM_FOLDER_PATH", &dsyms)
        .env("PROJECT_TEMP_DIR", &logdir)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    // Detached: the parent returns immediately with nothing on stderr, because
    // the daemon redirected its own fds into the log.
    assert!(
        out.stderr.is_empty(),
        "expected a detached run (empty parent stderr), got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // ...and the child actually got far enough to hit the token failure and
    // decline to fail the build over it.
    let log = logdir.join("bugsee-cli.log");
    assert!(
        log_eventually_contains(&log, "not failing the build"),
        "daemon log never recorded the downgrade; contents: {:?}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// The env var must behave identically to the flag — an Xcode build phase
/// configures things through the environment far more often than through argv.
#[cfg(unix)]
#[test]
fn no_fail_env_var_behaves_exactly_like_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let dsyms = tmp.path().join("dsyms");
    std::fs::create_dir_all(&dsyms).unwrap();
    fake_dsym(&dsyms, "App");
    let logdir = tmp.path().join("log");
    std::fs::create_dir_all(&logdir).unwrap();

    let out = cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", &dsyms)
        .env("BUGSEE_DSYM_UPLOAD_NO_FAIL", "1")
        .env("PROJECT_TEMP_DIR", &logdir)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty(), "expected a detached run");
    let log = logdir.join("bugsee-cli.log");
    assert!(
        log_eventually_contains(&log, "not failing the build"),
        "daemon log never recorded the downgrade; contents: {:?}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
}

/// The env vars must reach the SAME decision as the flags.
///
/// `should_daemonize` runs in `main` and collects the environment itself, so
/// every unit test — which hands the map in directly — is blind to it. This
/// combination detached despite BUGSEE_DSYM_UPLOAD_BACKGROUND=0 because that
/// key was simply not among the ones copied.
#[cfg(unix)]
#[test]
fn background_env_var_is_honoured_exactly_like_the_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let dsyms = tmp.path().join("dsyms");
    std::fs::create_dir_all(&dsyms).unwrap();
    fake_dsym(&dsyms, "App");
    let logdir = tmp.path().join("log");
    std::fs::create_dir_all(&logdir).unwrap();

    let out = cli()
        .args(["xcode", "upload-dsyms"])
        .env("DWARF_DSYM_FOLDER_PATH", &dsyms)
        .env("PROJECT_TEMP_DIR", &logdir)
        .env("BUGSEE_DSYM_UPLOAD_NO_FAIL", "1")
        .env("BUGSEE_DSYM_UPLOAD_BACKGROUND", "0")
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(0), "no-fail must still exit 0");
    // Foreground: the diagnostics come back on OUR stderr, and no daemon log
    // is written. A detached run is the exact opposite of both.
    assert!(
        !out.stderr.is_empty(),
        "expected a foreground run to report on stderr; it detached instead"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        !logdir.join("bugsee-cli.log").exists(),
        "BUGSEE_DSYM_UPLOAD_BACKGROUND=0 must not detach"
    );
}

/// On non-unix there is no fork, so `--no-fail` runs synchronously and the
/// exit code is the command's own.
#[cfg(not(unix))]
#[test]
fn no_fail_runs_in_the_foreground_and_still_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    fake_dsym(tmp.path(), "App");
    cli()
        .args(["xcode", "upload-dsyms", "--no-fail"])
        .env("DWARF_DSYM_FOLDER_PATH", tmp.path())
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
        .stdout(contains("--background"))
        .stdout(contains("--no-background"))
        .stdout(contains("BUGSEE_DSYM_UPLOAD_BACKGROUND"))
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
