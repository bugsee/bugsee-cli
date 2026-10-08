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

use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

/// `file(1)` on the fixture: `BuildID[md5/uuid]=bca64abfec40dbb631bb8f1c37414472`.
const FIXTURE_BUILD_ID: &str = "bca64abfec40dbb631bb8f1c37414472";

fn pack_native_zip(dir: &Path, entry_name: &str) -> PathBuf {
    let elf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let bytes = std::fs::read(&elf).expect("ELF fixture present");
    let zip_path = dir.join("native-debug-symbols.zip");
    let f = std::fs::File::create(&zip_path).unwrap();
    let mut zw = zip::ZipWriter::new(f);
    zw.start_file(entry_name, zip::write::SimpleFileOptions::default())
        .unwrap();
    zw.write_all(&bytes).unwrap();
    zw.finish().unwrap();
    zip_path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_registers_each_so_by_its_real_build_id() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = pack_native_zip(tmp.path(), "arm64-v8a/libsymbol1.so");

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
        let mut c = common::cli();
        c.args([
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

/// SYMBOL_TABLE → FULL upgrade. AGP's `.so.sym` (function names only) and a
/// later `.so.dbg` (with line info) of the same library share its GNU build-id,
/// and the server dedups on that alone. The server below models a library
/// already stored from a `.so.sym` upload: it answers 16004 unless the POST asks
/// to overwrite. Without `--force` the `.so.dbg` is skipped and nothing is PUT;
/// with it, the POST carries `overwrite: true` and the bytes are sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_force_overwrites_a_library_already_on_the_server() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = pack_native_zip(tmp.path(), "arm64-v8a/libsymbol1.so.dbg");

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(r#""overwrite":true"#))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": false,
            "error": {"type": "DuplicateSymbolsFoundError", "code": 16004}
        })))
        .with_priority(2)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1) // only the --force run transfers bytes
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let zip = zip_path.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || {
        let run = |extra: &[&str]| {
            let mut c = common::cli();
            c.args([
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
                "--uuid",
                "00000000-0000-0000-0000-000000000000",
            ]);
            c.args(extra).arg(&zip).assert()
        };
        run(&[])
            .success()
            .stderr(predicates::str::contains("re-run with --force"));
        run(&["--force"]).success();
    })
    .await
    .unwrap();

    let posts: Vec<serde_json::Value> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 2);
    assert!(posts[0].get("overwrite").is_none(), "{}", posts[0]);
    assert_eq!(posts[1]["overwrite"], true);
    assert_eq!(posts[1]["uuid"], FIXTURE_BUILD_ID);
}

/// Zip one of the ELF fixtures under `entry_name` (the unstripped library, or the same
/// library with its debug sections removed, which shares its build-id).
fn pack_fixture_zip(dir: &Path, fixture: &str, entry_name: &str) -> PathBuf {
    let elf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/elf")
        .join(fixture);
    let zip_path = dir.join("native-debug-symbols.zip");
    let mut zw = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
    zw.start_file(entry_name, zip::write::SimpleFileOptions::default())
        .unwrap();
    zw.write_all(&std::fs::read(elf).unwrap()).unwrap();
    zw.finish().unwrap();
    zip_path
}

/// Run `debug-files upload --type elf` on `zip` against `endpoint`; returns stderr.
async fn run_elf_upload_of(
    endpoint: String,
    zip: PathBuf,
    extra: &'static [&'static str],
) -> String {
    tokio::task::spawn_blocking(move || {
        let zip = zip.to_string_lossy().into_owned();
        let inputs = [zip];
        let out = common::cli()
            .args(elf_args(&endpoint, &inputs))
            .args(extra)
            .assert()
            .success();
        String::from_utf8_lossy(&out.get_output().stderr).into_owned()
    })
    .await
    .unwrap()
}

/// Drop ANSI colour sequences so a log line can be matched as plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn post_bodies(requests: &[wiremock::Request]) -> Vec<serde_json::Value> {
    requests
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

