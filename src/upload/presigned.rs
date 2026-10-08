//! Legacy presigned-URL upload protocol (symbol files).
//!
//! Stage 1: `POST {endpoint}/apps/{app_token}/symbols` with metadata JSON
//!          (`{uuid, version, build, hash, transform?}`). Server responds with
//!          either `{code: 0, endpoint: <presigned PUT URL>}` (proceed), the
//!          already-exists sentinel (skip — see [`is_already_exists`]), or an
//!          error envelope `{ok: false, error: {type, message, code}}`.
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

/// Server-side already-exists sentinel: `DuplicateSymbolsFoundError`, whose
/// code is the symbol-error offset 16000 + 4 (appserver `errors/symbol`).
const CODE_ALREADY_EXISTS: i64 = 16004;
const TYPE_ALREADY_EXISTS: &str = "DuplicateSymbolsFoundError";

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

    /// Explicit SymbolFile format (e.g. `il2cpp-linemap`). When absent the
    /// worker infers format from zip contents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'a str>,

    /// dSYM: the Mach-O slice UUIDs declared up front so the server can dedup
    /// BEFORE signing an upload URL — when every UUID is already present it
    /// responds with `DuplicateSymbolsFoundError` (code 16004) and the PUT is
    /// skipped. (`Outcome::AlreadyExists`.)
    ///
    /// Also used by `il2cpp-linemap` for multi-ABI module UUID lists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uuids: Option<&'a [String]>,

    /// `--force`: ask the server to sign an upload URL even when it already
    /// has these symbols (maps to the server's `overwrite`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,

    /// Native ELF only: what this file can symbolicate — `"symtab"` (symbol
    /// table, function names) or `"dwarf"` (debug info, file:line). The server
    /// stores it so a later upload of the same build-id can be compared with it.
    /// Absent for a library that carries neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format_variant: Option<&'a str>,

    /// Native ELF only: replace the stored copy when this file is richer
    /// (`symtab` -> `dwarf`), never otherwise. Unlike `overwrite` it does not
    /// re-transfer a library the server already holds at the same or better level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replace_if_richer: Option<bool>,
}

/// What a stored native symbol can symbolicate (the server's `format_variant`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Symbol table only (AGP `SYMBOL_TABLE`): function names, no file:line.
    Symtab,
    /// Full debug info.
    Dwarf,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Symtab => "symtab",
            Variant::Dwarf => "dwarf",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "symtab" => Some(Variant::Symtab),
            "dwarf" => Some(Variant::Dwarf),
            _ => None,
        }
    }
}

/// An upload attempt with the detail [`Outcome`] leaves out. Only the native ELF
/// flow reads it; every other flow uses [`upload`] and the plain `Outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadDetail {
    pub outcome: Outcome,
    /// Uploaded AND replaced a poorer stored copy (`replace_if_richer`).
    pub upgraded: bool,
    /// On `AlreadyExists`: what the server holds, when it says. `None` means it
    /// did not say — an older server, or a stored copy that predates the field.
    pub stored: Option<Variant>,
}

/// Outcome of a successful upload attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// File was uploaded to the presigned URL.
    Uploaded,
    /// Server already had this artifact; upload skipped. The server matches on
    /// the declared uuid(s) and format among READY records — not on `hash`.
    AlreadyExists,
}

#[derive(Debug, Deserialize)]
struct MetadataResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    endpoint: Option<String>,
    /// Set when the upload replaces a poorer stored copy (`replace_if_richer`).
    #[serde(default)]
    upgraded: Option<bool>,
    #[serde(default)]
    error: Option<ErrorPayload>,
}

#[derive(Debug, Deserialize)]
struct ErrorPayload {
    #[serde(default, rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// Kept loosely typed: only its numeric value is ever compared, and a
    /// non-integer here must not fail the parse of an otherwise readable error.
    #[serde(default)]
    code: Option<serde_json::Value>,
    /// On a duplicate, the server's `{format_variant}` for the stored copy.
    #[serde(default)]
    params: Option<ErrorParams>,
}

