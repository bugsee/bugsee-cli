//! Legacy presigned-URL upload protocol.
//!
//! Stage 1: `POST {endpoint}/apps/{app_token}/symbols` with metadata JSON
//!          (`{uuid, version, build, hash, transform?}`). Server responds with
//!          either `{code: 0, endpoint: <presigned PUT URL>}` (proceed) or
//!          `{code: 16004}` (already exists, skip), or an error.
//! Stage 2: `PUT <presigned URL>` with the binary body.
//!
//! Wire format already implemented identically across the existing
//! Kotlin (Gradle plugin), Python (BugseeAgent + Flutter), C# (Bugsee.Symbols),
//! JS (bugsee-sourcemaps), and shell (upload-native-symbols.sh) clients.
//! This module consolidates them.
//!
//! Status code policy mirrors the existing `SymbolUploader` (Kotlin): the
//! full 2xx range is accepted on both legs because S3 / CDN proxies return
//! 201/202/204 interchangeably depending on the storage class and whether
//! the request was multipart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Server-side already-exists sentinel returned in the metadata POST body.
const CODE_ALREADY_EXISTS: i64 = 16004;

/// Telemetry header attached to the metadata POST so the backend can count
/// CLI usage vs. legacy in-language fallback usage during the rollout.
/// The fallback (Kotlin / Python / etc.) emits a different value of the same
/// header so the two paths are distinguishable server-side without touching
/// customer code. See [[bugsee_cli_rollout_paradigm]] for the sunset plan.
const TELEMETRY_HEADER: &str = "X-Bugsee-Uploader";
const TELEMETRY_VALUE: &str = "cli";

/// Metadata POST body. Field names MUST match the wire format the worker
/// has been receiving — every existing uploader emits these exact keys.
///
/// Field absence reflects per-platform reality:
///   - ProGuard mapping (Android): sends `uuid` (Java-UUID hash) + `hash`
///     (SHA-1) so the server can dedup; `transform` absent.
///   - Native ELF (Android NDK): sends `uuid` + `hash` + `transform =
///     "breakpad"`.
///   - dSYM (iOS): sends ONLY `version` + `build`. The server extracts the
///     Mach-O UUIDs from the uploaded zip itself (one per arch slice);
///     dedup is client-side in BugseeAgent's `~/.bugseeUploadList`, which
///     this CLI does not yet re-implement.
#[derive(Debug, Clone, Serialize)]
pub struct Metadata<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<&'a str>,
    pub version: &'a str,
    pub build: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<&'a str>,

    /// Only set for native ELF / Breakpad uploads (value `"breakpad"`).
    /// Absent for ProGuard mappings, dSYMs, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<&'a str>,
}

/// Outcome of a successful upload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// File was uploaded to the presigned URL.
    Uploaded,
    /// Server already had this artifact (matched on hash); upload skipped.
    AlreadyExists,
}

#[derive(Debug, Deserialize)]
struct MetadataResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    error: Option<ErrorPayload>,
}

#[derive(Debug, Deserialize)]
struct ErrorPayload {
    #[serde(default, rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

/// Build a reqwest client configured for symbol upload.
pub fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("bugsee-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(Error::from)
}

/// Run the two-stage presigned upload for a single symbol artifact.
pub async fn upload(
    client: &reqwest::Client,
    endpoint: &str,
    app_token: &str,
    metadata: &Metadata<'_>,
    payload: &Path,
) -> Result<Outcome> {
    let metadata_url = format!(
        "{}/apps/{}/symbols",
        endpoint.trim_end_matches('/'),
        app_token
    );

    tracing::debug!(url = %metadata_url, ?metadata, "POST metadata");
    // Telemetry header lands on the POST only — the presigned PUT goes to S3,
    // whose signature is bound to a specific header set; adding extras there
    // would trigger SignatureDoesNotMatch.
    let post_resp = client
        .post(&metadata_url)
        .header(TELEMETRY_HEADER, TELEMETRY_VALUE)
        .json(metadata)
        .send()
        .await
        .map_err(|e| Error::UploadTransport(e.to_string()))?;

    let status = post_resp.status();
    let body_text = post_resp
        .text()
        .await
        .map_err(|e| Error::UploadTransport(format!("reading metadata response body: {e}")))?;

    if !status.is_success() {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: truncate_for_log(&body_text, 512),
        });
    }

    let parsed: MetadataResponse =
        serde_json::from_str(&body_text).map_err(|e| Error::UploadServer {
            status: status.as_u16(),
            message: format!(
                "response body was not valid JSON: {e} — body preview: {}",
                truncate_for_log(&body_text, 200),
            ),
        })?;

    if parsed.code == Some(CODE_ALREADY_EXISTS) {
        tracing::debug!("server reports SymbolAlreadyExists ({CODE_ALREADY_EXISTS})");
        return Ok(Outcome::AlreadyExists);
    }

    let presigned = match parsed.endpoint.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => {
            if let Some(err) = parsed.error {
                let kind = err.error_type.as_deref().unwrap_or("unknown");
                let msg = err.message.unwrap_or_else(|| "(no message)".into());
                if kind == "ApplicationNotFoundError" {
                    return Err(Error::AppTokenRejected);
                }
                return Err(Error::UploadServer {
                    status: status.as_u16(),
                    message: format!("server returned error: type={kind} message={msg}"),
                });
            }
            return Err(Error::UploadServer {
                status: status.as_u16(),
                message: "metadata response had no presigned endpoint and no error payload".into(),
            });
        }
    };

    tracing::debug!(presigned_url = %presigned, "PUT payload");
    let payload_bytes = tokio::fs::read(PathBuf::from(payload)).await?;
    let put_resp = client
        .put(presigned)
        .body(payload_bytes)
        .send()
        .await
        .map_err(|e| Error::UploadTransport(format!("presigned PUT failed: {e}")))?;

    let put_status = put_resp.status();
    if !put_status.is_success() {
        let body = put_resp.text().await.unwrap_or_default();
        return Err(Error::UploadServer {
            status: put_status.as_u16(),
            message: truncate_for_log(&body, 512),
        });
    }

    Ok(Outcome::Uploaded)
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
