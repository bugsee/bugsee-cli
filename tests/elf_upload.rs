//! End-to-end test of `debug-files upload --type elf` through the COMPILED
//! binary against an in-process mock server.
//!
//! Proves the core of the per-`.so` native-symbol model: each library is
//! registered by its OWN GNU build-id (`code_id`) — NOT the build-level
//! `--uuid` (the ProGuard mapping's identity) — with `transform = "breakpad"`,
//! and its bytes are PUT to the signed URL. The fixture is a real aarch64 ELF
//! whose build-id is `bca64abfec40dbb631bb8f1c37414472`; the same `symbolic`
//! crate family the worker uses must extract exactly that value.

use std::io::Write;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// `file(1)` on the fixture: `BuildID[md5/uuid]=bca64abfec40dbb631bb8f1c37414472`.
const FIXTURE_BUILD_ID: &str = "bca64abfec40dbb631bb8f1c37414472";

fn pack_native_zip(dir: &Path) -> PathBuf {
    let elf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let bytes = std::fs::read(&elf).expect("ELF fixture present");
    let zip_path = dir.join("native-debug-symbols.zip");
    let f = std::fs::File::create(&zip_path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    zw.start_file(
        "arm64-v8a/libsymbol1.so",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zw.write_all(&bytes).unwrap();
    zw.finish().unwrap();
    zip_path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_registers_each_so_by_its_real_build_id() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = pack_native_zip(tmp.path());

    let put_url = format!("{}/put/sym", server.uri());
    // The metadata POST MUST carry the .so's real build-id (not the --uuid) and
    // transform=breakpad. If those aren't in the body, this mock doesn't match
    // and the `.expect(1)` verification fails on server drop.
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(FIXTURE_BUILD_ID))
        .and(body_string_contains("breakpad"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1) // exactly one .so payload PUT
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let zip = zip_path.to_string_lossy().into_owned();
    // assert_cmd is blocking; keep it off the async runtime so the mock serves.
    tokio::task::spawn_blocking(move || {
        let mut c = Command::cargo_bin("bugsee-cli").unwrap();
        c.env_clear()
            .args([
                "--endpoint",
                &endpoint,
                "--app-token",
                "TKN",
                "debug-files",
                "upload",
                "--type",
                "elf",
                "--version",
                "1.0",
                "--build",
                "1",
                // The build UUID is still accepted but must NOT become the
                // symbol identity — the real build-id above proves that.
                "--uuid",
                "00000000-0000-0000-0000-000000000000",
                &zip,
            ])
            .assert()
            .success();
    })
    .await
    .unwrap();
    // server drop verifies the POST(real build-id, breakpad) + PUT counts.
}