/// Every native upload declares how rich its file is and asks the server to replace a
/// poorer stored copy — no `--force`, so no `overwrite`. The declaration is read off the
/// FILE (debug info vs symbol table), never off its name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_declares_its_richness_and_asks_for_replace_if_richer() {
    for (fixture, entry, expected) in [
        ("libsymbol1.so", "arm64-v8a/libsymbol1.so", "dwarf"),
        (
            "libsymbol1.symtab.so",
            "arm64-v8a/libsymbol1.so.sym",
            "symtab",
        ),
        // Name and content disagree: the content decides.
        ("libsymbol1.symtab.so", "arm64-v8a/libsymbol1.so", "symtab"),
        ("libsymbol1.so", "arm64-v8a/libsymbol1.so.sym", "dwarf"),
    ] {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let zip = pack_fixture_zip(tmp.path(), fixture, entry);
        let put_url = format!("{}/put/sym", server.uri());
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        run_elf_upload_of(server.uri(), zip, &[]).await;

        let posts = post_bodies(&server.received_requests().await.unwrap());
        assert_eq!(posts.len(), 1, "{fixture} as {entry}");
        assert_eq!(posts[0]["format_variant"], expected, "{fixture} as {entry}");
        assert_eq!(posts[0]["replace_if_richer"], true, "{fixture} as {entry}");
        assert!(posts[0].get("overwrite").is_none(), "{}", posts[0]);
        assert_eq!(posts[0]["uuid"], FIXTURE_BUILD_ID);
        assert_eq!(posts[0]["transform"], "breakpad");
    }
}

/// Acceptance: the server holds a symbol table for the build-id; the unstripped library
/// replaces it with NO `--force`, transfers its bytes once, and the run reports the upgrade.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_reports_an_upgrade_and_transfers_once() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip = pack_fixture_zip(tmp.path(), "libsymbol1.so", "arm64-v8a/libsymbol1.so");
    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(body_string_contains(r#""format_variant":"dwarf""#))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"code": 0, "endpoint": put_url, "upgraded": true})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let stderr = strip_ansi(&run_elf_upload_of(server.uri(), zip, &[]).await);

    assert!(stderr.contains("upgraded SYMBOL_TABLE -> FULL"), "{stderr}");
    assert!(stderr.contains("uploaded=1 upgraded=1"), "{stderr}");
    assert!(!stderr.contains("re-run with --force"), "{stderr}");
}

/// Acceptance: re-running with the same file, or offering a symbol table when the server
/// holds DWARF, transfers nothing — the server answers 16004 and says what it keeps — and
/// the old "re-run with --force" advice does not appear, because it would be wrong.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_skips_without_transfer_when_the_server_keeps_the_richer_copy() {
    for (fixture, entry, stored) in [
        ("libsymbol1.so", "arm64-v8a/libsymbol1.so", "dwarf"),
        (
            "libsymbol1.symtab.so",
            "arm64-v8a/libsymbol1.so.sym",
            "dwarf",
        ),
        (
            "libsymbol1.symtab.so",
            "arm64-v8a/libsymbol1.so.sym",
            "symtab",
        ),
    ] {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let zip = pack_fixture_zip(tmp.path(), fixture, entry);
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": false,
                "error": {
                    "type": "DuplicateSymbolsFoundError",
                    "code": 16004,
                    "params": {"format_variant": stored}
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // nothing is transferred
            .mount(&server)
            .await;

        let stderr = run_elf_upload_of(server.uri(), zip, &[]).await;

        assert!(stderr.contains("same or better level"), "{stderr}");
        assert!(!stderr.contains("re-run with --force"), "{stderr}");
    }
}

/// When the server does not say what it holds (a server that predates this, or a copy
/// with no variant), the per-library line must not claim it is "the same or better".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_does_not_claim_same_or_better_when_the_server_does_not_say() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip = pack_fixture_zip(tmp.path(), "libsymbol1.so", "arm64-v8a/libsymbol1.so");
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": false,
            "error": {"type": "DuplicateSymbolsFoundError", "code": 16004}
        })))
        .mount(&server)
        .await;

    let stderr = run_elf_upload_of(server.uri(), zip, &[]).await;

    assert!(stderr.contains("already on server, skipped"), "{stderr}");
    assert!(!stderr.contains("same or better"), "{stderr}");
    // The summary still tells the operator how to replace a copy this server cannot upgrade.
    assert!(stderr.contains("re-run with --force"), "{stderr}");
}

