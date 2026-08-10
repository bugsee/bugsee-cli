//! Pin metadata POST `format` values for symbol uploads that now send
//! format-scoped uniqueness to the appserver.
//!
//! Regression: ProGuard and JS sourcemap briefly swapped `"mapping"` /
//! `"sourcemap"` when format was first put on the wire.

use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proguard_upload_sends_format_mapping() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let mapping = tmp.path().join("mapping.txt");
    std::fs::write(&mapping, b"# compiler: R8\ncom.Foo -> a:\n").unwrap();

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(r#""format":"mapping""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let path = mapping.to_string_lossy().into_owned();
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
                "proguard",
                "--version",
                "1.0",
                "--build",
                "1",
                &path,
            ])
            .assert()
            .success();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sourcemap_upload_sends_format_sourcemap() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let map_path = tmp.path().join("bundle.js.map");
    let body = r#"{"version":3,"file":"bundle.js","sources":["a.js"],"mappings":"AAAA","debugId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}"#;
    std::fs::write(&map_path, body.as_bytes()).unwrap();

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(r#""format":"sourcemap""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let path = map_path.to_string_lossy().into_owned();
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
                "sourcemaps",
                "--version",
                "1.0",
                "--build",
                "1",
                &path,
            ])
            .assert()
            .success();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn il2cpp_linemap_upload_sends_format_il2cpp_linemap() {
    let server = MockServer::start().await;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/il2cpp-linemap/android");

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(r#""format":"il2cpp-linemap""#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let path = root.to_string_lossy().into_owned();
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
                "il2cpp-linemap",
                "--version",
                "1.0",
                "--build",
                "1",
                "--uuid",
                "deadbeefcafebabe",
                "--force",
                &path,
            ])
            .assert()
            .success();
    })
    .await
    .unwrap();
}