#[derive(Debug, Deserialize)]
struct ErrorParams {
    #[serde(default)]
    format_variant: Option<String>,
}

/// Whether the metadata response says the server already has this symbol.
///
/// The appserver answers a duplicate with HTTP 200 (`code/routing/routers/
/// error.router.js` `onError`) and the code NESTED in the envelope that
/// `code/app.utils.js` `error()` builds —
/// `{ok: false, error: {type: "DuplicateSymbolsFoundError", code: 16004}}` —
/// so the nested code and the error type are both checked. A top-level
/// `code: 16004` is accepted too: it is what this client originally matched on,
/// and the check costs nothing. Matching ONLY the top level is what made every
/// re-upload of an unchanged artifact a hard failure that aborted the batch.
fn is_already_exists(parsed: &MetadataResponse) -> bool {
    parsed.code == Some(CODE_ALREADY_EXISTS)
        || parsed.error.as_ref().is_some_and(|e| {
            e.code.as_ref().and_then(serde_json::Value::as_i64) == Some(CODE_ALREADY_EXISTS)
                || e.error_type.as_deref() == Some(TYPE_ALREADY_EXISTS)
        })
}

/// Outcome of the metadata POST (stage 1). Either the server already has the
/// symbol (skip the PUT — no need to even read/pack the payload) or it signed a
/// presigned URL to PUT the bytes to.
#[derive(Debug)]
pub enum Registration {
    /// Server already has these symbols (`DuplicateSymbolsFoundError`, 16004).
    /// `stored` is what it holds, when it says.
    AlreadyExists { stored: Option<Variant> },
    /// Proceed: PUT the payload to this presigned URL. `upgraded` is set when the
    /// upload replaces a poorer stored copy.
    Proceed {
        presigned_url: String,
        upgraded: bool,
    },
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

    tracing::debug!(url = %http::redact_url(&metadata_url), ?metadata, "POST metadata");
    // The POST is NOT retried on a retriable status (the server may have created
    // the symbol record, so a status-retry could double-create); transport
    // retries are safe for a symbol that already reached `ready` — the server
    // dedups by declared uuid + format among READY records (not by `hash`), so a
    // retry landing while the first attempt's record is still `uploading` can
    // leave a second record. Telemetry header lands
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

    if is_already_exists(&parsed) {
        tracing::debug!("server reports SymbolAlreadyExists ({CODE_ALREADY_EXISTS})");
        let stored = parsed
            .error
            .as_ref()
            .and_then(|e| e.params.as_ref())
            .and_then(|p| p.format_variant.as_deref())
            .and_then(Variant::parse);
        return Ok(Registration::AlreadyExists { stored });
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
        upgraded: parsed.upgraded == Some(true),
    })
}

/// Stage 2: PUT the payload bytes to a presigned URL from [`register`].
pub async fn put_payload(
    client: &reqwest::Client,
    policy: RetryPolicy,
    presigned_url: &str,
    payload: &Path,
) -> Result<()> {
    tracing::debug!(url = %http::redact_url(presigned_url), "PUT payload");
    // Stream the payload from disk: reading it whole (and cloning it per retry
    // attempt) held two-plus copies of the archive in memory per upload. The
    // length is sent explicitly because a streamed body would otherwise go out
    // chunked, which a presigned S3 PUT rejects.
    // The PUT is idempotent (overwrite of the same key) — retry on transport
    // AND retriable status. Each attempt re-opens the file from the start.
    let put_resp =
        http::put_file(client, policy, "symbol PUT", presigned_url, payload, None).await?;

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
    Ok(
        upload_detailed(client, policy, endpoint, app_token, metadata, payload)
            .await?
            .outcome,
    )
}