/// `--force` still means "always replace": it sends `overwrite` and still declares the
/// file's richness, so the record it creates is labelled for the next comparison.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_force_still_overwrites_and_declares_richness() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip = pack_fixture_zip(
        tmp.path(),
        "libsymbol1.symtab.so",
        "arm64-v8a/libsymbol1.so.sym",
    );
    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    run_elf_upload_of(server.uri(), zip, &["--force"]).await;

    let posts = post_bodies(&server.received_requests().await.unwrap());
    assert_eq!(posts[0]["overwrite"], true);
    assert_eq!(posts[0]["format_variant"], "symtab");
}

/// `--extension` lets a caller pick up a symbol spelling the CLI does not know
/// yet: the entry is keyed by its real build-id exactly like a `.so`. Without
/// the flag the same archive uploads nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_picks_up_an_extra_suffix_entry() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = pack_native_zip(tmp.path(), "arm64-v8a/libsymbol1.so.debug");

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(FIXTURE_BUILD_ID))
        .and(body_string_contains("breakpad"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .expect(1) // only the run WITH --extension registers anything
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let zip = zip_path.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || {
        let run = |extra: &[&str]| {
            let mut c = common::cli();
            c.args([
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
                "--uuid",
                "00000000-0000-0000-0000-000000000000",
            ]);
            c.args(extra).arg(&zip).assert().success();
        };
        run(&[]);
        run(&["--extension", "so.debug"]);
    })
    .await
    .unwrap();
}

/// GNU split-debug companions — a `libfoo.so` and its `libfoo.so.debug`, same
/// build-id — are ONE symbol. With `--extension so.debug` both match by name;
/// registering both raced two POSTs for one id. Exactly one is registered and PUT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_sends_one_library_per_build_id() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let elf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let bytes = std::fs::read(&elf).unwrap();
    let zip_path = tmp.path().join("native-debug-symbols.zip");
    {
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
        for name in ["arm64-v8a/libsymbol1.so", "arm64-v8a/libsymbol1.so.debug"] {
            zw.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zw.write_all(&bytes).unwrap();
        }
        zw.finish().unwrap();
    }

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(FIXTURE_BUILD_ID))
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
    let zip = zip_path.to_string_lossy().into_owned();
    tokio::task::spawn_blocking(move || {
        let mut c = common::cli();
        c.args([
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
            "--uuid",
            "00000000-0000-0000-0000-000000000000",
            "--extension",
            "so.debug",
            &zip,
        ])
        .assert()
        .success()
        .stderr(predicates::str::contains("uploading only the richer one"));
    })
    .await
    .unwrap();
}

/// A dry run is how a caller checks whether an archive will upload anything, so
/// an archive with no name-matching entry must point at `--extension` there too.
#[test]
fn elf_dry_run_with_no_matching_entries_suggests_extension() {
    let tmp = tempfile::tempdir().unwrap();
    let zip_path = pack_native_zip(tmp.path(), "arm64-v8a/libsymbol1.so.debug");
    let mut c = common::cli();
    c.args([
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
        "--uuid",
        "00000000-0000-0000-0000-000000000000",
        "--dry-run",
    ])
    .arg(&zip_path)
    .assert()
    .success()
    .stderr(predicates::str::contains("pass it with --extension"));
}

fn elf_args<'a>(endpoint: &'a str, inputs: &'a [String]) -> Vec<&'a str> {
    let mut a = vec![
        "--endpoint",
        endpoint,
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
        "--uuid",
        "00000000-0000-0000-0000-000000000000",
    ];
    a.extend(inputs.iter().map(String::as_str));
    a
}

