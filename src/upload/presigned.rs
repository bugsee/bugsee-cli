//! Legacy presigned-URL upload protocol (symbol files).
//!
//! Stage 1: `POST {endpoint}/apps/{app_token}/symbols` with metadata JSON
//!          (`{uuid, version, build, hash, transform?}`). Server responds with
//!          either `{code: 0, endpoint: <presigned PUT URL>}` (proceed) or
//!          `{code: 16004}` (already exists, skip), or an error.
//! Stage 2: `PUT <presigned URL>` with the binary body.
//!
//! Wire format already implemented identically across the existing Kotlin
//! (Gradle plugin), Python (BugseeAgent + Flutter), C# (Bugsee.Symbols), JS
//! (bugsee-sourcemaps), and shell clients. This module consolidates them.
//!
//! Status-code policy mirrors the existing `SymbolUploader` (Kotlin): the full
//! 2xx range is accepted on both legs because S3 / CDN proxies return
//! 201/202/204 interchangeably depending on storage class / multipart.
//!
//! Network I/O (client, telemetry header, retry/backoff, log truncation) flows
//! through the shared [`crate::upload::http`] layer — one HTTP implementation,
//! tested once.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::upload::http::{self, RetryPolicy};

/// Server-side already-exists sentinel returned in the metadata POST body.
const CODE_ALREADY_EXISTS: i64 = 16004;

/// Metadata POST body. Field names MUST match the wire format the worker has
/// been receiving — every existing uploader emits these exact keys.
///
/// Field absence reflects per-platform reality:
///   - ProGuard mapping (Android): `uuid` (Java-UUID hash) + `hash` (SHA-1);
///     `transform` absent.
///   - Native ELF (Android NDK): `uuid` + `hash` + `transform = "breakpad"`.
///   - dSYM (iOS): ONLY `version` + `build`; the server extracts the Mach-O
///     UUIDs from the uploaded zip itself.
#[derive(Debug, Clone, Serialize)]
pub struct Metadata<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuid: Option<&'a str>,
    pub version: &'a str,
    pub build: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<&'a str>,

    /// Only set for native ELF / Breakpad uploads (value `"breakpad"`).
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

/// Run the two-stage presigned upload for a single symbol artifact.
pub async fn upload(
    client: &reqwest::Client,
    policy: RetryPolicy,
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
    // The POST is NOT retried on a retriable status (the server may have created
    // the symbol record, so a status-retry could double-create); transport
    // retries are safe — the server dedups by hash. Telemetry header lands on
    // the POST only — the presigned PUT goes to S3, whose signature is bound to
    // a specific header set; extras there trigger SignatureDoesNotMatch.
    let post_resp = http::send_with_retry(policy, "symbol metadata POST", false, || {
        client
            .post(&metadata_url)
            .header(http::TELEMETRY_HEADER, http::TELEMETRY_VALUE)
            .json(metadata)
    })
    .await?;

    let status = post_resp.status();
    let body_text = post_resp
        .text()
        .await
        .map_err(|e| Error::UploadTransport(format!("reading metadata response body: {e}")))?;

    if !status.is_success() {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: http::truncate_for_log(&body_text, 512),
        });
    }

    let parsed: MetadataResponse =
        serde_json::from_str(&body_text).map_err(|e| Error::UploadServer {
            status: status.as_u16(),
            message: format!(
                "response body was not valid JSON: {e} — body preview: {}",
                http::truncate_for_log(&body_text, 200),
            ),
        })?;

    if parsed.code == Some(CODE_ALREADY_EXISTS) {
        tracing::debug!("server reports SymbolAlreadyExists ({CODE_ALREADY_EXISTS})");
        return Ok(Outcome::AlreadyExists);
    }

    let presigned = match parsed.endpoint.as_deref() {
        Some(s) if !s.is_empty() => s.to_string(),
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
    let payload_bytes = tokio::fs::read(payload).await?;
    // The PUT is idempotent (overwrite of the same key) — retry on transport
    // AND retriable status.
    let put_resp = http::send_with_retry(policy, "symbol PUT", true, || {
        client.put(&presigned).body(payload_bytes.clone())
    })
    .await?;

    let put_status = put_resp.status();
    if !put_status.is_success() {
        let body = put_resp.text().await.unwrap_or_default();
        return Err(Error::UploadServer {
            status: put_status.as_u16(),
            message: http::truncate_for_log(&body, 512),
        });
    }

    Ok(Outcome::Uploaded)
}
