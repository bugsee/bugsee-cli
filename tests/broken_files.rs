//! How the compiled binary behaves on BROKEN input files: corrupted or truncated JSON,
//! ELF, Mach-O/DWARF, PDB, zip, plist and dependency manifests.
//!
//! The contract these pin, per flow:
//!  - never a crash (no panic exit 101, no signal / abort);
//!  - a stable exit code an integrator can branch on, and a message naming the file;
//!  - nothing is sent to the server for input that could not be identified;
//!  - the user's own files are left exactly as they were;
//!  - stdout stays reserved for structured output.
//!
//! Where a flow deliberately TOLERATES a broken file (it warns and carries on), the test
//! says so, so loosening or tightening that is a conscious change.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

const FIXTURE_ELF: &str = "tests/fixtures/elf/libsymbol1.so";

fn write(dir: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p
}

fn fixture_elf() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_ELF)).unwrap()
}

/// Exit code of a finished process, failing the test on a crash (panic = 101 or a signal).
fn code_of(out: &std::process::Output) -> i32 {
    let code = out.status.code().unwrap_or(-1);
    assert!(
        code != 101 && code >= 0,
        "the binary CRASHED (status {:?}):\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    code
}

fn run(args: &[&str]) -> std::process::Output {
    common::cli().args(args).output().unwrap()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// Source-map JSON: upload, --strip-sources-content and inject, one table
// ---------------------------------------------------------------------------

const VALID_MAP: &str = r#"{"version":3,"debug_id":"22222222-2222-2222-2222-222222222222","sources":["a"],"sourcesContent":["x"],"mappings":"AAAA"}"#;

/// (label, map bytes, upload dry-run, strip dry-run, inject) exit codes.
///
/// 11 = input invalid. Non-object but well-formed JSON (`[]`, `3`) is NOT a syntax
/// error: upload / strip dry runs tolerate it (there is no id to read), while
/// `inject` cannot write an id into it and says so.
fn json_cases() -> Vec<(&'static str, Vec<u8>, i32, i32, i32)> {
    let deep_array = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let deep_object = format!("{}1{}", r#"{"a":"#.repeat(50_000), "}".repeat(50_000));
    let bom = format!("\u{feff}{VALID_MAP}");
    let trailing_doc = format!("{VALID_MAP}{VALID_MAP}");
    let truncated = VALID_MAP[..40].to_string();
    let trailing_comma = format!("{},}}", &VALID_MAP[..VALID_MAP.len() - 1]);
    let c = |label, bytes: &str, a, b, c| (label, bytes.as_bytes().to_vec(), a, b, c);
    vec![
        c("empty", "", 11, 11, 11),
        c("whitespace only", "  \n\t ", 11, 11, 11),
        c("truncated", &truncated, 11, 11, 11),
        c("trailing comma", &trailing_comma, 11, 11, 11),
        c("byte-order mark", &bom, 11, 11, 11),
        c("two documents", &trailing_doc, 11, 11, 11),
        c("NaN literal", r#"{"a":NaN}"#, 11, 11, 11),
        c("bad escape", r#"{"a":"\x"}"#, 11, 11, 11),
        c("lone surrogate", r#"{"debug_id":"\ud800"}"#, 11, 11, 11),
        c(
            "raw control char in a string",
            "{\"a\":\"x\ty\"}",
            11,
            11,
            11,
        ),
        c("NUL byte in a string", "{\"a\":\"x\0y\"}", 11, 11, 11),
        c("100k-deep arrays", &deep_array, 11, 11, 11),
        c("50k-deep objects", &deep_object, 11, 11, 11),
        c("array at the top level", "[]", 0, 0, 11),
        c("number at the top level", "3", 0, 0, 11),
        c("null at the top level", "null", 0, 0, 11),
        c("string at the top level", "\"s\"", 0, 0, 11),
        // Numbers no double holds (or with a 3-digit exponent) are valid JSON: they are
        // skipped when reading ids and copied verbatim when rewriting, never refused.
        c(
            "number beyond any double",
            r#"{"version":3,"n":1e999999,"mappings":""}"#,
            0,
            0,
            0,
        ),
        c(
            "three-digit exponent",
            r#"{"version":3,"n":1e100,"mappings":""}"#,
            0,
            0,
            0,
        ),
        // Duplicate keys are legal; the last one wins, as in any JSON parser.
        c(
            "duplicate keys",
            r#"{"debug_id":"a","debug_id":"b","mappings":""}"#,
            0,
            0,
            0,
        ),
    ]
}

#[test]
fn broken_source_maps_never_crash_and_exit_with_stable_codes() {
    for (label, bytes, want_upload, want_strip, want_inject) in json_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let map = write(tmp.path(), "m/app.js.map", &bytes);
        let dir = tmp.path().join("m");
        let dir_s = dir.to_str().unwrap();
        let base = [
            "debug-files",
            "upload",
            "--type",
            "sourcemaps",
            "--version",
            "1",
            "--build",
            "1",
            "--dry-run",
        ];

        let up = common::cli().args(base).arg(&dir).output().unwrap();
        assert_eq!(
            code_of(&up),
            want_upload,
            "upload: {label}\n{}",
            stderr(&up)
        );
        assert!(
            up.stdout.is_empty(),
            "upload: {label}: stdout must stay empty"
        );

        let strip = common::cli()
            .args(base)
            .arg("--strip-sources-content")
            .arg(&dir)
            .output()
            .unwrap();
        assert_eq!(
            code_of(&strip),
            want_strip,
            "strip: {label}\n{}",
            stderr(&strip)
        );

        // Inject needs a bundle next to the map; it must not touch a map it rejects.
        write(tmp.path(), "m/app.js", b"a()\n");
        let inj = run(&["sourcemaps", "inject", dir_s]);
        assert_eq!(
            code_of(&inj),
            want_inject,
            "inject: {label}\n{}",
            stderr(&inj)
        );
        if want_inject == 11 {
            assert_eq!(
                std::fs::read(&map).unwrap(),
                bytes,
                "inject rewrote a map it rejected: {label}"
            );
            assert_eq!(
                std::fs::read(dir.join("app.js")).unwrap(),
                b"a()\n",
                "inject stamped the bundle although it rejected the map: {label}"
            );
            let left: Vec<_> = std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(left.len(), 2, "{label}: stray files left behind: {left:?}");
        }
        if want_upload == 11 {
            assert!(
                stderr(&up).contains(map.to_str().unwrap()) || stderr(&up).contains("sourcemap"),
                "{label}: the message must name the problem: {}",
                stderr(&up)
            );
        }
    }
}

/// A real (non-dry-run) upload of a map that cannot be identified registers NOTHING.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unidentifiable_source_map_sends_nothing_to_the_server() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code": 0})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    for (i, bytes) in [&b"{ not json"[..], b"[]", b"\"s\"", b"", b"{\"a\":"]
        .iter()
        .enumerate()
    {
        write(tmp.path(), &format!("m{i}/app.js.map"), bytes);
    }
    let endpoint = server.uri();
    let root = tmp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        for i in 0..5 {
            let out = common::cli()
                .args(["--endpoint", &endpoint, "--app-token", "TKN"])
                .args([
                    "debug-files",
                    "upload",
                    "--type",
                    "sourcemaps",
                    "--version",
                    "1",
                    "--build",
                    "1",
                ])
                .arg(root.join(format!("m{i}")))
                .output()
                .unwrap();
            let code = code_of(&out);
            assert_ne!(code, 0, "case m{i} must fail: {}", stderr(&out));
        }
    })
    .await
    .unwrap();
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

/// The bytes of the single entry of the (only) uploaded ZIP.
async fn uploaded_entry(server: &MockServer) -> Vec<u8> {
    let reqs = server.received_requests().await.unwrap();
    let put = reqs
        .iter()
        .find(|r| r.method.as_str() == "PUT")
        .expect("uploaded");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(put.body.clone())).unwrap();
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut archive.by_index(0).unwrap(), &mut body).unwrap();
    body
}

/// `--strip-sources-content` over a map it cannot process (not a JSON object) uploads the
/// ORIGINAL (a privacy preference is not a reason to fail the upload) — and says so
/// loudly, because the sources then go out unstripped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_over_an_unprocessable_map_uploads_the_original_and_warns() {
    let server = mock_symbol_server().await;
    let tmp = tempfile::tempdir().unwrap();
    // Valid JSON, but an array: it has no `sourcesContent` to strip. `--uuid` supplies its key.
    let odd = br#"[{"sourcesContent":["secret"]}]"#;
    write(tmp.path(), "m/app.js.map", odd);
    let endpoint = server.uri();
    let dir = tmp.path().join("m");
    let warned = tokio::task::spawn_blocking(move || {
        let out = common::cli()
            .args(["--endpoint", &endpoint, "--app-token", "TKN"])
            .args([
                "debug-files",
                "upload",
                "--type",
                "sourcemaps",
                "--strip-sources-content",
            ])
            .args(["--version", "1", "--build", "1"])
            .args(["--uuid", "55555555-5555-5555-5555-555555555555"])
            .arg(&dir)
            .output()
            .unwrap();
        assert_eq!(code_of(&out), 0, "{}", stderr(&out));
        stderr(&out).contains("UNSTRIPPED")
    })
    .await
    .unwrap();
    assert!(
        warned,
        "the unstripped fallback must be announced, not silent"
    );
    assert_eq!(
        uploaded_entry(&server).await,
        odd.to_vec(),
        "the original bytes, untouched"
    );
}

