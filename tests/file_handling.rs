//! File reading, processing and output, end to end through the COMPILED binary.
//!
//! The unit tests pin each piece; these pin what an integrator can observe: the
//! exact bytes left on disk, the exact bytes sent over the wire (including large
//! payloads that must stream), stdout staying reserved for structured output, and
//! exit codes for unusable input.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const FIXTURE_ELF: &str = "tests/fixtures/elf/libsymbol1.so";
const FIXTURE_BUILD_ID: &str = "bca64abfec40dbb631bb8f1c37414472";

fn write(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p
}

fn sha1_hex(bytes: &[u8]) -> String {
    hex::encode(Sha1::digest(bytes))
}

/// Patterned, non-constant bytes (a constant run would hide a chunking bug).
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 251) % 256) as u8).collect()
}

/// The entries of an uploaded ZIP, decompressed, in order.
fn zip_entries(zip_bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes.to_vec())).unwrap();
    (0..archive.len())
        .map(|i| {
            let mut e = archive.by_index(i).unwrap();
            let mut data = Vec::new();
            e.read_to_end(&mut data).unwrap(); // verifies the CRC
            (e.name().to_string(), data)
        })
        .collect()
}

async fn requests(server: &MockServer, verb: &str) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == verb)
        .collect()
}

// ---------------------------------------------------------------------------
// `sourcemaps inject`: bytes on disk, idempotence, stdout discipline, exit codes
// ---------------------------------------------------------------------------

const MAP: &str = r#"{"version":3,"sources":["a.ts"],"names":[],"mappings":"AAAA"}"#;

#[test]
fn inject_stamps_files_idempotently_and_keeps_stdout_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let js = write(tmp.path(), "dist/app.js", b"console.log(1);\n");
    let map = write(tmp.path(), "dist/app.js.map", MAP.as_bytes());
    let dist = tmp.path().join("dist");

    let out = common::cli()
        .args(["sourcemaps", "inject"])
        .arg(&dist)
        .assert()
        .success()
        .stdout("") // stdout is reserved for structured output
        .get_output()
        .clone();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("inject"),
        "progress goes to stderr"
    );

    let stamped_js = std::fs::read_to_string(&js).unwrap();
    assert!(
        stamped_js.starts_with("console.log(1);\n"),
        "original bytes are a prefix"
    );
    assert!(stamped_js.contains("//# debugId="));
    let map_json: Value = serde_json::from_slice(&std::fs::read(&map).unwrap()).unwrap();
    assert_eq!(map_json["debug_id"], map_json["debugId"]);
    assert_eq!(map_json["mappings"], "AAAA");

    // Second run: no byte changes at all.
    let (js1, map1) = (std::fs::read(&js).unwrap(), std::fs::read(&map).unwrap());
    common::cli()
        .args(["sourcemaps", "inject"])
        .arg(&dist)
        .assert()
        .success();
    assert_eq!(std::fs::read(&js).unwrap(), js1);
    assert_eq!(std::fs::read(&map).unwrap(), map1);
}

#[test]
fn inject_dry_run_reports_but_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let js = write(tmp.path(), "app.js", b"a()\n");
    let map = write(tmp.path(), "app.js.map", MAP.as_bytes());
    common::cli()
        .args(["sourcemaps", "inject", "--dry-run"])
        .arg(tmp.path())
        .assert()
        .success()
        .stdout("");
    assert_eq!(std::fs::read(&js).unwrap(), b"a()\n");
    assert_eq!(std::fs::read(&map).unwrap(), MAP.as_bytes());
    let mut left: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["app.js", "app.js.map"]);
}

#[test]
fn inject_rejects_an_unusable_map_and_leaves_it_alone() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "app.js", b"a()\n");
    let map = write(tmp.path(), "app.js.map", b"{ not json");
    common::cli()
        .args(["sourcemaps", "inject"])
        .arg(tmp.path())
        .assert()
        .code(11) // InputInvalid
        .stderr(predicates::str::contains("not valid JSON"));
    assert_eq!(std::fs::read(&map).unwrap(), b"{ not json");
}

// ---------------------------------------------------------------------------
// stdout discipline for `debug-files upload`
// ---------------------------------------------------------------------------

#[test]
fn upload_dry_runs_write_nothing_to_stdout_for_every_file_type() {
    let tmp = tempfile::tempdir().unwrap();
    let elf = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ELF);
    write(
        tmp.path(),
        "native/lib/arm64-v8a/libsymbol1.so",
        &std::fs::read(elf).unwrap(),
    );
    write(tmp.path(), "proguard/mapping.txt", b"a.B -> c.D:\n");
    write(
        tmp.path(),
        "maps/app.js.map",
        br#"{"version":3,"debug_id":"22222222-2222-2222-2222-222222222222","mappings":"AAAA"}"#,
    );
    for (kind, dir) in [
        ("elf", "native"),
        ("proguard", "proguard"),
        ("sourcemaps", "maps"),
    ] {
        common::cli()
            .args([
                "debug-files",
                "upload",
                "--type",
                kind,
                "--version",
                "1",
                "--build",
                "1",
            ])
            .args([
                "--uuid",
                "00000000-0000-0000-0000-000000000000",
                "--dry-run",
            ])
            .arg(tmp.path().join(dir))
            .assert()
            .success()
            .stdout("");
    }
}

