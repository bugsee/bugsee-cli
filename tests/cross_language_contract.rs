//! Cross-language wire-shape contract tests.
//!
//! The `bugsee-cli` JSON output is the **source of truth** for two
//! Python BugseeAgents (the fastlane plugin and the iOS SDK's
//! `tools.bundle/BugseeAgent`) plus the Android Gradle plugin's
//! upload pipeline. Drift between Rust's serialised output and what
//! the Python parsers expect is the highest-impact bug class — it
//! silently degrades coverage with no error surface.
//!
//! Pre-existing tests rely heavily on `mock.patch(subprocess.run)` +
//! canned JSON strings on the Python side, and serde-derive
//! assumptions on the Rust side. Neither half exercises the COMPILED
//! binary against checked-in fixtures, so a serde-rename mutation, a
//! key-name typo, or a `skip_serializing_if` flip would pass every
//! test in both repos and only break in production. The C-series
//! review report flagged this as the single highest-stakes contract-
//! drift hazard.
//!
//! Each test here runs the compiled `bugsee-cli` against a fixture
//! under `tests/fixtures/<subcmd>/` and pins the stdout JSON shape
//! end-to-end. Pure shape pins (top-level keys, lowercase casing,
//! omission of None-valued optional fields). Concrete value pins
//! only where a value is part of the contract (e.g. SPM url
//! extraction).
//!
//! Pairs with each Python BugseeAgent's `_via_cli` helper tests —
//! together they form the cross-language reference vector.

use assert_cmd::Command;
use serde_json::Value;
use std::path::PathBuf;

fn cli() -> Command {
    Command::cargo_bin("bugsee-cli").expect("compiled bugsee-cli binary")
}

fn fixture(path: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(path);
    p
}

/// Run `bugsee-cli <argv>` with no env vars set, capture stdout,
/// require exit 0, and parse as JSON. Helper for the common case.
fn run_and_parse_json(args: &[&str]) -> Value {
    let assert = cli().args(args).env_clear().assert().success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)
        .expect("stdout is utf-8")
        .trim()
        .to_string();
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("CLI output for {args:?} not JSON: {e}\n---\n{stdout}\n---"))
}

// ─── ios-deps ───────────────────────────────────────────────────────

#[test]
fn ios_deps_collect_emits_expected_top_level_keys() {
    // Top-level shape: `{ entries: [...], scope_label: "all",
    // truncated: false }`. The fastlane plugin's
    // `_collect_all_dependencies_via_cli` reads
    // `data.get("entries")`, the SDK side reads the same. A typo
    // (`entry` vs `entries`) or a rename would silently produce
    // empty deps payloads on both sides.
    let root = fixture("ios_deps");
    let v = run_and_parse_json(&[
        "ios-deps",
        "collect",
        "--project-root",
        root.to_str().unwrap(),
    ]);
    assert!(v.get("entries").is_some(), "missing top-level `entries`");
    assert!(
        v.get("scope_label").is_some(),
        "missing top-level `scope_label`"
    );
    assert!(
        v.get("truncated").is_some(),
        "missing top-level `truncated`"
    );
    assert_eq!(v["scope_label"], "all");
    assert_eq!(v["truncated"], false);
    assert!(v["entries"].is_array(), "`entries` must be an array");
}

#[test]
fn ios_deps_collect_emits_expected_per_entry_keys() {
    let root = fixture("ios_deps");
    let v = run_and_parse_json(&[
        "ios-deps",
        "collect",
        "--project-root",
        root.to_str().unwrap(),
    ]);
    let entries = v["entries"].as_array().expect("entries is array");
    assert!(
        !entries.is_empty(),
        "fixture should produce non-empty entries"
    );
    for e in entries {
        let obj = e.as_object().expect("entry is object");
        // Required lowercase keys. Drift on any of these = silent
        // wire-shape divergence the Python parsers can't tolerate.
        for required in ["id", "group", "name", "direct", "type", "parents"] {
            assert!(
                obj.contains_key(required),
                "entry missing `{required}`: {e}",
            );
        }
        // `id` is a colon-separated string; `direct` is bool.
        assert!(obj["id"].is_string(), "id must be string in {e}");
        assert!(obj["direct"].is_boolean(), "direct must be bool in {e}");
        assert!(obj["parents"].is_array(), "parents must be array in {e}");
    }
}

#[test]
fn ios_deps_collect_omits_none_valued_optional_fields() {
    // The Rust DepEntry uses
    // `#[serde(skip_serializing_if = "Option::is_none")]` for
    // `version`, `scope`, and `url`. The Python parsers rely on
    // this — they use `if e.get("url"):` rather than
    // `if e.get("url") is not None:`. If a future Rust change
    // started emitting `"scope":null`, the Python side would
    // tolerate it but the wire bytes would drift across versions.
    // Pin omission explicitly.
    let root = fixture("ios_deps");
    let v = run_and_parse_json(&[
        "ios-deps",
        "collect",
        "--project-root",
        root.to_str().unwrap(),
    ]);
    for e in v["entries"].as_array().unwrap() {
        let obj = e.as_object().unwrap();
        // `scope` is None for every fixture entry → must be absent.
        assert!(
            !obj.contains_key("scope"),
            "scope:None must be omitted, not serialised as null: {e}",
        );
    }
}

