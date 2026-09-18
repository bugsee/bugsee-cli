//! The CLI must survive a hostile-but-legal process environment.
//!
//! Xcode and CI environments are large and not curated: any tool in the build
//! may set a variable this CLI has never heard of. Reading the environment is
//! not allowed to abort the run over one of them.

use assert_cmd::Command;

// Only the Unix-gated tests below use this today (a non-UTF-8 env var is not constructible on
// Windows, where `OsString` is WTF-16), and `-D warnings` makes an unused helper a hard error —
// which is how `cargo test` was broken on Windows while every CI job stayed green.
#[cfg(unix)]
fn cli() -> Command {
    let mut c = Command::cargo_bin("bugsee-cli").expect("compiled bugsee-cli binary");
    c.env_clear();
    c
}

/// `should_daemonize` runs for EVERY subcommand, before anything else. It must
/// not read the whole environment: `std::env::vars()` panics on a non-UTF-8
/// key or value, which would turn any unrelated command into exit 101 — a code
/// outside the documented contract entirely, so an integrator branching on
/// `should_fallback` gets undefined behaviour.
///
/// Regression test: a non-Unicode variable in the environment is not this
/// CLI's business, and plenty of real environments carry one.
#[cfg(unix)]
#[test]
fn a_non_utf8_env_var_does_not_crash_unrelated_subcommands() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let bad = OsString::from_vec(vec![0xff, 0xfe, 0x80, b'b', b'a', b'd']);
    // Every command that collects the environment. `vcs-metadata`, `build-env`
    // and `xcode post-action` each build their own map, so each is its own
    // opportunity to reintroduce the panic.
    for args in [
        vec!["dsym", "uuid", "/nonexistent"],
        vec!["xcode", "upload-dsyms"],
        vec!["xcode", "post-action", "--force-foreground"],
        vec!["vcs-metadata"],
        vec!["build-env", "machine-label"],
    ] {
        let out = cli()
            .args(&args)
            .env("BUGSEE_BAD_VAR", &bad)
            .env("DWARF_DSYM_FOLDER_PATH", "/nonexistent")
            .output()
            .unwrap();
        assert_ne!(
            out.status.code(),
            Some(101),
            "`{}` panicked on a non-UTF-8 env var: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