// ---------------------------------------------------------------------------
// What goes over the wire for large payloads
// ---------------------------------------------------------------------------

/// The artefact is the biggest thing the CLI sends: it must arrive intact, with an
/// exact Content-Length (no chunked body), whatever its size relative to the stream
/// chunk — here 5 MB, STORED inside the upload ZIP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_artefact_arrives_intact_with_an_exact_content_length() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let artefact = pattern(5 * 1024 * 1024 + 17);
    let artifact_path = write(tmp.path(), "app.aab", &artefact);
    let payload = write(tmp.path(), "p.json", br#"{"uuid":"abc","version":"1.0"}"#);
    let put_url = format!("{}/artefact-put", server.uri());

    Mock::given(method("POST"))
        .and(wm_path("/v2/apps/TKN/builds"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"ok": true, "result": {"build_id": "b1", "endpoint": put_url}}),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(wm_path("/artefact-put"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || {
        common::cli()
            .args([
                "--endpoint",
                &endpoint,
                "--app-token",
                "TKN",
                "upload",
                "build",
            ])
            .arg("--payload-json")
            .arg(&payload)
            .arg("--artifact")
            .arg(&artifact_path)
            .assert()
            .success();
    })
    .await
    .unwrap();

    let put = &requests(&server, "PUT").await[0];
    assert_eq!(
        put.headers.get("content-length").unwrap().to_str().unwrap(),
        put.body.len().to_string()
    );
    assert!(put.headers.get("transfer-encoding").is_none());
    let entries = zip_entries(&put.body);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "app.aab");
    assert_eq!(
        sha1_hex(&entries[0].1),
        sha1_hex(&artefact),
        "artefact bytes survive the stream"
    );
}

/// A directory of large libraries: each upload ZIP holds exactly that library's
/// bytes, registered under that library's OWN build-id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_libraries_upload_with_their_own_bytes_and_build_ids() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let fixture = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ELF)).unwrap();
    // Two libraries with different build-ids (flip the id's first byte), each padded
    // past the 64 KiB stream chunk so the PUT spans several chunks.
    let id = hex::decode(FIXTURE_BUILD_ID).unwrap();
    let pos = fixture.windows(id.len()).position(|w| w == id).unwrap();
    let mut other = fixture.clone();
    other[pos] ^= 0xff;
    let other_id = hex::encode(&other[pos..pos + id.len()]);
    let (mut a, mut b) = (fixture.clone(), other.clone());
    a.extend(pattern(700_000));
    b.extend(pattern(900_001));
    write(tmp.path(), "libs/lib/arm64-v8a/libfoo.so", &a);
    write(tmp.path(), "libs/lib/x86_64/libfoo.so", &b);

    let put_url = format!("{}/put", server.uri());
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
    let libs = tmp.path().join("libs");
    tokio::task::spawn_blocking(move || {
        common::cli()
            .args(["--endpoint", &endpoint, "--app-token", "TKN"])
            .args([
                "debug-files",
                "upload",
                "--type",
                "elf",
                "--version",
                "1",
                "--build",
                "1",
            ])
            .args(["--uuid", "00000000-0000-0000-0000-000000000000"])
            .arg(&libs)
            .assert()
            .success();
    })
    .await
    .unwrap();

    let posts: Vec<Value> = requests(&server, "POST")
        .await
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let ids: Vec<&str> = posts.iter().map(|p| p["uuid"].as_str().unwrap()).collect();
    assert!(
        ids.contains(&FIXTURE_BUILD_ID) && ids.contains(&other_id.as_str()),
        "{ids:?}"
    );
    for p in &posts {
        assert_eq!(p["format"], "elf");
        assert_eq!(p["transform"], "breakpad");
    }
    // Every PUT body is a zip whose single entry is one of the two libraries, whole,
    // and the `hash` registered for it is the SHA-1 of the ZIP that was uploaded.
    let puts = requests(&server, "PUT").await;
    let mut seen = Vec::new();
    for put in &puts {
        let entries = zip_entries(&put.body);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "libfoo.so");
        seen.push(sha1_hex(&entries[0].1));
        assert!(
            posts.iter().any(|p| p["hash"] == sha1_hex(&put.body)),
            "registered hash must describe the uploaded ZIP"
        );
        assert_eq!(
            put.headers.get("content-length").unwrap().to_str().unwrap(),
            put.body.len().to_string()
        );
    }
    seen.sort();
    let mut want = vec![sha1_hex(&a), sha1_hex(&b)];
    want.sort();
    assert_eq!(seen, want);
}