/// REGRESSION: numbers that are valid JSON but unusual (`1e100`, beyond any double, 150
/// digits) used to make the strip give up and upload the UNSTRIPPED map, silently. The sources
/// must be gone from what is uploaded, and every number's text preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strip_removes_sources_from_maps_with_unusual_numbers() {
    let long = "7".repeat(150);
    for n in ["1e100", "-3E-120", "1e999999", long.as_str()] {
        let server = mock_symbol_server().await;
        let tmp = tempfile::tempdir().unwrap();
        let map = format!(
            r#"{{"version":3,"debug_id":"66666666-6666-6666-6666-666666666666","n":{n},"sourcesContent":["secret source"],"mappings":"AAAA"}}"#
        );
        write(tmp.path(), "m/app.js.map", map.as_bytes());
        let endpoint = server.uri();
        let dir = tmp.path().join("m");
        tokio::task::spawn_blocking(move || {
            let out = common::cli()
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
                .output()
                .unwrap();
            assert_eq!(code_of(&out), 0, "{}", stderr(&out));
            assert!(!stderr(&out).contains("UNSTRIPPED"), "{}", stderr(&out));
        })
        .await
        .unwrap();
        let uploaded = String::from_utf8(uploaded_entry(&server).await).unwrap();
        assert!(
            !uploaded.contains("secret source"),
            "{n}: the source leaked: {uploaded}"
        );
        assert!(
            uploaded.contains(&format!(r#""n":{n}"#)),
            "{n}: number text changed: {uploaded}"
        );
    }
}

// ---------------------------------------------------------------------------
// ELF: damaged libraries in a directory, damaged zips
// ---------------------------------------------------------------------------

fn elf_dry_run(input: &Path) -> std::process::Output {
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
        .arg(input)
        .output()
        .unwrap()
}

/// A directory with one good library and every kind of damaged one: the damaged are
/// warned about and skipped, the good one is still uploaded, and the run succeeds.
#[test]
fn damaged_libraries_are_skipped_with_a_warning_and_the_good_one_survives() {
    let elf = fixture_elf();
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "libs/libgood.so", &elf);
    write(tmp.path(), "libs/libempty.so", b"");
    write(tmp.path(), "libs/libhalf.so", &elf[..elf.len() / 2]);
    write(tmp.path(), "libs/libhead.so", &elf[..64]);
    write(tmp.path(), "libs/libzero.so", &vec![0u8; 8192]);
    write(
        tmp.path(),
        "libs/libtext.so",
        b"#!/bin/sh\necho not a library\n",
    );
    // A VALID Mach-O (it has a real UUID) named *.so, as a macOS-built module would be:
    // not an ELF, so it must not become an "elf" symbol keyed by its Mach-O UUID.
    let macho = thin_macho_with_uuid([0x5a; 16]);
    write(tmp.path(), "libs/libmacho.so", &macho);

    let out = elf_dry_run(&tmp.path().join("libs"));
    assert_eq!(code_of(&out), 0, "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    let err = stderr(&out);
    assert!(err.contains("would register + upload 1 libraries"), "{err}");
    assert_eq!(err.matches("no GNU build-id").count(), 6, "{err}");
}

/// If NOTHING in the directory is usable, that is a run with no symbols: it succeeds
/// (the zip contract) but says so.
#[test]
fn a_directory_of_only_damaged_libraries_uploads_nothing_and_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "libs/liba.so", b"junk");
    write(tmp.path(), "libs/libb.so", &[0u8; 100]);
    let out = elf_dry_run(&tmp.path().join("libs"));
    assert_eq!(code_of(&out), 0);
    assert!(
        stderr(&out).contains("would register + upload 0 libraries"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn broken_native_symbol_zips_are_input_invalid() {
    let elf = fixture_elf();
    let tmp = tempfile::tempdir().unwrap();
    let good = write(tmp.path(), "good.zip", &[]);
    {
        let mut zw = zip::ZipWriter::new(std::fs::File::create(&good).unwrap());
        zw.start_file(
            "arm64-v8a/libfoo.so",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        std::io::Write::write_all(&mut zw, &elf).unwrap();
        zw.finish().unwrap();
    }
    let raw = std::fs::read(&good).unwrap();
    let mut crc = raw.clone();
    let mid = crc.len() / 2;
    crc[mid] ^= 0x5a;
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty file", Vec::new()),
        ("truncated in half", raw[..raw.len() / 2].to_vec()),
        (
            "end-of-central-directory missing",
            raw[..raw.len() - 12].to_vec(),
        ),
        (
            "random bytes",
            (0..4000u32).map(|i| (i * 7 % 251) as u8).collect(),
        ),
        ("a flipped byte in the payload", crc),
    ];
    for (label, bytes) in cases {
        let p = write(tmp.path(), "bad.zip", &bytes);
        let out = elf_dry_run(&p);
        assert_eq!(code_of(&out), 11, "{label}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("bad.zip"),
            "{label}: names the file: {}",
            stderr(&out)
        );
    }
}

// ---------------------------------------------------------------------------
// Mach-O / dSYM / PDB
// ---------------------------------------------------------------------------

fn bundle_with(parent: &Path, name: &str, dwarf: &[u8]) -> PathBuf {
    let b = parent.join(format!("{name}.dSYM"));
    write(&b, &format!("Contents/Resources/DWARF/{name}"), dwarf);
    b
}

fn thin_macho_with_uuid(uuid: [u8; 16]) -> Vec<u8> {
    let mut b = vec![0xcf, 0xfa, 0xed, 0xfe];
    b.extend_from_slice(&0x0100_000cu32.to_le_bytes()); // arm64
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0x0au32.to_le_bytes()); // MH_DSYM
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&24u32.to_le_bytes());
    b.extend_from_slice(&[0u8; 8]);
    b.extend_from_slice(&0x1bu32.to_le_bytes()); // LC_UUID
    b.extend_from_slice(&24u32.to_le_bytes());
    b.extend_from_slice(&uuid);
    b
}