/// A directory (AGP's `merged_native_libs` layout) is walked recursively. The
/// same library under two ABI folders, plus a zip of it, shares ONE build-id,
/// so exactly one POST + PUT go out; the non-library file is ignored.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_walks_a_directory_and_dedups_by_build_id_across_paths() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let root = tmp.path().join("merged_native_libs/release/out/lib");
    for abi in ["arm64-v8a", "x86_64"] {
        std::fs::create_dir_all(root.join(abi)).unwrap();
        std::fs::copy(&fixture, root.join(abi).join("libsymbol1.so")).unwrap();
    }
    std::fs::write(root.join("x86_64/notes.txt"), b"ignored").unwrap();
    let zip_path = pack_native_zip(tmp.path(), "arm64-v8a/libsymbol1.so");

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .and(body_string_contains(FIXTURE_BUILD_ID))
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
    let inputs = vec![
        tmp.path()
            .join("merged_native_libs")
            .to_string_lossy()
            .into_owned(),
        zip_path.to_string_lossy().into_owned(),
    ];
    tokio::task::spawn_blocking(move || {
        common::cli()
            .args(elf_args(&endpoint, &inputs))
            .assert()
            .success();
    })
    .await
    .unwrap();
}

/// An input path that exists as neither a directory nor a readable zip still
/// fails (it is not silently skipped), with a message naming it.
#[test]
fn elf_upload_rejects_a_missing_path() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope").to_string_lossy().into_owned();
    let inputs = vec![missing];
    common::cli()
        .args(elf_args("http://127.0.0.1:1", &inputs))
        .assert()
        .code(10)
        .stderr(predicates::str::contains("nope"));
}

/// Two zips whose entries share a basename and archive index but carry DIFFERENT
/// build-ids must each upload their own bytes (extraction must not collide).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn elf_upload_keeps_same_named_entries_of_different_zips_apart() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let a = pack_native_zip(tmp.path(), "arm64-v8a/libsymbol1.so");
    let a = {
        let d = tmp.path().join("a");
        std::fs::create_dir_all(&d).unwrap();
        let t = d.join("a.zip");
        std::fs::rename(a, &t).unwrap();
        t
    };
    // Same fixture with the build-id's first byte flipped (found by value).
    let mut other = std::fs::read(&fixture).unwrap();
    let id = hex::decode(FIXTURE_BUILD_ID).unwrap();
    let pos = other.windows(id.len()).position(|w| w == id).unwrap();
    other[pos] ^= 0xff;
    let other_id = hex::encode(&other[pos..pos + id.len()]);
    let b = tmp.path().join("b.zip");
    {
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&b).unwrap());
        zw.start_file(
            "x86_64/libsymbol1.so",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zw.write_all(&other).unwrap();
        zw.finish().unwrap();
    }

    let put_url = format!("{}/put/sym", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    let inputs = vec![
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];
    tokio::task::spawn_blocking(move || {
        common::cli()
            .args(elf_args(&endpoint, &inputs))
            .assert()
            .success();
    })
    .await
    .unwrap();

    let reqs = server.received_requests().await.unwrap();
    let posts: Vec<String> = reqs
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| String::from_utf8_lossy(&r.body).into_owned())
        .collect();
    assert!(posts.iter().any(|p| p.contains(FIXTURE_BUILD_ID)));
    assert!(posts.iter().any(|p| p.contains(&other_id)));
    let puts = reqs.iter().filter(|r| r.method.as_str() == "PUT").count();
    assert_eq!(puts, 2);
}

