//! Integration tests for `bugsee-cli xcode post-action`'s CLI override flags.
//!
//! These run the COMPILED binary (via assert_cmd) and verify, end-to-end, that
//! the new `--enable-*` / `--disable-*` toggle flags and the `--size-check-*`
//! threshold flags actually flow through to the post-action's behaviour — not
//! just that they parse.
//!
//! Strategy: the post-action's gate runs FIRST, and the very next step is the
//! "app token required" hard config error (exit 20). So for the gate-affecting
//! flags we can prove the flag took effect with NO network and NO Xcode build
//! by toggling the exit code between 0 (gated out → skip) and 20 (gate opened →
//! token now required) with the flag as the only difference. Each pair below
//! runs the SAME environment twice, with and without the flag.
//!
//! Coverage boundary: the flags that only change behaviour PAST the network
//! boundary — `--disable-dependencies` / `--disable-timings` (bundle contents),
//! `--enable-size-analysis` / `--enable-chunked-upload` (artefact upload), and
//! the size-check threshold *evaluation* — are verified by the in-process unit
//! tests in `src/cli/xcode.rs` (flag → env overlay → gate/threshold resolver).
//! Exercising THOSE end-to-end would need a wiremock server plus a fabricated
//! `.xcarchive` containing a real `.app` (Info.plist + Mach-O); that heavier
//! fixture is intentionally out of scope here. What this file pins is the
//! binary-level surface: gating, value validation, and the `--help` contract.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::str::contains;
use serde_json::json;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Every flag added to `xcode post-action`. Used to pin the `--help` surface.
const NEW_FLAGS: &[&str] = &[
    "--enable-build-info",
    "--disable-build-info",
    "--enable-all-actions",
    "--disable-all-actions",
    "--enable-all-configurations",
    "--disable-all-configurations",
    "--enable-dependencies",
    "--disable-dependencies",
    "--enable-timings",
    "--disable-timings",
    "--enable-size-analysis",
    "--disable-size-analysis",
    "--enable-chunked-upload",
    "--disable-chunked-upload",
    "--enable-size-check",
    "--disable-size-check",
    "--size-check-warning-pct",
    "--size-check-fail-pct",
    "--size-check-warning-bytes",
    "--size-check-fail-bytes",
];

/// A hermetic invocation. The post-action reads dozens of Xcode/`BUGSEE_*` vars
/// straight from the environment, so a developer's shell (or CI) must not leak
/// any in — each test then sets EXACTLY the vars it needs. The gate-out and
/// token-missing code paths never shell out, so an otherwise-empty env is fine.
fn cli() -> Command {
    let mut c = Command::cargo_bin("bugsee-cli").expect("compiled bugsee-cli binary");
    c.env_clear();
    c
}

#[test]
fn help_lists_every_new_flag() {
    let assert = cli()
        .args(["xcode", "post-action", "--help"])
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    for flag in NEW_FLAGS {
        assert!(
            stdout.contains(flag),
            "post-action --help is missing `{flag}`"
        );
    }
}

// ── Gate-affecting toggles: flag flips the exit code 0 <-> 20 ──────────────

#[test]
fn disable_build_info_gates_out_through_the_binary() {
    let archive = tempfile::tempdir().unwrap();
    let archive_path = archive.path().to_str().unwrap();

    // Release Archive, no app token: the gate OPENS, so the missing token is the
    // hard config error — proving execution got past the gate.
    cli()
        .args(["xcode", "post-action", "--force-foreground"])
        .env("ACTION", "install")
        .env("ARCHIVE_PATH", archive_path)
        .env("CONFIGURATION", "Release")
        .assert()
        .failure()
        .code(20)
        .stderr(contains("app-token"));

    // Identical env + the flag: the gate now SKIPS, so the binary exits 0 and
    // never reaches the token check. The flag is the only difference.
    cli()
        .args([
            "xcode",
            "post-action",
            "--force-foreground",
            "--disable-build-info",
        ])
        .env("ACTION", "install")
        .env("ARCHIVE_PATH", archive_path)
        .env("CONFIGURATION", "Release")
        .assert()
        .success();
}

