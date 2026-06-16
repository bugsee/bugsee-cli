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

    /// dSYM: the Mach-O slice UUIDs declared up front so the server can dedup
    /// BEFORE signing an upload URL — when every UUID is already present it
    /// responds with `DuplicateSymbolsFoundError` (code 16004) and the PUT is
    /// skipped. (`Outcome::AlreadyExists`.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuids: Option<&'a [String]>,

    /// `--force`: ask the server to sign an upload URL even when it already
    /// has these symbols (maps to the server's `overwrite`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
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

/// Outcome of the metadata POST (stage 1). Either the server already has the
/// symbol (skip the PUT — no need to even read/pack the payload) or it signed a
/// presigned URL to PUT the bytes to.
#[derive(Debug)]
pub enum Registration {
    /// Server already has these symbols (`DuplicateSymbolsFoundError`, 16004).
    AlreadyExists,
    /// Proceed: PUT the payload to this presigned URL.
    Proceed { presigned_url: String },
}

/// Stage 1: POST the metadata and interpret the response. Lets the caller dedup
/// BEFORE producing the payload — the dSYM flow uses this to avoid packing a
/// large bundle the server already has.
pub async fn register(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    metadata: &Metadata<'_>,
) -> Result<Registration> {
    let metadata_url = format!(
        "{}/apps/{}/symbols",
        endpoint.trim_end_matches('/'),
        app_token
    );

    tracing::debug!(url = %metadata_url, ?metadata, "POST metadata");
    // The POST is NOT retried on a retriable status (the server may have created
    // the symbol record, so a status-retry could double-create); transport
    // retries are safe — the server dedups by hash/uuid. Telemetry header lands
    // on the POST only — the presigned PUT goes to S3, whose signature is bound
    // to a specific header set; extras there trigger SignatureDoesNotMatch.
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
        return Ok(Registration::AlreadyExists);
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

    Ok(Registration::Proceed {
        presigned_url: presigned,
    })
}

/// Stage 2: PUT the payload bytes to a presigned URL from [`register`].
pub async fn put_payload(
    client: &reqwest::Client,
    policy: RetryPolicy,
    presigned_url: &str,
    payload: &Path,
) -> Result<()> {
    tracing::debug!(presigned_url, "PUT payload");
    let payload_bytes = tokio::fs::read(payload).await?;
    // The PUT is idempotent (overwrite of the same key) — retry on transport
    // AND retriable status.
    let put_resp = http::send_with_retry(policy, "symbol PUT", true, || {
        client.put(presigned_url).body(payload_bytes.clone())
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

    Ok(())
}

/// Run the two-stage presigned upload for a single symbol artifact
/// ([`register`] then [`put_payload`]). The payload is always produced up
/// front; callers that want to skip producing it when the server already has
/// the symbol should call [`register`] / [`put_payload`] directly.
pub async fn upload(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    metadata: &Metadata<'_>,
    payload: &Path,
) -> Result<Outcome> {
    match register(client, policy, endpoint, app_token, metadata).await? {
        Registration::AlreadyExists => Ok(Outcome::AlreadyExists),
        Registration::Proceed { presigned_url } => {
            put_payload(client, policy, &presigned_url, payload).await?;
            Ok(Outcome::Uploaded)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn metadata_serializes_uuids_and_overwrite_omitting_none() {
        let uuids = vec!["aaaa".to_string(), "bbbb".to_string()];
        let m = Metadata {
            uuid: None,
            version: "1",
            build: "2",
            hash: None,
            transform: None,
            uuids: Some(&uuids),
            overwrite: Some(true),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["uuids"], serde_json::json!(["aaaa", "bbbb"]));
        assert_eq!(v["overwrite"], serde_json::json!(true));
        assert_eq!(v["version"], "1");
        // None fields are omitted from the wire body.
        assert!(v.get("uuid").is_none());
        assert!(v.get("hash").is_none());
        assert!(v.get("transform").is_none());

        // The non-dSYM path leaves both new fields off entirely.
        let m2 = Metadata {
            uuid: Some("x"),
            version: "1",
            build: "2",
            hash: Some("h"),
            transform: None,
            uuids: None,
            overwrite: None,
        };
        let v2 = serde_json::to_value(&m2).unwrap();
        assert!(v2.get("uuids").is_none());
        assert!(v2.get("overwrite").is_none());
    }

    #[tokio::test]
    async fn register_returns_already_exists_on_16004_and_sends_uuids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "code": 16004 })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let uuids = vec!["aaaa".to_string()];
        let m = Metadata {
            uuid: None,
            version: "1",
            build: "1",
            hash: None,
            transform: None,
            uuids: Some(&uuids),
            overwrite: None,
        };
        let client = http::build_client().unwrap();
        let reg = register(&client, RetryPolicy::none(), &server.uri(), "TKN", &m)
            .await
            .unwrap();
        assert!(matches!(reg, Registration::AlreadyExists));

        // The metadata POST carried the declared UUIDs (the dedup key).
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["uuids"], serde_json::json!(["aaaa"]));
    }

    #[tokio::test]
    async fn register_returns_proceed_with_presigned_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "code": 0, "endpoint": "https://s3.example/put" }),
            ))
            .mount(&server)
            .await;

        let m = Metadata {
            uuid: None,
            version: "1",
            build: "1",
            hash: None,
            transform: None,
            uuids: None,
            overwrite: None,
        };
        let client = http::build_client().unwrap();
        match register(&client, RetryPolicy::none(), &server.uri(), "TKN", &m)
            .await
            .unwrap()
        {
            Registration::Proceed { presigned_url } => {
                assert_eq!(presigned_url, "https://s3.example/put")
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }
}