#[test]
fn dsym_uuid_on_broken_machos_prints_an_empty_list_and_never_a_nil_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let valid = thin_macho_with_uuid([0xAB; 16]);
    let cases: Vec<(&str, Vec<u8>, Vec<&str>)> = vec![
        (
            "valid",
            valid.clone(),
            vec!["ABABABAB-ABAB-ABAB-ABAB-ABABABABABAB"],
        ),
        ("no LC_UUID (nil)", thin_macho_with_uuid([0; 16]), vec![]),
        ("empty", Vec::new(), vec![]),
        ("magic only", vec![0xcf, 0xfa, 0xed, 0xfe], vec![]),
        (
            "fat header claiming 2^32 slices",
            [
                &[0xca, 0xfe, 0xba, 0xbe, 0xff, 0xff, 0xff, 0xff][..],
                &[0u8; 64],
            ]
            .concat(),
            vec![],
        ),
        (
            "garbage",
            (0..3000u32).map(|i| (i * 31 % 253) as u8).collect(),
            vec![],
        ),
        (
            "valid, cut mid-command",
            valid[..valid.len() - 5].to_vec(),
            vec![],
        ),
    ];
    for (label, bytes, want) in cases {
        for target in ["file", "bundle"] {
            let bundle = bundle_with(tmp.path(), "App", &bytes);
            let path = if target == "file" {
                bundle.join("Contents/Resources/DWARF/App")
            } else {
                bundle
            };
            let out = run(&["dsym", "uuid", path.to_str().unwrap()]);
            assert_eq!(code_of(&out), 0, "{label}/{target}: {}", stderr(&out));
            let got: Vec<String> = serde_json::from_slice(&out.stdout)
                .unwrap_or_else(|e| panic!("{label}/{target}: stdout is not a JSON array ({e})"));
            assert_eq!(got, want, "{label}/{target}");
        }
    }
}