#[test]
fn enable_all_configurations_admits_a_debug_build_through_the_binary() {
    let archive = tempfile::tempdir().unwrap();
    let archive_path = archive.path().to_str().unwrap();

    // A Debug Archive is Release-only-gated out by default → exit 0.
    cli()
        .args(["xcode", "post-action", "--force-foreground"])
        .env("ACTION", "install")
        .env("ARCHIVE_PATH", archive_path)
        .env("CONFIGURATION", "Debug")
        .assert()
        .success();

    // The flag lifts the Release-only restriction → gate opens → token required.
    cli()
        .args([
            "xcode",
            "post-action",
            "--force-foreground",
            "--enable-all-configurations",
        ])
        .env("ACTION", "install")
        .env("ARCHIVE_PATH", archive_path)
        .env("CONFIGURATION", "Debug")
        .assert()
        .failure()
        .code(20)
        .stderr(contains("app-token"));
}

#[test]
fn enable_all_actions_admits_a_plain_build_through_the_binary() {
    let build_dir = tempfile::tempdir().unwrap();
    let build_dir_path = build_dir.path().to_str().unwrap();

    // A plain Build action (ACTION != install) with no archive is gated out by
    // default — build-info wants an Archive → exit 0.
    cli()
        .args(["xcode", "post-action", "--force-foreground"])
        .env("ACTION", "build")
        .env("CONFIGURATION", "Release")
        .env("TARGET_BUILD_DIR", build_dir_path)
        .assert()
        .success();

    // Opting in admits the plain Build → gate opens → token required.
    cli()
        .args([
            "xcode",
            "post-action",
            "--force-foreground",
            "--enable-all-actions",
        ])
        .env("ACTION", "build")
        .env("CONFIGURATION", "Release")
        .env("TARGET_BUILD_DIR", build_dir_path)
        .assert()
        .failure()
        .code(20)
        .stderr(contains("app-token"));
}

// ── Value validation (stricter than the env path) ─────────────────────────

