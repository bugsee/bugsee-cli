//! Integration tests for `bugsee-cli upload build-info`.
//!
//! These run the COMPILED binary (via assert_cmd) and pin the exit-code
//! contract integrators depend on, plus one end-to-end happy path against an
//! in-process wiremock server. Fine-grained wire-shape assertions (ZIP entry
//! names, zstd method, registration body) live in the `build_info` module's
//! unit tests; here we prove the binary surface behaves.

use std::io::Read;
use std::path::Path;

use assert_cmd::Command;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::ZipArchive;

fn cli() -> Command {
    let mut c = Command::cargo_bin("bugsee-cli").expect("compiled bugsee-cli binary");
    // Don't let a developer's shell env leak the endpoint / token into tests.
    c.env_remove("BUGSEE_ENDPOINT")
        .env_remove("BUGSEE_APP_TOKEN");
    c
}

fn write(dir: &Path, name: &str, content: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, content).unwrap();
    p.to_string_lossy().into_owned()
}

#[test]
fn dry_run_writes_bundle_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = write(tmp.path(), "dependencies.json", r#"{"deps":["a"]}"#);
    let timings = write(tmp.path(), "timings.json", r#"{"total_ms":7}"#);
    let out = tmp.path().join("bundle.zip");
    let out_s = out.to_string_lossy().into_owned();

    cli()
        .args([
            "upload",
            "build-info",
            "--deps",
            &deps,
            "--timings",
            &timings,
            "--dry-run",
            "--out",
            &out_s,
        ])
        .assert()
        .success();

    // The written bundle contains exactly the two sidecars.
    let mut archive = ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
    let names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    assert_eq!(names, vec!["dependencies.json", "timings.json"]);
    let mut got = String::new();
    archive
        .by_name("timings.json")
        .unwrap()
        .read_to_string(&mut got)
        .unwrap();
    assert_eq!(got, r#"{"total_ms":7}"#);
}

#[test]
fn nothing_to_upload_is_config_error_exit_20() {
    cli()
        .args(["upload", "build-info"])
        .assert()
        .failure()
        .code(20);
}

#[test]
fn missing_source_file_is_input_not_found_exit_10() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("does-not-exist.json");
    cli()
        .args([
            "upload",
            "build-info",
            "--deps",
            &missing.to_string_lossy(),
            "--dry-run",
            "--out",
            &tmp.path().join("o.zip").to_string_lossy(),
        ])
        .assert()
        .failure()
        .code(10);
}

#[test]
fn out_without_dry_run_is_config_error_exit_20() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = write(tmp.path(), "dependencies.json", "{}");
    cli()
        .args([
            "upload",
            "build-info",
            "--deps",
            &deps,
            "--out",
            &tmp.path().join("o.zip").to_string_lossy(),
        ])
        .assert()
        .failure()
        .code(20);
}

#[test]
fn below_floor_zstd_level_is_config_error_exit_20() {
    let tmp = tempfile::tempdir().unwrap();
    let deps = write(tmp.path(), "dependencies.json", "{}");
    cli()
        .args([
            "upload",
            "build-info",
            "--deps",
            &deps,
            "--zstd-level",
            "3",
            "--dry-run",
            "--out",
            &tmp.path().join("o.zip").to_string_lossy(),
        ])
        .assert()
        .failure()
        .code(20);
}

#[test]
fn malformed_sidecar_spec_is_config_error_exit_20() {
    let tmp = tempfile::tempdir().unwrap();
    cli()
        .args([
            "upload",
            "build-info",
            "--sidecar",
            "no-equals-sign",
            "--dry-run",
        ])
        .assert()
        .failure()
        .code(20);
    let _ = tmp; // keep tempdir alive for symmetry; nothing written
}

#[test]
fn duplicate_bundle_entry_name_is_config_error_exit_20() {
    // --deps packs an implicit `dependencies.json` entry; a --sidecar that
    // reuses that name collides. The worker keys per-asset processing on the
    // entry name, so the CLI must reject the collision (exit 20) rather than
    // silently drop one sidecar. --dry-run proves the guard fires before any
    // network I/O.
    let tmp = tempfile::tempdir().unwrap();
    let deps = write(tmp.path(), "dependencies.json", r#"{"deps":["a"]}"#);
    let dup = write(tmp.path(), "other.json", r#"{"deps":["b"]}"#);
    let sidecar = format!("dependencies.json={dup}");
    cli()
        .args([
            "upload",
            "build-info",
            "--deps",
            &deps,
            "--sidecar",
            &sidecar,
            "--dry-run",
            "--out",
            &tmp.path().join("o.zip").to_string_lossy(),
        ])
        .assert()
        .failure()
        .code(20);
}

#[test]
fn help_lists_build_info_subcommand() {
    cli()
        .args(["upload", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("build-info"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_happy_path_against_mock_server() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);
    let payload = write(
        tmp.path(),
        "payload.json",
        r#"{"version":"1.0","build":"1"}"#,
    );

    let put_url = format!("{}/bundle-put", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/v2/apps/TKN/builds"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": { "build_info_upload_endpoint": put_url }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(wm_path("/bundle-put"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    // assert_cmd is blocking; run it off the async runtime so the mock server
    // keeps serving while the subprocess talks to it.
    let result = tokio::task::spawn_blocking(move || {
        cli()
            .args([
                "--endpoint",
                &endpoint,
                "--app-token",
                "TKN",
                "upload",
                "build-info",
                "--payload-json",
                &payload,
                "--deps",
                &deps,
            ])
            .assert()
            .success();
    })
    .await;
    result.unwrap();
    // server drops here → wiremock verifies the .expect(1) counts were met.
}