#[test]
fn a_dsym_with_unreadable_dwarf_is_skipped_by_upload_but_fails_the_build_phase() {
    let tmp = tempfile::tempdir().unwrap();
    for (label, bytes) in [
        (
            "garbage",
            (0..3000u32)
                .map(|i| (i * 31 % 253) as u8)
                .collect::<Vec<u8>>(),
        ),
        ("empty", Vec::new()),
        ("no LC_UUID", thin_macho_with_uuid([0; 16])),
    ] {
        let dsyms = tmp.path().join(label.replace(' ', "_"));
        bundle_with(&dsyms, "App", &bytes);

        // `debug-files upload`: the bundle is warned about and skipped (exit 0, nothing sent).
        let out = common::cli()
            .args([
                "debug-files",
                "upload",
                "--type",
                "dsym",
                "--version",
                "1",
                "--build",
                "1",
                "--dry-run",
            ])
            .arg(&dsyms)
            .output()
            .unwrap();
        assert_eq!(code_of(&out), 0, "{label}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("not a readable dSYM bundle"),
            "{label}: {}",
            stderr(&out)
        );

        // `xcode upload-dsyms`: symbols silently not uploaded is the failure it exists to
        // prevent, so the same bundle FAILS the build phase (11).
        let out = common::cli()
            .args(["xcode", "upload-dsyms", "--no-background"])
            .env("DWARF_DSYM_FOLDER_PATH", &dsyms)
            .env("BUGSEE_APP_TOKEN", "TKN")
            .env("BUGSEE_ENDPOINT", "http://127.0.0.1:1")
            .output()
            .unwrap();
        assert_eq!(code_of(&out), 11, "{label}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("could not be read"),
            "{label}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn broken_pdbs_are_not_identified_and_never_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let msf = b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0";
    for (label, bytes, want) in [
        ("empty", Vec::new(), 10),
        (
            "random bytes",
            (0..4000u32).map(|i| (i * 13 % 251) as u8).collect(),
            10,
        ),
        ("MSF magic only", msf.to_vec(), 11),
        (
            "MSF magic + zeroed superblock",
            [&msf[..], &[0u8; 64]].concat(),
            11,
        ),
        (
            "MSF magic + hostile superblock",
            [&msf[..], &[0xffu8; 64]].concat(),
            11,
        ),
    ] {
        let dir = tmp.path().join(label.replace([' ', '+'], "_"));
        write(&dir, "a.pdb", &bytes);
        let out = common::cli()
            .args([
                "debug-files",
                "upload",
                "--type",
                "pdb",
                "--version",
                "1",
                "--build",
                "1",
                "--dry-run",
            ])
            .arg(&dir)
            .output()
            .unwrap();
        assert_eq!(code_of(&out), want, "{label}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{label}");
    }
}

// ---------------------------------------------------------------------------
// Plists
// ---------------------------------------------------------------------------

/// `build-env read-plist` reads `{}` for anything unusable, INCLUDING nesting deep
/// enough to overflow a stack (which used to abort the process with SIGABRT).
#[test]
fn read_plist_of_broken_files_is_an_empty_object_and_never_a_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let nested_xml = [
        &b"<?xml version=\"1.0\"?><plist version=\"1.0\">"[..],
        &b"<array>".repeat(60_000),
        &b"</array>".repeat(60_000),
        b"</plist>",
    ]
    .concat();
    for (label, bytes) in [
        ("empty", Vec::new()),
        (
            "random bytes",
            (0..300u32).map(|i| (i * 37 % 251) as u8).collect(),
        ),
        (
            "truncated XML",
            b"<?xml version=\"1.0\"?><plist><dict><key>CFBundle".to_vec(),
        ),
        ("bplist magic only", b"bplist00".to_vec()),
        (
            "array root",
            b"<?xml version=\"1.0\"?><plist version=\"1.0\"><array/></plist>".to_vec(),
        ),
        ("60k-deep nesting", nested_xml),
    ] {
        let p = write(tmp.path(), "Info.plist", &bytes);
        let out = run(&["build-env", "read-plist", p.to_str().unwrap()]);
        assert_eq!(code_of(&out), 0, "{label}: {}", stderr(&out));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "{}", "{label}");
    }
}

