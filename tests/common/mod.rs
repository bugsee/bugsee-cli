//! Shared helpers for the integration tests that drive the COMPILED binary.

use assert_cmd::Command;

/// `bugsee-cli` with a cleared environment — plus, on Windows, the OS variables the child cannot
/// work without.
///
/// The tests clear the environment so an ambient `BUGSEE_*` or CI variable cannot decide what the
/// binary does. On Unix that is all it means. On Windows, `env_clear()` also removes `SystemRoot`,
/// and WinSock cannot initialise without it: every request from the child then fails with
/// "error sending request", three retries and exit 31, with nothing to say why. Found the first time
/// the suite ran on Windows at all — `cargo test` had only ever run on ubuntu and macOS.
pub fn cli() -> Command {
    let mut command = Command::cargo_bin("bugsee-cli").expect("compiled bugsee-cli binary");
    command.env_clear();
    #[cfg(windows)]
    for key in [
        "SystemRoot",
        "windir",
        "SystemDrive",
        "TEMP",
        "TMP",
        "USERPROFILE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}
