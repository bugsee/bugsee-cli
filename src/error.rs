use thiserror::Error;

use crate::exit_code::ExitCode;

/// Errors that map cleanly to the exit-code contract. Producing code wraps
/// these in `anyhow::Error` (so the dispatch chain stays `anyhow::Result`);
/// `main` downcasts and classifies. Errors that aren't one of these variants
/// fall through to `ExitCode::Unexpected`.
#[derive(Debug, Error)]
pub enum Error {
    /// Input path doesn't exist, isn't readable, or there's nothing matching
    /// the requested type under it.
    #[error("input not found: {0}")]
    InputNotFound(String),

    /// Input was found but couldn't be parsed / is the wrong shape for its
    /// declared type. Reserved for parser failures (dSYM, ELF, PDB) once
    /// those land.
    #[error("input invalid: {0}")]
    InputInvalid(String),

    /// User-supplied configuration is wrong (missing required flag,
    /// incompatible flag combination, malformed value).
    #[error("configuration error: {0}")]
    ConfigInvalid(String),

    /// Server rejected the app token (responded with
    /// `error.type == "ApplicationNotFoundError"`). Distinct from generic
    /// server errors so integrators can surface a targeted message.
    #[error(
        "app token rejected by server (ApplicationNotFoundError) — \
         verify BUGSEE_APP_TOKEN matches the project"
    )]
    AppTokenRejected,

    /// Server returned a non-success status or otherwise rejected the
    /// upload (4xx, 5xx, malformed response body).
    #[error("upload failed: server responded with status {status} — {message}")]
    UploadServer { status: u16, message: String },

    /// Transport-level upload failure (DNS, connect, TLS, body stream,
    /// timeout). Distinguished from `UploadServer` because retry strategy
    /// differs.
    #[error("upload failed: {0}")]
    UploadTransport(String),

    /// I/O passthrough. Classified as `InputNotFound` since the most common
    /// cause is a missing or unreadable file; permission errors and broken
    /// pipes also land here and carry their underlying messages.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP-client passthrough. Classified as `UploadTransport`.
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// A deliberate build gate failed (size-check threshold crossed). The
    /// `message` is already the user-facing gate line (it carries its own
    /// context), so it is rendered verbatim — `main` prints `error: {message}`,
    /// which Xcode surfaces in the build log + Report navigator.
    #[error("{0}")]
    SizeCheckFailed(String),
}

impl Error {
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Error::InputNotFound(_) | Error::Io(_) => ExitCode::InputNotFound,
            Error::InputInvalid(_) => ExitCode::InputInvalid,
            Error::ConfigInvalid(_) => ExitCode::ConfigInvalid,
            Error::AppTokenRejected => ExitCode::AppTokenRejected,
            Error::UploadServer { .. } => ExitCode::UploadServer,
            Error::UploadTransport(_) | Error::Http(_) => ExitCode::UploadTransport,
            Error::SizeCheckFailed(_) => ExitCode::SizeCheckFailed,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

// Convenience constructors so dispatch sites can `return Err(input_invalid(...).into())`
// without spelling out the variant + .into() chain every time.

pub fn input_not_found<S: Into<String>>(msg: S) -> anyhow::Error {
    Error::InputNotFound(msg.into()).into()
}

pub fn input_invalid<S: Into<String>>(msg: S) -> anyhow::Error {
    Error::InputInvalid(msg.into()).into()
}

pub fn config_invalid<S: Into<String>>(msg: S) -> anyhow::Error {
    Error::ConfigInvalid(msg.into()).into()
}

/// Classify an anyhow error into an exit code. Unknown errors → `Unexpected`.
pub fn classify(err: &anyhow::Error) -> ExitCode {
    err.downcast_ref::<Error>()
        .map(Error::exit_code)
        .unwrap_or(ExitCode::Unexpected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_variant_maps_to_documented_code() {
        assert_eq!(
            Error::InputNotFound("x".into()).exit_code(),
            ExitCode::InputNotFound
        );
        assert_eq!(
            Error::InputInvalid("x".into()).exit_code(),
            ExitCode::InputInvalid
        );
        assert_eq!(
            Error::ConfigInvalid("x".into()).exit_code(),
            ExitCode::ConfigInvalid
        );
        assert_eq!(
            Error::AppTokenRejected.exit_code(),
            ExitCode::AppTokenRejected
        );
        assert_eq!(
            Error::UploadServer {
                status: 500,
                message: "x".into()
            }
            .exit_code(),
            ExitCode::UploadServer,
        );
        assert_eq!(
            Error::UploadTransport("x".into()).exit_code(),
            ExitCode::UploadTransport
        );
        assert_eq!(
            Error::SizeCheckFailed("grew too much".into()).exit_code(),
            ExitCode::SizeCheckFailed
        );
    }

    #[test]
    fn size_check_failed_displays_message_verbatim() {
        // `main` prints `error: {Display}`; the message must NOT be wrapped so
        // Xcode parses the `error: Bugsee size check: ...` line cleanly.
        let e = Error::SizeCheckFailed("Bugsee size check: grew".into());
        assert_eq!(format!("{e}"), "Bugsee size check: grew");
    }

    #[test]
    fn classify_anyhow_downcasts_our_errors() {
        let e: anyhow::Error = config_invalid("missing --version");
        assert_eq!(classify(&e), ExitCode::ConfigInvalid);

        let plain: anyhow::Error = anyhow::anyhow!("some untyped error");
        assert_eq!(classify(&plain), ExitCode::Unexpected);
    }
}