// ---------------------------------------------------------------------------
// iOS dependency manifests
// ---------------------------------------------------------------------------

fn ios_deps(pods: &[u8], pins: &[u8]) -> Value {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "Podfile.lock", pods);
    write(tmp.path(), "Package.resolved", pins);
    let out = common::cli()
        .args(["ios-deps", "collect", "--project-root"])
        .arg(tmp.path())
        .output()
        .unwrap();
    assert_eq!(code_of(&out), 0, "{}", stderr(&out));
    serde_json::from_slice(&out.stdout).expect("stdout is always the JSON document")
}

/// Broken manifests degrade to fewer entries, never to a crash, a non-JSON stdout, or a
/// bogus nameless dependency in the graph.
#[test]
fn broken_dependency_manifests_still_yield_a_clean_json_document() {
    let deep = [&b"["[..].repeat(100_000)[..], &b"]".repeat(100_000)].concat();
    let big_lock = [&b"PODS:\n"[..], &b"  - Foo (1.0)\n".repeat(50_000)].concat();
    let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
        ("empty", Vec::new(), Vec::new()),
        (
            "random bytes",
            (0..500u32).map(|i| (i * 37 % 251) as u8).collect(),
            (0..500u32).map(|i| (i * 53 % 251) as u8).collect(),
        ),
        (
            "truncated",
            b"PODS:\n  - Foo (1.0\n  - ".to_vec(),
            br#"{"pins":[{"identity":"a","state":{"ver"#.to_vec(),
        ),
        (
            "not UTF-8",
            b"PODS:\n  - \xff\xfe (1.0)\n".to_vec(),
            b"\xff".to_vec(),
        ),
        (
            "nameless pod lines",
            b"PODS:\n  - (1.0)\n  - \n  - Real (2.0)\n".to_vec(),
            Vec::new(),
        ),
        ("100k-deep JSON", Vec::new(), deep),
        ("50k pods", big_lock, Vec::new()),
    ];
    for (label, pods, pins) in cases {
        let doc = ios_deps(&pods, &pins);
        let entries = doc["entries"]
            .as_array()
            .unwrap_or_else(|| panic!("{label}: no entries array"));
        for e in entries {
            let name = e["name"].as_str().unwrap_or("");
            assert!(
                !name.is_empty(),
                "{label}: nameless dependency in the graph: {e}"
            );
            assert_ne!(e["id"], "library::", "{label}");
        }
    }
    // The survivable line of a mangled lockfile is still reported.
    let doc = ios_deps(b"PODS:\n  - (1.0)\n  - \n  - Real (2.0)\n", b"");
    let names: Vec<&str> = doc["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Real"]);
}

