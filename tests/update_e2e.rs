//! End-to-end tests for `bugsee-cli update` — the ACTUAL self-replace path.
//!
//! These run a COPY of the compiled binary (never the dev/test binary itself)
//! against an in-process mock server via `BUGSEE_CLI_UPDATE_BASE_URL`, and
//! assert the copy's bytes are really replaced (or deliberately NOT replaced).
//! This is the one behaviour unit tests can't cover without clobbering the test
//! binary: download → SHA-256 verify → extract → `self_replace` in place.

use std::path::{Path, PathBuf};

use assert_cmd::cargo::cargo_bin;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The host triple this test binary (and therefore the bugsee-cli under test)
/// was built for — must match what the CLI requests.
fn host_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        other => panic!("unsupported test host: {other:?}"),
    }
}

fn current_major() -> u64 {
    env!("CARGO_PKG_VERSION")
        .split('.')
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Build a plain tar (system `tar` auto-detects "no compression" by content, so
/// the `.tar.xz` name is fine) containing `bugsee-cli-<triple>/bugsee-cli` with
/// `payload` as its bytes — the shape `install()` extracts with
/// `--strip-components=1`.
fn build_release_tar(dir: &Path, triple: &str, payload: &[u8]) -> Vec<u8> {
    let staging = dir.join("staging");
    let wrapper = staging.join(format!("bugsee-cli-{triple}"));
    std::fs::create_dir_all(&wrapper).unwrap();
    std::fs::write(wrapper.join("bugsee-cli"), payload).unwrap();
    let tar_path = dir.join("artefact.tar.xz");
    let ok = std::process::Command::new("tar")
        .args([
            "-cf",
            &tar_path.to_string_lossy(),
            "-C",
            &staging.to_string_lossy(),
            &format!("bugsee-cli-{triple}"),
        ])
        .status()
        .unwrap()
        .success();
    assert!(ok, "failed to build test tar");
    std::fs::read(&tar_path).unwrap()
}

/// Copy the compiled bugsee-cli to a throwaway path (so a self-replace can't
/// touch the real test/dev binary) and return it.
fn copy_binary(dir: &Path) -> PathBuf {
    let src = cargo_bin("bugsee-cli");
    let dst = dir.join(if cfg!(windows) {
        "bugsee-cli.exe"
    } else {
        "bugsee-cli"
    });
    std::fs::copy(&src, &dst).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dst
}

/// Mount the per-major `version.txt` pointer returning `latest`.
async fn mount_pointer(server: &MockServer, latest: &str) {
    Mock::given(method("GET"))
        .and(wm_path(format!("/v{}.x/version.txt", current_major())))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{latest}\n")))
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_self_replaces_to_a_newer_same_major_release() {
    let triple = host_triple();
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_binary(tmp.path());
    let before = std::fs::read(&exe).unwrap();

    // A clearly-newer same-major version, and a release tarball whose binary is a
    // sentinel we can detect after the replace.
    let newer = format!("{}.99.0", current_major());
    let sentinel = b"SENTINEL-NEW-BUGSEE-CLI-BINARY-vNEXT".to_vec();
    let tar = build_release_tar(tmp.path(), triple, &sentinel);
    let sha = sha256_hex(&tar);

    let server = MockServer::start().await;
    mount_pointer(&server, &newer).await;
    Mock::given(method("GET"))
        .and(wm_path(format!("/v{newer}/bugsee-cli-{triple}.tar.xz")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tar.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wm_path(format!(
            "/v{newer}/bugsee-cli-{triple}.tar.xz.sha256"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sha}  x")))
        .mount(&server)
        .await;

    let base = server.uri();
    let exe_for_run = exe.clone();
    tokio::task::spawn_blocking(move || {
        assert_cmd::Command::new(&exe_for_run)
            .env("BUGSEE_CLI_UPDATE_BASE_URL", &base)
            .arg("update")
            .assert()
            .success();
    })
    .await
    .unwrap();

    // The running binary really replaced itself in place with the new bytes.
    let after = std::fs::read(&exe).unwrap();
    assert_ne!(after, before, "binary should have been replaced");
    assert_eq!(
        after, sentinel,
        "binary should now be the downloaded release"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_check_reports_available_without_replacing() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_binary(tmp.path());
    let before = std::fs::read(&exe).unwrap();

    let newer = format!("{}.99.0", current_major());
    let server = MockServer::start().await;
    mount_pointer(&server, &newer).await;
    // No artefact mock: --check must NOT download.

    let base = server.uri();
    let exe_for_run = exe.clone();
    let out = tokio::task::spawn_blocking(move || {
        assert_cmd::Command::new(&exe_for_run)
            .env("BUGSEE_CLI_UPDATE_BASE_URL", &base)
            .args(["update", "--check"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    })
    .await
    .unwrap();

    let stdout = String::from_utf8_lossy(&out);
    assert!(
        stdout.contains("\"action\":\"available\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(&format!("\"target\":\"{newer}\"")),
        "stdout: {stdout}"
    );
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        before,
        "--check must not replace the binary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_refuses_a_cross_major_release() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_binary(tmp.path());
    let before = std::fs::read(&exe).unwrap();

    // A NEWER version in the NEXT major — the same-major cap must refuse it.
    let cross_major = format!("{}.0.0", current_major() + 1);
    let server = MockServer::start().await;
    mount_pointer(&server, &cross_major).await;
    // No artefact mock: a refused update must NOT download.

    let base = server.uri();
    let exe_for_run = exe.clone();
    let out = tokio::task::spawn_blocking(move || {
        assert_cmd::Command::new(&exe_for_run)
            .env("BUGSEE_CLI_UPDATE_BASE_URL", &base)
            .arg("update")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone()
    })
    .await
    .unwrap();

    let stdout = String::from_utf8_lossy(&out);
    assert!(
        stdout.contains("\"action\":\"up-to-date\""),
        "stdout: {stdout}"
    );
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        before,
        "a cross-major release must never replace the binary"
    );
}

// ── Failure modes: the live binary is NEVER corrupted ──────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_download_failure_without_max_age_errors_and_keeps_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_binary(tmp.path());
    let before = std::fs::read(&exe).unwrap();

    // Pointer resolves to a newer version, but the artefact 500s → download fails
    // AFTER the version decision. Without --max-age this is a hard error.
    let newer = format!("{}.99.0", current_major());
    let triple = host_triple();
    let server = MockServer::start().await;
    mount_pointer(&server, &newer).await;
    Mock::given(method("GET"))
        .and(wm_path(format!("/v{newer}/bugsee-cli-{triple}.tar.xz")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let base = server.uri();
    let exe_for_run = exe.clone();
    tokio::task::spawn_blocking(move || {
        assert_cmd::Command::new(&exe_for_run)
            .env("BUGSEE_CLI_UPDATE_BASE_URL", &base)
            .arg("update")
            .assert()
            .failure(); // non-zero: the caller learns the update didn't happen
    })
    .await
    .unwrap();

    // The existing binary is byte-for-byte intact — a failed download never
    // touches it (verify happens before any self-replace).
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_max_age_swallows_download_failure_and_keeps_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = copy_binary(tmp.path());
    let before = std::fs::read(&exe).unwrap();

    // No mocks at all → even the pointer fetch 404s. Under --max-age (the
    // consumer path) this is best-effort: exit 0, binary untouched.
    let server = MockServer::start().await;
    let base = server.uri();
    let exe_for_run = exe.clone();
    let out = tokio::task::spawn_blocking(move || {
        assert_cmd::Command::new(&exe_for_run)
            .env("BUGSEE_CLI_UPDATE_BASE_URL", &base)
            .args(["update", "--max-age", "12h"])
            .assert()
            .success() // best-effort: never fails the build
            .get_output()
            .stdout
            .clone()
    })
    .await
    .unwrap();

    assert!(
        String::from_utf8_lossy(&out).contains("\"action\":\"skipped\""),
        "stdout: {}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(std::fs::read(&exe).unwrap(), before);
}

/// A read-only install directory: `self_replace` cannot stage the new binary, so
/// the update must fail WITHOUT corrupting the existing binary.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_readonly_dir_fails_safe_and_keeps_binary() {
    use std::os::unix::fs::PermissionsExt;

    let triple = host_triple();
    let tmp = tempfile::tempdir().unwrap();
    // The binary lives in its own subdir we can flip read-only (leaving the
    // tempdir root writable so cleanup still works).
    let bindir = tmp.path().join("ro");
    std::fs::create_dir(&bindir).unwrap();
    let exe = copy_binary(&bindir);
    let before = std::fs::read(&exe).unwrap();

    // Serve a perfectly valid newer release — the ONLY thing that fails is the
    // in-place replace, because the directory is not writable.
    let newer = format!("{}.99.0", current_major());
    let sentinel = b"SENTINEL-SHOULD-NEVER-LAND".to_vec();
    let tar = build_release_tar(tmp.path(), triple, &sentinel);
    let sha = sha256_hex(&tar);
    let server = MockServer::start().await;
    mount_pointer(&server, &newer).await;
    Mock::given(method("GET"))
        .and(wm_path(format!("/v{newer}/bugsee-cli-{triple}.tar.xz")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tar))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(wm_path(format!(
            "/v{newer}/bugsee-cli-{triple}.tar.xz.sha256"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sha}  x")))
        .mount(&server)
        .await;

    // Read+execute, NO write → self_replace's staging file can't be created.
    std::fs::set_permissions(&bindir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let base = server.uri();
    let exe_for_run = exe.clone();
    let bindir_for_run = bindir.clone();
    tokio::task::spawn_blocking(move || {
        let assert = assert_cmd::Command::new(&exe_for_run)
            .env("BUGSEE_CLI_UPDATE_BASE_URL", &base)
            .arg("update")
            .assert()
            .failure();
        // The actionable message names the replace problem.
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        // Restore write perms so the tempdir can be cleaned up regardless.
        let _ = std::fs::set_permissions(&bindir_for_run, std::fs::Permissions::from_mode(0o755));
        assert!(
            stderr.contains("could not replace") || stderr.contains("replace the running binary"),
            "stderr should explain the replace failure: {stderr}"
        );
    })
    .await
    .unwrap();

    // The original binary survived intact — a failed replace never corrupts it.
    assert_eq!(
        std::fs::read(&exe).unwrap(),
        before,
        "a failed self-replace must leave the existing binary untouched"
    );
}