#[test]
fn size_check_pct_threshold_rejects_non_numeric_value() {
    cli()
        .args(["xcode", "post-action", "--size-check-fail-pct", "notanum"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("invalid value"));
}

#[test]
fn size_check_byte_threshold_rejects_negative_value() {
    // The stringly-typed env path treats `<= 0` as "disable"; on the CLI a
    // hyphen-led numeric value is a hard parse error instead.
    cli()
        .args(["xcode", "post-action", "--size-check-fail-bytes", "-5"])
        .assert()
        .failure()
        .code(2)
        .stderr(contains("unexpected argument"));
}

// ── `overrides_with` last-wins is accepted (no usage error) ───────────────

#[test]
fn conflicting_size_check_pair_is_accepted_not_a_usage_error() {
    // Passing both halves of a pair must NOT be a clap conflict error; the
    // reciprocal `overrides_with` resolves it (last one wins). In this gated-out
    // env (plain Build, no opt-in) the binary simply exits 0.
    cli()
        .args([
            "xcode",
            "post-action",
            "--force-foreground",
            "--enable-size-check",
            "--disable-size-check",
        ])
        .env("ACTION", "build")
        .env("CONFIGURATION", "Release")
        .assert()
        .success();
}

// ── Full networked flow through the COMPILED BINARY, driven by flags ───────
//
// The gate/validation tests above prove the flags reach the gate; these prove a
// flag drives the deep-flow behaviour (artefact ship / size-check fail) through
// a COMPLETE run of the real binary against an in-process mock server — exercise
// of main → dispatch → apply_overrides → network, end to end. (`--force-foreground`
// so the run is synchronous instead of daemonising.) The in-process tests in
// `src/cli/xcode.rs` cover deps/timings opt-out the same way at the function
// level; here we pin the two flags whose effect is a network request or a
// non-zero exit, which are most valuable to verify through the actual binary.

/// Build a minimal Release `.xcarchive` (with a packageable `.app`) plus empty
/// SRCROOT and dSYMs dirs — the integration-test analogue of the unit tests'
/// `size_check_archive`. No Mach-O is needed: `.ipa` packaging only zips the
/// `.app`, and the main-executable UUID falls back to random when absent.
fn make_release_archive(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let archive = root.join("App.xcarchive");
    let app = archive
        .join("Products")
        .join("Applications")
        .join("MyApp.app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.myapp</string>
  <key>CFBundleShortVersionString</key><string>3.0.0</string>
  <key>CFBundleVersion</key><string>300</string>
</dict></plist>"#,
    )
    .unwrap();
    let srcroot = root.join("src"); // empty → no deps
    std::fs::create_dir_all(&srcroot).unwrap();
    let dsym_dir = root.join("dSYMs"); // empty → dSYM step no-ops
    std::fs::create_dir_all(&dsym_dir).unwrap();
    (archive, srcroot, dsym_dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_size_analysis_flag_ships_artifact_e2e() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let (archive, srcroot, dsym_dir) = make_release_archive(tmp.path());

    let art_url = format!("{}/art", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/v2/apps/TKN/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": { "build_id": "b1", "endpoint": art_url }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1) // exactly one artefact PUT
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let (a, s, d) = (
        archive.to_string_lossy().into_owned(),
        srcroot.to_string_lossy().into_owned(),
        dsym_dir.to_string_lossy().into_owned(),
    );
    // assert_cmd is blocking; run it off the async runtime so the mock keeps
    // serving while the subprocess talks to it.
    let result = tokio::task::spawn_blocking(move || {
        cli()
            .args([
                "--endpoint",
                &endpoint,
                "--app-token",
                "TKN",
                "xcode",
                "post-action",
                "--force-foreground",
                "--enable-size-analysis",
            ])
            .env("ACTION", "install")
            .env("ARCHIVE_PATH", &a)
            .env("CONFIGURATION", "Release")
            .env("SRCROOT", &s)
            .env("DWARF_DSYM_FOLDER_PATH", &d)
            .assert()
            .success();
    })
    .await;
    result.unwrap();
    // server drops → wiremock verifies the POST + artefact PUT .expect(1) counts.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_size_check_fail_flag_exits_40_e2e() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let (archive, srcroot, dsym_dir) = make_release_archive(tmp.path());

    // A 1-byte baseline forces the freshly packaged .ipa over the fail gate.
    Mock::given(method("GET"))
        .and(wm_path("/v2/apps/TKN/builds/baseline"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "result": { "build": { "artifact_size": 1, "version": "2.9", "build": "299" } }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(wm_path("/v2/apps/TKN/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "result": { "build_id": "b1" }
        })))
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let (a, s, d) = (
        archive.to_string_lossy().into_owned(),
        srcroot.to_string_lossy().into_owned(),
        dsym_dir.to_string_lossy().into_owned(),
    );
    // The size-check enable AND the fail threshold both come from FLAGS; the
    // packaged .ipa dwarfs the 1-byte baseline, so the build is failed with the
    // deliberate terminal exit code 40, through the real binary.
    let result = tokio::task::spawn_blocking(move || {
        cli()
            .args([
                "--endpoint",
                &endpoint,
                "--app-token",
                "TKN",
                "xcode",
                "post-action",
                "--force-foreground",
                "--enable-size-check",
                "--size-check-fail-bytes",
                "1",
            ])
            .env("ACTION", "install")
            .env("ARCHIVE_PATH", &a)
            .env("CONFIGURATION", "Release")
            .env("SRCROOT", &s)
            .env("DWARF_DSYM_FOLDER_PATH", &d)
            .assert()
            .failure()
            .code(40);
    })
    .await;
    result.unwrap();
}
