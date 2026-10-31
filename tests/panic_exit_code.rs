//! A panic must never leave the binary as exit 101 (outside the exit-code contract, so every
//! integrator reads it as a substantive failure). It maps by phase: BEFORE any request was sent ->
//! exit 1 (fallback is safe), AFTER one may have been -> exit 50 (server state unknown, no fallback).
//!
//! The panic is injected by `BUGSEE_CLI_TEST_PANIC` in `upload::http::send_with_retry`, which only
//! exists in debug builds, so this whole file is compiled out of `cargo test --release`.
#![cfg(debug_assertions)]

use std::path::Path;

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

fn mapping(dir: &Path) -> std::path::PathBuf {
    let p = dir.join("mapping.txt");
    std::fs::write(&p, "com.example.Foo -> a.a:\n    int bar -> a\n").unwrap();
    p
}

async fn upload_with_panic(server: &MockServer, point: &'static str) -> (Option<i32>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let map = mapping(tmp.path());
    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || {
        let out = common::cli()
            .env("BUGSEE_CLI_TEST_PANIC", point)
            .args([
                "--endpoint",
                &endpoint,
                "--app-token",
                "TKN",
                "debug-files",
                "upload",
            ])
            .arg(&map)
            .args(["--type", "proguard", "--version", "1", "--build", "1"])
            .args(["--uuid", "11111111-1111-1111-1111-111111111111"])
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_before_any_request_exits_1_and_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0) // nothing may reach the server
        .mount(&server)
        .await;
    let (code, stderr) = upload_with_panic(&server, "before-send").await;
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("injected test panic (before-send)"),
        "{stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn panic_after_a_request_was_sent_exits_50() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1) // the request really was sent
        .mount(&server)
        .await;
    let (code, stderr) = upload_with_panic(&server, "after-send").await;
    assert_eq!(code, Some(50), "stderr: {stderr}");
    assert!(
        stderr.contains("injected test panic (after-send)"),
        "{stderr}"
    );
}

#[test]
fn no_panic_keeps_the_normal_exit_codes() {
    // An unknown flag is clap's usage error: still 2, untouched by the panic mapping.
    let out = common::cli().arg("--no-such-flag").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
