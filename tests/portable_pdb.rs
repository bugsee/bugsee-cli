//! `debug-files upload --type portable-pdb` and `debug-files convert --to bmf` through the COMPILED
//! binary: the compiler's assemblies and portable PDBs become a BMF registered under the module id
//! (MVID) of the assembly the app actually ships.
//!
//! The fixtures are in `tests/fixtures/dotnet/`: `BugseeSample.dll` + `.pdb` (built by Roslyn from
//! `BugseeSample.cs`) and `BugseeSample.rewritten.dll`, a rebuild with one method removed and one
//! method's body changed, standing in for Unity's linker output.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

/// The module id (MVID) of `BugseeSample.dll`, and of its rewritten stand-in.
const COMPILER_MVID: &str = "8481329b49854132970cd9220dad97be";
const SHIPPED_MVID: &str = "db54951704a1467eacf4bb55b8c77bfe";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dotnet")
        .join(name)
}

/// A directory holding the compiler's output.
fn compiler_dir(root: &Path) -> PathBuf {
    let dir = root.join("compiler");
    std::fs::create_dir_all(&dir).unwrap();
    for name in ["BugseeSample.dll", "BugseeSample.pdb"] {
        std::fs::copy(fixture(name), dir.join(name)).unwrap();
    }
    dir
}

/// A directory holding the shipped (rewritten) copy under the original file name.
fn shipped_dir(root: &Path) -> PathBuf {
    let dir = root.join("shipped");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(
        fixture("BugseeSample.rewritten.dll"),
        dir.join("BugseeSample.dll"),
    )
    .unwrap();
    dir
}

/// stderr with the colour codes `tracing` adds removed, so a field reads as `name=value`.
fn stderr(out: &std::process::Output) -> String {
    let raw = String::from_utf8_lossy(&out.stderr);
    let mut plain = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for end in chars.by_ref() {
                if end.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
}

async fn mock_symbol_server() -> MockServer {
    let server = MockServer::start().await;
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
    server
}

/// The registration bodies and the entries of every uploaded ZIP, in request order.
async fn uploads(server: &MockServer) -> (Vec<Value>, Vec<Vec<(String, Vec<u8>)>>) {
    let reqs = server.received_requests().await.unwrap();
    let posts = reqs
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect();
    let zips = reqs
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .map(|r| {
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(r.body.clone())).unwrap();
            (0..archive.len())
                .map(|i| {
                    let mut file = archive.by_index(i).unwrap();
                    let mut bytes = Vec::new();
                    file.read_to_end(&mut bytes).unwrap();
                    // `enclosed_name` (not `name`) so this compiles on zip 8 and 9 alike.
                    let name = file.enclosed_name().unwrap().to_string_lossy().into_owned();
                    (name, bytes)
                })
                .collect()
        })
        .collect();
    (posts, zips)
}

fn upload_args(paths: &[&Path], extra: &[&str]) -> Vec<String> {
    let mut a: Vec<String> = [
        "debug-files",
        "upload",
        "--type",
        "portable-pdb",
        "--version",
        "1.2",
        "--build",
        "34",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.extend(paths.iter().map(|p| p.to_str().unwrap().to_string()));
    a.extend(extra.iter().map(|s| s.to_string()));
    a
}

async fn run_upload(server: &MockServer, args: Vec<String>) -> std::process::Output {
    let endpoint = server.uri();
    tokio::task::spawn_blocking(move || {
        common::cli()
            .args(["--endpoint", &endpoint, "--app-token", "TKN"])
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_registers_a_bmf_under_the_assemblys_module_id() {
    let server = mock_symbol_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());

    let out = run_upload(&server, upload_args(&[&dir], &[])).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        out.stdout.is_empty(),
        "stdout is reserved for structured output"
    );

    let (posts, zips) = uploads(&server).await;
    assert_eq!(posts.len(), 1);
    let post = &posts[0];
    assert_eq!(post["format"], "bmf");
    assert_eq!(post["uuid"], COMPILER_MVID);
    assert_eq!(post["uuids"], json!([COMPILER_MVID]));
    assert_eq!(post["version"], "1.2");
    assert_eq!(post["build"], "34");
    assert!(
        post.get("overwrite").is_none(),
        "no --force, no overwrite: {post}"
    );
    assert!(post["hash"].as_str().is_some_and(|h| h.len() == 40));

    assert_eq!(zips.len(), 1);
    let entries = &zips[0];
    let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [format!("{COMPILER_MVID}.bmf").as_str(), "manifest.json"]
    );
    assert!(entries[0].1.starts_with(b"BMFH\x01"), "a BMF, version 1");

    let manifest: Value = serde_json::from_slice(&entries[1].1).unwrap();
    assert_eq!(manifest["uuid"], COMPILER_MVID);
    let item = &manifest["items"][0];
    assert_eq!(item["entry"], format!("{COMPILER_MVID}.bmf"));
    assert_eq!(item["name"], "BugseeSample.bmf");
    assert_eq!(item["uuids"], json!([COMPILER_MVID]));
    assert_eq!(item["format"], "bmf");
    assert_eq!(item["version"], "1.2");
    assert_eq!(item["build"], "34");
}

/// With the linker's output the mapping is keyed by what the RUNTIME prints: the shipped module id,
/// never the compiler's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_with_a_shipped_dir_uses_the_shipped_module_id() {
    let server = mock_symbol_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    let shipped = shipped_dir(tmp.path());

    let out = run_upload(
        &server,
        upload_args(
            &[&dir],
            &["--shipped-dir", shipped.to_str().unwrap(), "--force"],
        ),
    )
    .await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));

    let (posts, zips) = uploads(&server).await;
    assert_eq!(posts[0]["uuid"], SHIPPED_MVID);
    assert_eq!(posts[0]["uuids"], json!([SHIPPED_MVID]));
    assert_eq!(posts[0]["overwrite"], true, "--force replaces");
    assert_eq!(zips[0][0].0, format!("{SHIPPED_MVID}.bmf"));
    assert!(
        !String::from_utf8_lossy(&zips[0][0].1).contains(COMPILER_MVID),
        "the compiler's id must not leak into the mapping"
    );
    // What the build removed or rewrote is reported, not silently mapped.
    assert!(
        stderr(&out).contains("dropped_linked_away=2"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("dropped_il_mismatch=1"),
        "{}",
        stderr(&out)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_already_has_the_symbol_is_a_success_not_an_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        // The appserver answers a duplicate with HTTP 200 and the code nested in the envelope.
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": false,
            "error": {"type": "DuplicateSymbolsFoundError", "code": 16004}
        })))
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    let out = run_upload(&server, upload_args(&[&dir], &[])).await;
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("already on server"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn dry_run_packs_and_sends_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    let out = common::cli()
        .args(upload_args(&[&dir], &["--dry-run"]))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stderr(&out).contains("dry-run"), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
}