/// An unreadable/nonexistent directory root must not exit 0.
/// Any I/O error while scanning a directory fails the run (exit 11): skipping
/// would upload a partial set — a whole ABI missing — behind a green build.
#[cfg(unix)]
#[test]
fn elf_upload_fails_on_unreadable_directory_content() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let lock = |p: &Path| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root bypasses permission checks; nothing to assert then.
        std::fs::read_dir(p).is_err() && std::fs::read(p).is_err()
    };
    let unlock = |p: &Path| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    let run = |root: &Path| {
        let inputs = vec![root.to_string_lossy().into_owned()];
        common::cli()
            .args(elf_args("http://127.0.0.1:1", &inputs))
            .assert()
            .code(11)
    };

    // unreadable root
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("locked");
    std::fs::create_dir_all(&root).unwrap();
    if lock(&root) {
        run(&root);
    }
    unlock(&root);

    // a good library plus an unreadable ABI subdirectory
    let root = tmp.path().join("r2");
    std::fs::create_dir_all(root.join("x86_64")).unwrap();
    std::fs::copy(&fixture, root.join("libok.so")).unwrap();
    if lock(&root.join("x86_64")) {
        run(&root);
    }
    unlock(&root.join("x86_64"));

    // a good library plus an unreadable library file
    let root = tmp.path().join("r3");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(&fixture, root.join("libok.so")).unwrap();
    std::fs::copy(&fixture, root.join("libbad.so")).unwrap();
    if lock(&root.join("libbad.so")) {
        run(&root);
    }
    unlock(&root.join("libbad.so"));

    // a dangling symlink named like a library
    let root = tmp.path().join("r4");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::copy(&fixture, root.join("libok.so")).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("gone"), root.join("libdangling.so")).unwrap();
    run(&root);
}

/// A directory holding only a zip, and a bare `.so`, get pointed at the right input.
#[test]
fn elf_upload_hints_for_a_zip_in_a_directory_and_a_bare_so() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("native-debug-symbols");
    std::fs::create_dir_all(&dir).unwrap();
    pack_native_zip(&dir, "arm64-v8a/libsymbol1.so");
    let inputs = vec![dir.to_string_lossy().into_owned()];
    common::cli()
        .args(elf_args("http://127.0.0.1:1", &inputs))
        .assert()
        .code(10)
        .stderr(predicates::str::contains("pass the zip itself"));

    let so = tmp.path().join("libx.so");
    std::fs::write(&so, b"x").unwrap();
    let inputs = vec![so.to_string_lossy().into_owned()];
    common::cli()
        .args(elf_args("http://127.0.0.1:1", &inputs))
        .assert()
        .code(11)
        .stderr(predicates::str::contains("pass its directory"));
}

/// A directory that exists but holds no libraries is a miswired path / too-early
/// task, not a finished upload: exit 10, never 0.
#[test]
fn elf_upload_empty_directory_is_input_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let inputs = vec![tmp.path().to_string_lossy().into_owned()];
    common::cli()
        .args(elf_args("http://127.0.0.1:1", &inputs))
        .assert()
        .code(10)
        .stderr(predicates::str::contains("no .so"));
}

/// The fixture with its GNU build-id's first byte flipped: same ELF, different identity.
/// (Unix-only because its sole user is: an unused helper is a hard error under CI's
/// `-D warnings` on Windows.)
#[cfg(unix)]
fn fixture_with_other_build_id() -> (Vec<u8>, String) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let mut bytes = std::fs::read(&fixture).unwrap();
    let id = hex::decode(FIXTURE_BUILD_ID).unwrap();
    let pos = bytes.windows(id.len()).position(|w| w == id).unwrap();
    bytes[pos] ^= 0xff;
    let other = hex::encode(&bytes[pos..pos + id.len()]);
    (bytes, other)
}

/// Directory symlinks are not descended; a symlink to a library file is read.
/// The escaped library has its OWN build-id, so following the link would make it
/// 2 libraries (same-id copies would be collapsed by dedup and prove nothing).
#[cfg(unix)]
#[test]
fn elf_dry_run_does_not_descend_directory_symlinks_but_reads_file_symlinks() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(
        outside.join("libescaped.so"),
        fixture_with_other_build_id().0,
    )
    .unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();
    std::os::unix::fs::symlink(&fixture, root.join("liblinked.so")).unwrap();
    let inputs = vec![root.to_string_lossy().into_owned()];
    let mut args = elf_args("http://127.0.0.1:1", &inputs);
    args.push("--dry-run");
    common::cli()
        .args(args)
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "would register + upload 1 libraries",
        ));
}
