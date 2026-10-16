//! Log output must carry no ANSI escape codes unless stderr is a terminal.
//!
//! Xcode, Gradle and CI capture stderr into logs, and the detached daemon writes
//! it to `bugsee-cli.log`; escape codes there are noise. Under `assert_cmd` stderr
//! is a pipe, so any ESC byte means the `with_ansi(...)` wiring in `main` regressed.

mod common;

/// `sourcemaps inject` on an empty directory needs no network and logs one info line.
#[test]
fn piped_stderr_has_no_escape_codes() {
    let dir = tempfile::tempdir().unwrap();
    let out = common::cli()
        .env("RUST_LOG", "bugsee_cli=info")
        .env_remove("NO_COLOR")
        .args(["sourcemaps", "inject"])
        .arg(dir.path())
        .assert()
        .success();
    let stderr = out.get_output().stderr.clone();
    let text = String::from_utf8_lossy(&stderr);
    assert!(
        text.contains("sourcemaps inject complete"),
        "expected a log line on stderr, got: {text:?}"
    );
    assert!(
        !stderr.contains(&0x1b),
        "stderr contains ANSI escapes: {text:?}"
    );
}