#[test]
fn convert_writes_one_bmf_per_assembly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    // Neighbours a Unity output directory has: a dll with no pdb, a native library, a non-PE
    // file. None may stop the run.
    std::fs::copy(fixture("BugseeSample.rewritten.dll"), dir.join("NoPdb.dll")).unwrap();
    std::fs::write(dir.join("native.dll"), b"not a PE file at all").unwrap();
    std::fs::write(dir.join("native.pdb"), b"not a pdb either").unwrap();
    let out_dir = tmp.path().join("out");

    let out = common::cli()
        .args([
            "debug-files",
            "convert",
            dir.to_str().unwrap(),
            "--to",
            "bmf",
            "--output",
        ])
        .arg(&out_dir)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let names: Vec<String> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, [format!("{COMPILER_MVID}.bmf")]);
    let bytes = std::fs::read(out_dir.join(&names[0])).unwrap();
    assert!(bytes.starts_with(b"BMFH\x01"));
}

#[test]
fn a_named_assembly_without_its_pdb_is_input_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let dll = tmp.path().join("Lonely.dll");
    std::fs::copy(fixture("BugseeSample.dll"), &dll).unwrap();
    let out = common::cli()
        .args([
            "debug-files",
            "convert",
            dll.to_str().unwrap(),
            "--to",
            "bmf",
            "--output",
        ])
        .arg(tmp.path().join("out"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(10), "{}", stderr(&out));
    assert!(stderr(&out).contains("Lonely.pdb"), "{}", stderr(&out));
}

#[test]
fn a_directory_with_nothing_to_convert_is_input_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::copy(fixture("BugseeSample.dll"), tmp.path().join("NoPdb.dll")).unwrap();
    let out = common::cli()
        .args([
            "debug-files",
            "convert",
            tmp.path().to_str().unwrap(),
            "--to",
            "bmf",
            "--output",
        ])
        .arg(tmp.path().join("out"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(10), "{}", stderr(&out));
}

#[test]
fn a_shipped_dir_that_lacks_the_assembly_converts_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    let empty = tmp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let out = common::cli()
        .args([
            "debug-files",
            "convert",
            dir.to_str().unwrap(),
            "--shipped-dir",
        ])
        .arg(&empty)
        .args(["--to", "bmf", "--output"])
        .arg(tmp.path().join("out"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(10), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no copy of this assembly"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn flags_that_do_not_apply_are_configuration_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    let uuid = "11111111-2222-3333-4444-555555555555";
    let cases: Vec<(Vec<String>, &str)> = vec![
        (
            upload_args(&[&dir], &["--uuid", uuid, "--dry-run"]),
            "--uuid does not apply to --type portable-pdb",
        ),
        (
            vec![
                "debug-files".into(),
                "upload".into(),
                "--type".into(),
                "pdb".into(),
                "--version".into(),
                "1".into(),
                "--build".into(),
                "1".into(),
                "--shipped-dir".into(),
                dir.to_str().unwrap().into(),
                dir.to_str().unwrap().into(),
                "--dry-run".into(),
            ],
            "--shipped-dir is only valid for --type portable-pdb",
        ),
        (
            vec![
                "debug-files".into(),
                "convert".into(),
                dir.to_str().unwrap().into(),
                "--to".into(),
                "bsf".into(),
                "--output".into(),
                tmp.path().join("o").to_str().unwrap().into(),
            ],
            "--to bsf is not implemented",
        ),
    ];
    for (args, message) in cases {
        let out = common::cli().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(20), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains(message), "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn convert_refuses_an_output_that_is_a_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = compiler_dir(tmp.path());
    let file = tmp.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let out = common::cli()
        .args([
            "debug-files",
            "convert",
            dir.to_str().unwrap(),
            "--to",
            "bmf",
            "--output",
        ])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(11), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"x",
        "the user's file is untouched"
    );
}