#[test]
fn ios_deps_collect_extracts_spm_url_from_package_resolved() {
    // The url field is the most-load-bearing of the optional
    // fields — it's the join key for OSV's SwiftURL ecosystem
    // vuln lookups. Pin that the SPM `location` (v2/v3 key)
    // round-trips into the JSON `url` field for the
    // `swift-collections` fixture entry.
    let root = fixture("ios_deps");
    let v = run_and_parse_json(&[
        "ios-deps",
        "collect",
        "--project-root",
        root.to_str().unwrap(),
    ]);
    let swift_collections = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "swift-collections")
        .expect("swift-collections must surface in entries");
    assert_eq!(
        swift_collections["url"], "https://github.com/apple/swift-collections.git",
        "SPM url must be propagated from Package.resolved.location",
    );
}

// ─── build-env ──────────────────────────────────────────────────────

#[test]
fn build_env_read_plist_emits_string_keyed_dict() {
    // Output shape: a JSON object with string keys → string values
    // (or numbers for numeric plist entries). The fastlane and SDK
    // Python wrappers `json.loads()` then index with the requested
    // key. A list-shaped output would break the index lookup.
    let plist = fixture("build_env/Info.plist");
    let v = run_and_parse_json(&["build-env", "read-plist", plist.to_str().unwrap()]);
    let obj = v.as_object().expect("read-plist must emit an object");
    assert_eq!(obj["CFBundleIdentifier"], "com.example.app");
    assert_eq!(obj["CFBundleShortVersionString"], "1.2.3");
    assert_eq!(obj["CFBundleVersion"], "42");
}

#[test]
fn build_env_read_plist_returns_empty_object_on_missing_file() {
    // Pinned posture: parseable-failure exits 0 with `{}`. Python
    // consumers shell with `check=False` and rely on this so they
    // can short-circuit to the fallback rather than special-case
    // an exit code.
    let assert = cli()
        .args(["build-env", "read-plist", "/no/such/path.plist"])
        .env_clear()
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)
        .unwrap()
        .trim();
    assert_eq!(
        stdout, "{}",
        "missing plist must emit empty object on stdout"
    );
}

#[test]
fn build_env_xcode_version_emits_three_part_dotted_form() {
    // Pin the cross-resolver-path normalisation (recent fix).
    // Both the XCODE_VERSION_ACTUAL env-path AND the xcodebuild
    // fallback path now produce three-part dotted strings. The
    // env-path branch is what an Xcode Run Script triggers.
    let assert = cli()
        .args(["build-env", "xcode-version"])
        .env_clear()
        .env("XCODE_VERSION_ACTUAL", "1620")
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)
        .unwrap()
        .trim();
    assert_eq!(stdout, "16.2.0", "env-path must emit 3-part dotted form");
}

// ─── vcs-metadata ───────────────────────────────────────────────────

#[test]
fn vcs_metadata_github_push_emits_expected_keys() {
    // The Android Gradle plugin's canonical Kotlin
    // VcsMetadataResolver pins these field names. Drift on the
    // Rust side would silently mis-render the dashboard's commit
    // metadata column for every CI build.
    let assert = cli()
        .args(["vcs-metadata"])
        .env_clear()
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_SHA", "abc123def456")
        .env("GITHUB_REPOSITORY", "org/repo")
        .env("GITHUB_REF", "refs/heads/master")
        .env("GITHUB_EVENT_NAME", "push")
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)
        .unwrap()
        .trim()
        .to_string();
    let v: Value = serde_json::from_str(&stdout).expect("vcs-metadata output is JSON");
    assert_eq!(v["provider"], "github");
    assert_eq!(v["commit_sha"], "abc123def456");
    assert_eq!(v["repo"], "org/repo");
    assert_eq!(v["branch"], "master");
    // Absent fields must be omitted, not serialised as null.
    assert!(
        v.get("base_branch").is_none(),
        "base_branch:None must be omitted for a push event; got {v}",
    );
    assert!(
        v.get("pr_number").is_none(),
        "pr_number:None must be omitted for a push event; got {v}",
    );
}

#[test]
fn vcs_metadata_github_tag_push_omits_branch() {
    // Recent fix pin: tag pushes set GITHUB_REF=refs/tags/<tag>
    // and we now leave `branch` absent instead of emitting the
    // literal ref string. The cross-language contract is the
    // Kotlin canonical resolver's behaviour
    // (`removePrefix(...).takeIf { it != ref }`).
    let assert = cli()
        .args(["vcs-metadata"])
        .env_clear()
        .env("GITHUB_ACTIONS", "true")
        .env("GITHUB_SHA", "tag-sha-aabbcc")
        .env("GITHUB_REPOSITORY", "org/repo")
        .env("GITHUB_REF", "refs/tags/v1.0.0")
        .env("GITHUB_EVENT_NAME", "push")
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    assert_eq!(v["provider"], "github");
    assert!(
        v.get("branch").is_none(),
        "tag-pushed branch must be omitted; got {v}",
    );
}

// ─── dsym ───────────────────────────────────────────────────────────

#[test]
fn dsym_uuid_emits_empty_array_on_missing_path() {
    // Empty-fallback contract: dSYM extractor returns `[]` for any
    // input it can't parse, exit 0. Python `parseDSYM` relies on
    // empty list = "nothing parseable" without an exit-code check.
    let assert = cli()
        .args(["dsym", "uuid", "/no/such/dsym"])
        .env_clear()
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)
        .unwrap()
        .trim();
    assert_eq!(stdout, "[]");
}

#[test]
fn dsym_slices_emits_empty_array_on_missing_path() {
    // Parallel pin for the arch-aware variant. SDK's
    // `_load_macho_slices_via_cli` parses `json.loads()` and
    // checks `isinstance(data, list)` — must be an array, not
    // null or a dict.
    let assert = cli()
        .args(["dsym", "slices", "/no/such/dsym"])
        .env_clear()
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout)
        .unwrap()
        .trim();
    assert_eq!(stdout, "[]");
}
