//! Stable exit-code contract for `bugsee-cli`.
//!
//! Integrations (Gradle plugin, MSBuild target, fastlane plugin, npm wrapper)
//! depend on these codes to decide whether to fall back to their in-language
//! uploader. The contract is part of the public CLI surface — changing
//! a code's meaning is a breaking change.
//!
//! ## Categories
//!
//! | Range  | Meaning                                                              | Caller should fall back? |
//! |--------|----------------------------------------------------------------------|--------------------------|
//! | 0      | Success (artifact uploaded, or server reports it already exists).    | n/a                      |
//! | 1      | Unexpected / unhandled error.                                        | **yes**                  |
//! | 2      | Usage / argv error (likely a plugin↔CLI version mismatch).           | **yes**                  |
//! | 10–19  | Input / discovery problems (file not found, unparseable format).      | no                       |
//! | 20–29  | Configuration problems (bad token, unreachable endpoint).            | no                       |
//! | 30–39  | Upload problems (network, server 4xx/5xx).                            | no                       |
//! | 40     | Build gate failed deliberately (e.g. size-check FAIL).               | no                       |
//! | 41+    | Reserved.                                                            | no                       |
//!
//! The rationale for `should_fallback`: codes 1 and 2 indicate the CLI never
//! got a fair chance to run. Codes ≥ 10 indicate a substantive failure that
//! would hit the fallback uploader the same way — fallback would only burn
//! time and produce confusing logs.

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Success = 0,
    Unexpected = 1,
    Usage = 2,

    InputNotFound = 10,
    InputInvalid = 11,

    ConfigInvalid = 20,
    AppTokenRejected = 21,

    UploadServer = 30,
    UploadTransport = 31,

    /// A deliberate build gate failed (the build grew past a configured
    /// size-check threshold). Terminal — the build SHOULD fail — but NOT a
    /// structural CLI failure, so an integrating bootstrapper must propagate it
    /// as a build failure rather than fall back to its in-language path.
    SizeCheckFailed = 40,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }

    /// Whether an integrating plugin should fall back to its in-language
    /// uploader when the CLI exits with this code. See the module-level
    /// table for the rationale.
    ///
    /// Not invoked by the binary itself — it's the contract integrators
    /// (Gradle plugin, MSBuild, etc.) consume by re-deriving the same
    /// rule against this CLI's exit code, or by depending on this crate
    /// as a library and calling this method directly.
    #[allow(dead_code)]
    pub fn should_fallback(self) -> bool {
        matches!(self, ExitCode::Unexpected | ExitCode::Usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_only_on_structural_failures() {
        assert!(ExitCode::Unexpected.should_fallback());
        assert!(ExitCode::Usage.should_fallback());

        assert!(!ExitCode::Success.should_fallback());
        assert!(!ExitCode::InputNotFound.should_fallback());
        assert!(!ExitCode::InputInvalid.should_fallback());
        assert!(!ExitCode::ConfigInvalid.should_fallback());
        assert!(!ExitCode::AppTokenRejected.should_fallback());
        assert!(!ExitCode::UploadServer.should_fallback());
        assert!(!ExitCode::UploadTransport.should_fallback());
        // A size-check FAIL is terminal, NOT a fallback trigger — re-running via
        // the in-language path would skip the gate and not fail the build.
        assert!(!ExitCode::SizeCheckFailed.should_fallback());
    }

    #[test]
    fn numeric_values_match_documented_contract() {
        // Locked-in values — changing any of these is a breaking change for
        // integrators and must be coordinated across all plugins.
        assert_eq!(ExitCode::Success.as_i32(), 0);
        assert_eq!(ExitCode::Unexpected.as_i32(), 1);
        assert_eq!(ExitCode::Usage.as_i32(), 2);
        assert_eq!(ExitCode::InputNotFound.as_i32(), 10);
        assert_eq!(ExitCode::InputInvalid.as_i32(), 11);
        assert_eq!(ExitCode::ConfigInvalid.as_i32(), 20);
        assert_eq!(ExitCode::AppTokenRejected.as_i32(), 21);
        assert_eq!(ExitCode::UploadServer.as_i32(), 30);
        assert_eq!(ExitCode::UploadTransport.as_i32(), 31);
        assert_eq!(ExitCode::SizeCheckFailed.as_i32(), 40);
    }
}