/// [`upload`], also reporting whether it replaced a poorer stored copy and what
/// the server said it already held.
pub async fn upload_detailed(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    metadata: &Metadata<'_>,
    payload: &Path,
) -> Result<UploadDetail> {
    match register(client, policy, endpoint, app_token, metadata).await? {
        Registration::AlreadyExists { stored } => Ok(UploadDetail {
            outcome: Outcome::AlreadyExists,
            upgraded: false,
            stored,
        }),
        Registration::Proceed {
            presigned_url,
            upgraded,
        } => {
            put_payload(client, policy, &presigned_url, payload).await?;
            Ok(UploadDetail {
                outcome: Outcome::Uploaded,
                upgraded,
                stored: None,
            })
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
            format: None,
            uuids: Some(&uuids),
            overwrite: Some(true),
            format_variant: None,
            replace_if_richer: None,
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
            format: None,
            uuids: None,
            overwrite: None,
            format_variant: None,
            replace_if_richer: None,
        };
        let v2 = serde_json::to_value(&m2).unwrap();
        assert!(v2.get("uuids").is_none());
        assert!(v2.get("overwrite").is_none());
    }

    #[test]
    fn metadata_serializes_il2cpp_linemap_format() {
        let uuids = vec!["deadbeef".to_string(), "cafebabe".to_string()];
        let m = Metadata {
            uuid: Some("deadbeef"),
            version: "1.0",
            build: "42",
            hash: Some("abc"),
            transform: None,
            format: Some("il2cpp-linemap"),
            uuids: Some(&uuids),
            overwrite: Some(true),
            format_variant: None,
            replace_if_richer: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["format"], "il2cpp-linemap");
        assert_eq!(v["uuids"], serde_json::json!(["deadbeef", "cafebabe"]));
        assert_eq!(v["overwrite"], true);
    }

    fn elf_metadata<'a>(variant: Option<&'a str>, overwrite: Option<bool>) -> Metadata<'a> {
        Metadata {
            uuid: Some("bca64abfec40dbb631bb8f1c37414472"),
            version: "1",
            build: "1",
            hash: Some("h"),
            transform: Some("breakpad"),
            format: Some("elf"),
            uuids: None,
            overwrite,
            format_variant: variant,
            replace_if_richer: Some(true),
        }
    }

    #[test]
    fn metadata_serializes_the_richness_pair_and_omits_it_when_unset() {
        let v = serde_json::to_value(elf_metadata(Some("dwarf"), None)).unwrap();
        assert_eq!(v["format_variant"], "dwarf");
        assert_eq!(v["replace_if_richer"], true);
        assert!(v.get("overwrite").is_none());

        // A library with neither debug info nor a symbol table declares no variant.
        let v = serde_json::to_value(elf_metadata(None, None)).unwrap();
        assert!(v.get("format_variant").is_none(), "{v}");
        assert_eq!(v["replace_if_richer"], true);
    }

    /// Every other flow leaves both fields off the wire entirely.
    #[test]
    fn metadata_without_the_richness_pair_is_unchanged_on_the_wire() {
        let mut m = elf_metadata(None, None);
        m.replace_if_richer = None;
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("format_variant").is_none());
        assert!(v.get("replace_if_richer").is_none());
    }

    async fn post_replying(body: serde_json::Value) -> (MockServer, reqwest::Client) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        (server, http::build_client().unwrap())
    }

    #[tokio::test]
    async fn register_reports_an_upgrade_when_the_server_says_so() {
        let (server, client) = post_replying(serde_json::json!({
            "code": 0, "endpoint": "https://s3.example/put", "upgraded": true
        }))
        .await;
        let reg = register(
            &client,
            RetryPolicy::none(),
            &server.uri(),
            "TKN",
            &elf_metadata(Some("dwarf"), None),
        )
        .await
        .unwrap();
        assert!(
            matches!(&reg, Registration::Proceed { upgraded: true, .. }),
            "{reg:?}"
        );
    }

    #[tokio::test]
    async fn register_is_not_an_upgrade_when_the_server_does_not_say_so() {
        for body in [
            serde_json::json!({"code": 0, "endpoint": "https://s3.example/put"}),
            serde_json::json!({"code": 0, "endpoint": "https://s3.example/put", "upgraded": false}),
        ] {
            let (server, client) = post_replying(body).await;
            let reg = register(
                &client,
                RetryPolicy::none(),
                &server.uri(),
                "TKN",
                &elf_metadata(Some("dwarf"), None),
            )
            .await
            .unwrap();
            assert!(
                matches!(
                    &reg,
                    Registration::Proceed {
                        upgraded: false,
                        ..
                    }
                ),
                "{reg:?}"
            );
        }
    }

    #[tokio::test]
    async fn register_reads_the_stored_variant_off_a_duplicate_reply() {
        for (reported, expected) in [
            (Some("dwarf"), Some(Variant::Dwarf)),
            (Some("symtab"), Some(Variant::Symtab)),
            // A value this client does not know is "not said", never a guess.
            (Some("future-kind"), None),
            (None, None),
        ] {
            let mut error =
                serde_json::json!({"type": "DuplicateSymbolsFoundError", "code": 16004});
            if let Some(v) = reported {
                error["params"] = serde_json::json!({ "format_variant": v });
            }
            let (server, client) =
                post_replying(serde_json::json!({"ok": false, "error": error})).await;
            let reg = register(
                &client,
                RetryPolicy::none(),
                &server.uri(),
                "TKN",
                &elf_metadata(Some("symtab"), None),
            )
            .await
            .unwrap();
            assert!(
                matches!(&reg, Registration::AlreadyExists { stored } if *stored == expected),
                "reported {reported:?}: {reg:?}"
            );
        }
    }

    #[tokio::test]
    async fn upload_detailed_carries_the_upgrade_and_the_stored_variant() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = tmp.path().join("p.zip");
        std::fs::write(&payload, b"zip").unwrap();

        let server = MockServer::start().await;
        let put_url = format!("{}/put", server.uri());
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"code": 0, "endpoint": put_url, "upgraded": true}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let client = http::build_client().unwrap();
        let detail = upload_detailed(
            &client,
            RetryPolicy::none(),
            &server.uri(),
            "TKN",
            &elf_metadata(Some("dwarf"), None),
            &payload,
        )
        .await
        .unwrap();
        assert_eq!(detail.outcome, Outcome::Uploaded);
        assert!(detail.upgraded);
        assert_eq!(detail.stored, None);
    }

    #[tokio::test]
    async fn register_sends_il2cpp_linemap_format_on_wire() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false,
                "error": { "type": "DuplicateSymbolsFoundError", "code": 16004 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let uuids = vec!["aaa".to_string(), "bbb".to_string()];
        let m = Metadata {
            uuid: Some("aaa"),
            version: "1",
            build: "1",
            hash: Some("h"),
            transform: None,
            format: Some("il2cpp-linemap"),
            uuids: Some(&uuids),
            overwrite: None,
            format_variant: None,
            replace_if_richer: None,
        };
        let client = http::build_client().unwrap();
        let reg = register(&client, RetryPolicy::none(), &server.uri(), "TKN", &m)
            .await
            .unwrap();
        assert!(matches!(reg, Registration::AlreadyExists { .. }));

        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["format"], "il2cpp-linemap");
        assert_eq!(body["uuids"], serde_json::json!(["aaa", "bbb"]));
    }

    #[tokio::test]
    async fn register_returns_already_exists_on_16004_and_sends_uuids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false,
                "error": { "type": "DuplicateSymbolsFoundError", "code": 16004 }
            })))
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
            format: None,
            uuids: Some(&uuids),
            overwrite: None,
            format_variant: None,
            replace_if_richer: None,
        };
        let client = http::build_client().unwrap();
        let reg = register(&client, RetryPolicy::none(), &server.uri(), "TKN", &m)
            .await
            .unwrap();
        assert!(matches!(reg, Registration::AlreadyExists { .. }));

        // The metadata POST carried the declared UUIDs (the dedup key).
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
        assert_eq!(body["uuids"], serde_json::json!(["aaaa"]));
    }

    /// Minimal metadata for the register/upload response-shape tests.
    fn plain_metadata() -> Metadata<'static> {
        Metadata {
            uuid: Some("did-1"),
            version: "1",
            build: "1",
            hash: Some("h"),
            transform: None,
            format: Some("sourcemap"),
            uuids: None,
            overwrite: None,
            format_variant: None,
            replace_if_richer: None,
        }
    }

    async fn register_against(body: serde_json::Value) -> Result<Registration> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let client = http::build_client().unwrap();
        register(
            &client,
            RetryPolicy::none(),
            &server.uri(),
            "TKN",
            &plain_metadata(),
        )
        .await
    }

    /// The appserver's real duplicate answer: HTTP 200 (`error.router.js`
    /// `onError`) with the code NESTED inside `error` by `app.utils.js` `error()`
    /// — `code: err.offset + err.code` = 16000 + 4 — never at the top level.
    /// Matching only a top-level `code` made every re-upload of an unchanged
    /// artifact a hard failure (exit 30) that aborted the whole batch.
    #[tokio::test]
    async fn register_recognises_the_appservers_nested_duplicate_envelope() {
        let reg = register_against(serde_json::json!({
            "ok": false,
            "error": {
                "type": "DuplicateSymbolsFoundError",
                "message": "A symbol file with the same identifier already exists",
                "code": 16004
            }
        }))
        .await
        .unwrap();
        assert!(
            matches!(reg, Registration::AlreadyExists { .. }),
            "got {reg:?}"
        );
    }

    #[tokio::test]
    async fn register_recognises_a_duplicate_by_nested_code_alone() {
        let reg = register_against(serde_json::json!({
            "ok": false,
            "error": { "code": 16004 }
        }))
        .await
        .unwrap();
        assert!(
            matches!(reg, Registration::AlreadyExists { .. }),
            "got {reg:?}"
        );
    }

    #[tokio::test]
    async fn register_recognises_a_duplicate_by_error_type_alone() {
        let reg = register_against(serde_json::json!({
            "ok": false,
            "error": { "type": "DuplicateSymbolsFoundError" }
        }))
        .await
        .unwrap();
        assert!(
            matches!(reg, Registration::AlreadyExists { .. }),
            "got {reg:?}"
        );
    }

    #[tokio::test]
    async fn register_still_fails_on_any_other_symbol_error() {
        // A sibling code from the same error family (SymbolNotFoundError = 16001)
        // must not be mistaken for "already there".
        let err = register_against(serde_json::json!({
            "ok": false,
            "error": {
                "type": "SymbolNotFoundError",
                "message": "Symbol file not found",
                "code": 16001
            }
        }))
        .await
        .unwrap_err();
        match err {
            Error::UploadServer { status, message } => {
                assert_eq!(status, 200);
                assert!(message.contains("SymbolNotFoundError"), "{message}");
            }
            other => panic!("expected UploadServer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_maps_the_nested_application_not_found_to_token_rejected() {
        let err = register_against(serde_json::json!({
            "ok": false,
            "error": { "type": "ApplicationNotFoundError", "code": 2001 }
        }))
        .await
        .unwrap_err();
        assert!(matches!(err, Error::AppTokenRejected), "got {err:?}");
    }

    /// The envelope's `code` is not ours to type strictly: a proxy or a future
    /// serializer that sends it as a string must not turn a recognisable error
    /// into "response body was not valid JSON" (exit 30 instead of 21).
    #[tokio::test]
    async fn register_tolerates_a_non_numeric_nested_code() {
        let err = register_against(serde_json::json!({
            "ok": false,
            "error": { "type": "ApplicationNotFoundError", "code": "2001" }
        }))
        .await
        .unwrap_err();
        assert!(matches!(err, Error::AppTokenRejected), "got {err:?}");

        let reg = register_against(serde_json::json!({
            "ok": false,
            "error": { "type": "DuplicateSymbolsFoundError", "code": "16004" }
        }))
        .await
        .unwrap();
        assert!(
            matches!(reg, Registration::AlreadyExists { .. }),
            "got {reg:?}"
        );
    }

    /// A payload bigger than the 64 KiB stream chunk and not a multiple of it.
    fn patterned_payload(len: usize) -> (tempfile::NamedTempFile, Vec<u8>) {
        use std::io::Write;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&data).unwrap();
        (f, data)
    }

    /// The PUT streams the file but must still go out with an exact
    /// `Content-Length` (a presigned S3 PUT rejects a chunked body) and every byte.
    #[tokio::test]
    async fn put_payload_streams_the_whole_file_with_an_exact_content_length() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let (payload, data) = patterned_payload(300_001);
        let client = http::build_client().unwrap();
        put_payload(
            &client,
            RetryPolicy::none(),
            &format!("{}/put", server.uri()),
            payload.path(),
        )
        .await
        .unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(
            reqs[0]
                .headers
                .get("content-length")
                .map(|v| v.to_str().unwrap().to_owned()),
            Some(data.len().to_string())
        );
        assert!(
            reqs[0].headers.get("transfer-encoding").is_none(),
            "must not be chunked"
        );
        assert_eq!(reqs[0].body, data);
    }

    /// Each retry re-opens the file: the second attempt must carry the full body,
    /// not whatever a shared cursor had left over.
    #[tokio::test]
    async fn put_payload_retry_resends_the_complete_body() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (payload, data) = patterned_payload(200_000);
        let client = http::build_client().unwrap();
        put_payload(
            &client,
            RetryPolicy::fast(3),
            &format!("{}/put", server.uri()),
            payload.path(),
        )
        .await
        .unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2, "one 503 then one success");
        for r in &reqs {
            assert_eq!(r.body, data, "every attempt sends the whole payload");
        }
    }

    /// A payload that vanished is a reported error, not a panic or a hang.
    /// A freshly created S3 bucket answers the presigned URL with a temporary 307 to the
    /// regional endpoint; the symbol PUT must follow it (a streamed body is not replayed by
    /// reqwest on its own).
    #[tokio::test]
    async fn put_payload_follows_a_temporary_redirect() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/bucket/key"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("Location", format!("{}/regional/key", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/regional/key"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let (payload, data) = patterned_payload(150_001);
        let client = http::build_client().unwrap();
        put_payload(
            &client,
            RetryPolicy::none(),
            &format!("{}/bucket/key", server.uri()),
            payload.path(),
        )
        .await
        .unwrap();
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1].body, data);
    }

    #[tokio::test]
    async fn put_payload_reports_a_missing_payload_file() {
        let server = MockServer::start().await;
        let client = http::build_client().unwrap();
        let err = put_payload(
            &client,
            RetryPolicy::none(),
            &format!("{}/put", server.uri()),
            Path::new("/nonexistent/payload.zip"),
        )
        .await
        .unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[tokio::test]
    async fn upload_skips_the_put_when_the_server_reports_the_nested_duplicate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/TKN/symbols"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false,
                "error": { "type": "DuplicateSymbolsFoundError", "code": 16004 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let payload = tempfile::NamedTempFile::new().unwrap();
        let client = http::build_client().unwrap();
        let outcome = upload(
            &client,
            RetryPolicy::none(),
            &server.uri(),
            "TKN",
            &plain_metadata(),
            payload.path(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::AlreadyExists);
    }

    /// The shape this client first matched on, and which no current appserver
    /// route sends. Still accepted — the check is free — but pinned under its own
    /// name so it is not mistaken for the real reply.
    #[tokio::test]
    async fn register_still_accepts_a_legacy_top_level_16004() {
        let reg = register_against(serde_json::json!({ "code": 16004 }))
            .await
            .unwrap();
        assert!(
            matches!(reg, Registration::AlreadyExists { .. }),
            "got {reg:?}"
        );
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
            format: None,
            uuids: None,
            overwrite: None,
            format_variant: None,
            replace_if_richer: None,
        };
        let client = http::build_client().unwrap();
        match register(&client, RetryPolicy::none(), &server.uri(), "TKN", &m)
            .await
            .unwrap()
        {
            Registration::Proceed { presigned_url, .. } => {
                assert_eq!(presigned_url, "https://s3.example/put")
            }
            other => panic!("expected Proceed, got {other:?}"),
        }
    }
}