/// `--strip-sources-content`: what is uploaded has no source and is still a valid
/// map; what is on disk is untouched; the registered hash is of the uploaded bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_sources_content_uploads_a_valid_stripped_copy_and_spares_the_original() {
    let server = MockServer::start().await;
    let tmp = tempfile::tempdir().unwrap();
    let original = json!({
        "version": 3,
        "debug_id": "33333333-3333-3333-3333-333333333333",
        "sources": ["a.ts", "b.ts"],
        "sourcesContent": ["const secret = 1;\n", "x".repeat(300_000)],
        "names": [],
        "mappings": "AAAA;AACA",
    });
    let bytes = serde_json::to_vec(&original).unwrap();
    write(tmp.path(), "maps/app.js.map", &bytes);

    let put_url = format!("{}/put", server.uri());
    Mock::given(method("POST"))
        .and(wm_path("/apps/TKN/symbols"))
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
    let dir = tmp.path().join("maps");
    tokio::task::spawn_blocking(move || {
        common::cli()
            .args(["--endpoint", &endpoint, "--app-token", "TKN"])
            .args([
                "debug-files",
                "upload",
                "--type",
                "sourcemaps",
                "--strip-sources-content",
            ])
            .args(["--version", "1", "--build", "1"])
            .arg(&dir)
            .assert()
            .success();
    })
    .await
    .unwrap();

    let entries = zip_entries(&requests(&server, "PUT").await[0].body);
    assert_eq!(entries.len(), 1);
    let uploaded: Value = serde_json::from_slice(&entries[0].1).unwrap();
    let mut expected = original.clone();
    expected.as_object_mut().unwrap().remove("sourcesContent");
    assert_eq!(uploaded, expected, "exactly the map minus its sources");

    let post: Value = serde_json::from_slice(&requests(&server, "POST").await[0].body).unwrap();
    assert_eq!(
        post["uuid"], "33333333-3333-3333-3333-333333333333",
        "the key is carried over"
    );
    assert_eq!(
        post["hash"],
        sha1_hex(&entries[0].1),
        "hash describes what was uploaded"
    );
    assert_eq!(
        std::fs::read(tmp.path().join("maps/app.js.map")).unwrap(),
        bytes,
        "the user's own map is not rewritten"
    );
}

/// A ProGuard mapping goes out as ONE zstd entry whose bytes are the file's — also
/// when the file is empty or not valid UTF-8 (identity is over raw bytes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proguard_mappings_round_trip_raw_bytes() {
    for (label, content) in [
        ("empty", Vec::new()),
        ("crlf", b"a.B -> c.D:\r\n    int x -> a\r\n".to_vec()),
        ("non-utf8", b"a.B -> c.D:\n\xff\xfe\x00\x01\n".to_vec()),
        ("large", pattern(400_003)),
    ] {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "pg/mapping.txt", &content);
        let put_url = format!("{}/put", server.uri());
        Mock::given(method("POST"))
            .and(wm_path("/apps/TKN/symbols"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"code": 0, "endpoint": put_url})),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let endpoint = server.uri();
        let dir = tmp.path().join("pg");
        tokio::task::spawn_blocking(move || {
            common::cli()
                .args(["--endpoint", &endpoint, "--app-token", "TKN"])
                .args(["debug-files", "upload", "--type", "proguard"])
                .args(["--version", "1", "--build", "1"])
                .arg(&dir)
                .assert()
                .success();
        })
        .await
        .unwrap();

        let entries = zip_entries(&requests(&server, "PUT").await[0].body);
        assert_eq!(
            entries,
            [("mapping.txt".to_string(), content.clone())],
            "{label}"
        );
        let post: Value = serde_json::from_slice(&requests(&server, "POST").await[0].body).unwrap();
        // Java `UUID.nameUUIDFromBytes`: MD5 of the raw bytes, version-3 / IETF bits.
        let mut md5 = <md5::Md5 as md5::Digest>::digest(&content);
        md5[6] = (md5[6] & 0x0f) | 0x30;
        md5[8] = (md5[8] & 0x3f) | 0x80;
        assert_eq!(
            post["uuid"].as_str().unwrap().replace('-', ""),
            hex::encode(md5),
            "{label}: debug-id is over the raw bytes"
        );
    }
}

/// An empty/zero-byte library inside a scanned directory must not crash the scan
/// (it cannot be memory-mapped): it is reported as having no build-id and skipped.
#[test]
fn a_zero_byte_library_in_a_directory_is_skipped_not_fatal() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ELF)).unwrap();
    write(tmp.path(), "libs/libok.so", &fixture);
    write(tmp.path(), "libs/libempty.so", b"");
    common::cli()
        .args([
            "debug-files",
            "upload",
            "--type",
            "elf",
            "--version",
            "1",
            "--build",
            "1",
        ])
        .args([
            "--uuid",
            "00000000-0000-0000-0000-000000000000",
            "--dry-run",
        ])
        .arg(tmp.path().join("libs"))
        .assert()
        .success()
        .stderr(predicates::str::contains("no GNU build-id"))
        .stderr(predicates::str::contains(
            "would register + upload 1 libraries",
        ));
}