// ---------------------------------------------------------------------------
// Build registration payloads and artefacts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broken_build_payload_fails_before_anything_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().unwrap();
    let artifact = write(tmp.path(), "app.aab", b"PK\x03\x04 fake");
    let endpoint = server.uri();
    let root = tmp.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        for (label, payload) in [
            ("empty", &b""[..]),
            ("truncated", br#"{"uuid":"a","vers"#),
            ("an array", b"[]"),
            ("null", b"null"),
            ("binary", &[0xff, 0xfe, 0x00, 0x01]),
        ] {
            let p = write(&root, "p.json", payload);
            let out = common::cli()
                .args([
                    "--endpoint",
                    &endpoint,
                    "--app-token",
                    "TKN",
                    "upload",
                    "build",
                ])
                .arg("--payload-json")
                .arg(&p)
                .arg("--artifact")
                .arg(&artifact)
                .output()
                .unwrap();
            assert_eq!(code_of(&out), 11, "{label}: {}", stderr(&out));
            assert!(
                stderr(&out).contains("--payload-json"),
                "{label}: {}",
                stderr(&out)
            );
        }
    })
    .await
    .unwrap();
}

#[test]
fn unusable_build_artifacts_are_input_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = write(tmp.path(), "p.json", br#"{"uuid":"a"}"#);
    let dir = tmp.path().join("a_directory");
    std::fs::create_dir_all(&dir).unwrap();
    for (label, artifact) in [
        ("a directory", dir),
        ("a missing file", tmp.path().join("nope.aab")),
    ] {
        let out = common::cli()
            .args([
                "--endpoint",
                "http://127.0.0.1:1",
                "--app-token",
                "TKN",
                "upload",
                "build",
                "--dry-run",
            ])
            .arg("--payload-json")
            .arg(&payload)
            .arg("--artifact")
            .arg(&artifact)
            .output()
            .unwrap();
        assert_eq!(code_of(&out), 10, "{label}: {}", stderr(&out));
    }
}

// ---------------------------------------------------------------------------
// IL2CPP line maps: a documented gap (characterisation)
// ---------------------------------------------------------------------------

/// The IL2CPP mappings JSON is packed as-is and NOT validated: a corrupt file uploads
/// "successfully". This pins that on purpose — rejecting it would be a behaviour
/// change integrators must be told about — so a future validation lands deliberately.
#[test]
fn il2cpp_linemap_does_not_validate_the_mappings_json_today() {
    let tmp = tempfile::tempdir().unwrap();
    for (label, bytes) in [
        ("empty", &b""[..]),
        ("truncated", br#"{"a":"#),
        ("binary", &[0xff, 0xfe, 0x00]),
    ] {
        let dir = tmp.path().join(label);
        write(&dir, "LineNumberMappings.json", bytes);
        let out = common::cli()
            .args([
                "debug-files",
                "upload",
                "--type",
                "il2cpp-linemap",
                "--version",
                "1",
                "--build",
                "1",
            ])
            .args([
                "--uuid",
                "11111111-2222-3333-4444-555555555555",
                "--dry-run",
            ])
            .arg(&dir)
            .output()
            .unwrap();
        assert_eq!(code_of(&out), 0, "{label}: {}", stderr(&out));
    }
}
