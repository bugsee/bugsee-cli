//! Build-info bundle upload — the design's "Build-info bundle" upload class.
//!
//! Bundles per-build metadata sidecars (`dependencies.json`, `timings.json`,
//! and future additive `*.json`) into ONE zstd ZIP and uploads it with a
//! SINGLE PUT. Two registration modes:
//!
//!   - **self-contained**: POST the producer's metadata to
//!     `/v2/apps/<token>/builds` (with `request_build_info_upload: true`
//!     injected), read back the signed `build_info_upload_endpoint`, then PUT.
//!     Used by producers that have no other reason to register the build (the
//!     iOS BugseeAgents' deps-only flow).
//!   - **pre-signed**: the producer already registered the build (e.g. the
//!     Android Gradle plugin's artefact registration returns the build-info
//!     URL in the same response) and passes it as `--upload-url`; the CLI just
//!     PUTs the bundle, avoiding a second build registration.
//!
//! The producer owns *what* to bundle (which sidecars, the metadata body); the
//! CLI owns *how* (ZIP packing, zstd, the registration handshake, retries,
//! telemetry). All network I/O flows through [`crate::upload::http`].

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::compress::{self, Strategy, ZipEntry};
use crate::error::{Error, Result};
use crate::upload::http::{self, RetryPolicy};

/// Outcome of a build-info upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Bundle packed and PUT to the presigned URL.
    Uploaded,
    /// `--dry-run`: bundle packed but no network I/O performed.
    DryRun,
}

/// One bundle entry: the in-archive name (worker dispatches on it) and the
/// on-disk source file.
pub struct Entry {
    pub name: String,
    pub source: PathBuf,
}

/// Parameters for a build-info upload.
pub struct Params<'a> {
    /// Bugsee API base endpoint (e.g. `https://api.bugsee.com`). Only used for
    /// the self-contained registration POST.
    pub endpoint: &'a str,
    /// App token — required for the self-contained path; unused with `upload_url`.
    pub app_token: Option<&'a str>,
    /// Registration metadata JSON — required for the self-contained path.
    pub payload_json: Option<&'a Path>,
    /// Presigned PUT URL — when set, skip registration and PUT directly.
    pub upload_url: Option<&'a str>,
    /// Bundle entries (at least one).
    pub entries: &'a [Entry],
    /// Compression strategy for the bundle entries.
    pub strategy: Strategy,
    /// In `--dry-run`, write the packed bundle here for inspection.
    pub out: Option<&'a Path>,
    /// Pack the bundle but skip all network I/O.
    pub dry_run: bool,
}

#[derive(Debug, Deserialize)]
struct ErrorPayload {
    #[serde(default, rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

pub async fn run(params: Params<'_>, policy: RetryPolicy) -> Result<Outcome> {
    if params.entries.is_empty() {
        return Err(Error::ConfigInvalid(
            "nothing to upload: pass at least one of --deps / --timings / --sidecar".into(),
        ));
    }

    // Pack the bundle. In dry-run with --out, pack straight to the target so
    // the caller inspects the exact bytes; otherwise a temp file.
    let tmpdir = tempfile::tempdir()?;
    let zip_path: PathBuf = match (params.dry_run, params.out) {
        (true, Some(out)) => out.to_path_buf(),
        _ => tmpdir.path().join("build-info.zip"),
    };

    let zip_entries: Vec<ZipEntry<'_>> = params
        .entries
        .iter()
        .map(|e| ZipEntry::compressed(e.name.as_str(), e.source.as_path()))
        .collect();
    let zip_size = compress::pack_entries(&zip_entries, &zip_path, params.strategy)?;
    tracing::info!(
        zip_size,
        entries = zip_entries.len(),
        strategy = ?params.strategy,
        "packed build-info bundle"
    );

    if params.dry_run {
        match params.out {
            Some(out) => tracing::info!(
                path = %out.display(),
                zip_size,
                "dry-run: wrote build-info bundle; skipping upload"
            ),
            None => tracing::info!(
                zip_size,
                "dry-run: packed build-info bundle (no --out; not persisted); skipping upload"
            ),
        }
        return Ok(Outcome::DryRun);
    }

    let client = http::build_client()?;

    // Resolve the presigned PUT URL.
    let presigned: String = match params.upload_url {
        Some(url) => url.to_string(),
        None => {
            let app_token = params.app_token.ok_or_else(|| {
                Error::ConfigInvalid(
                    "--app-token (or BUGSEE_APP_TOKEN) is required unless --upload-url is given"
                        .into(),
                )
            })?;
            let payload_json = params.payload_json.ok_or_else(|| {
                Error::ConfigInvalid(
                    "--payload-json is required unless --upload-url is given".into(),
                )
            })?;
            register(&client, policy, params.endpoint, app_token, payload_json).await?
        }
    };

    // Read the bundle into memory so retries can re-issue the body. The
    // build-info bundle is small (typically < 5 MB compressed).
    let body = tokio::fs::read(&zip_path).await?;
    tracing::debug!(presigned_url = %presigned, body_len = body.len(), "PUT build-info bundle");
    // The PUT to the presigned S3 URL is idempotent (an overwrite of the same
    // key), so retrying on a retriable 5xx is safe.
    let put = http::send_with_retry(policy, "build-info PUT", true, || {
        client.put(&presigned).body(body.clone())
    })
    .await?;

    let put_status = put.status();
    if !put_status.is_success() {
        let text = put.text().await.unwrap_or_default();
        return Err(Error::UploadServer {
            status: put_status.as_u16(),
            message: http::truncate_for_log(&text, 512),
        });
    }

    tracing::info!("build-info bundle uploaded");
    Ok(Outcome::Uploaded)
}

/// POST the producer metadata to `/v2/apps/<token>/builds`, ensuring
/// `request_build_info_upload: true`, and return the signed
/// `build_info_upload_endpoint`.
///
/// The registration POST is NOT retried on retriable HTTP statuses (it passes
/// `retry_on_status = false`): a 5xx means the server received the request, so
/// a status-retry could double-register the build. It IS still retried on
/// transport errors (connection / timeout), where the request likely never
/// reached the server. That transport-retry is safe because the appserver
/// dedups on the build `uuid` (replace-then-create), so even a double-sent POST
/// is effectively idempotent.
async fn register(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    payload_json: &Path,
) -> Result<String> {
    let raw = tokio::fs::read(payload_json).await?;
    let mut body: Map<String, Value> = serde_json::from_slice(&raw).map_err(|e| {
        Error::InputInvalid(format!(
            "--payload-json is not a JSON object: {e} (path: {})",
            payload_json.display()
        ))
    })?;
    // The CLI owns the "how": tell the server we want a build-info upload URL.
    body.insert("request_build_info_upload".into(), Value::Bool(true));

    let url = builds_url(endpoint, app_token);
    tracing::debug!(%url, "POST build registration");
    let resp = http::send_with_retry(policy, "build registration POST", false, || {
        client
            .post(&url)
            .header(http::TELEMETRY_HEADER, http::TELEMETRY_VALUE)
            .json(&body)
    })
    .await?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::UploadTransport(format!("reading registration response: {e}")))?;
    if !status.is_success() {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: http::truncate_for_log(&text, 512),
        });
    }

    let value: Value = serde_json::from_str(&text).map_err(|e| Error::UploadServer {
        status: status.as_u16(),
        message: format!(
            "registration response was not valid JSON: {e} — body preview: {}",
            http::truncate_for_log(&text, 200)
        ),
    })?;

    // Tolerate the v2 `{ ok, result: {...} }` envelope and the flat shape
    // (mirrors the Gradle plugin's ApiEndpoint unwrapping).
    let result = value
        .get("result")
        .filter(|r| r.is_object())
        .unwrap_or(&value);

    if let Some(url) = result
        .get("build_info_upload_endpoint")
        .and_then(Value::as_str)
    {
        if !url.is_empty() {
            return Ok(url.to_string());
        }
    }

    // No URL — surface a targeted error for a rejected app token, else generic.
    if let Some(err) = result.get("error") {
        if let Ok(payload) = serde_json::from_value::<ErrorPayload>(err.clone()) {
            let kind = payload.error_type.as_deref().unwrap_or("unknown");
            if kind == "ApplicationNotFoundError" {
                return Err(Error::AppTokenRejected);
            }
            let msg = payload.message.unwrap_or_else(|| "(no message)".into());
            return Err(Error::UploadServer {
                status: status.as_u16(),
                message: format!("server returned error: type={kind} message={msg}"),
            });
        }
    }

    Err(Error::UploadServer {
        status: status.as_u16(),
        message: "registration response had no build_info_upload_endpoint \
                  (is the build-info bundle feature enabled for this app?)"
            .into(),
    })
}

/// Construct the build-registration URL, tolerating an `endpoint` that already
/// carries the `/v2` suffix (mirrors the Gradle plugin's ApiEndpoint).
pub(crate) fn builds_url(endpoint: &str, app_token: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    let base = base.strip_suffix("/v2").unwrap_or(base);
    format!("{base}/v2/apps/{app_token}/builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit_code::ExitCode;
    use std::io::Read;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zip::ZipArchive;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn builds_url_tolerates_v2_suffix() {
        assert_eq!(
            builds_url("https://api.bugsee.com", "TKN"),
            "https://api.bugsee.com/v2/apps/TKN/builds"
        );
        assert_eq!(
            builds_url("https://api.bugsee.com/", "TKN"),
            "https://api.bugsee.com/v2/apps/TKN/builds"
        );
        assert_eq!(
            builds_url("https://api.bugsee.com/v2", "TKN"),
            "https://api.bugsee.com/v2/apps/TKN/builds"
        );
        assert_eq!(
            builds_url("https://api.bugsee.com/v2/", "TKN"),
            "https://api.bugsee.com/v2/apps/TKN/builds"
        );
    }

    #[tokio::test]
    async fn self_contained_registers_then_puts_zstd_bundle() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();

        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":["a"]}"#);
        let timings = write(tmp.path(), "timings.json", r#"{"total_ms":1234}"#);
        let payload = write(
            tmp.path(),
            "payload.json",
            r#"{"uuid":"abc","version":"1.0","build":"42"}"#,
        );

        let put_url = format!("{}/put-here", server.uri());

        // Registration POST: must carry the telemetry header, the injected
        // request flag, and the producer's own metadata.
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .and(header("X-Bugsee-Uploader", "cli"))
            .and(body_partial_json(serde_json::json!({
                "request_build_info_upload": true,
                "uuid": "abc",
                "version": "1.0",
                "build": "42"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "build_id": "b1", "build_info_upload_endpoint": put_url }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/put-here"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let entries = vec![
            Entry {
                name: "dependencies.json".into(),
                source: deps,
            },
            Entry {
                name: "timings.json".into(),
                source: timings,
            },
        ];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: Some("TKN"),
            payload_json: Some(&payload),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let outcome = run(params, RetryPolicy::fast(3)).await.unwrap();
        assert_eq!(outcome, Outcome::Uploaded);

        // Pin the PUT body: a zstd ZIP whose entries are exactly the sidecars.
        let received = server.received_requests().await.unwrap();
        let put = received
            .iter()
            .find(|r| r.url.path() == "/put-here")
            .expect("a PUT to the presigned URL");
        let mut archive = ZipArchive::new(std::io::Cursor::new(put.body.clone())).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["dependencies.json", "timings.json"]);
        {
            let dep_entry = archive.by_name("dependencies.json").unwrap();
            assert_eq!(dep_entry.compression(), zip::CompressionMethod::Zstd);
        }
        let mut got = String::new();
        archive
            .by_name("dependencies.json")
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, r#"{"deps":["a"]}"#);
    }

    #[tokio::test]
    async fn upload_url_skips_registration_and_puts_directly() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);

        let put_url = format!("{}/presigned-put", server.uri());
        // ONLY a PUT is expected — any POST would fail the (absent) registration mock.
        Mock::given(method("PUT"))
            .and(path("/presigned-put"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: None,
            payload_json: None,
            upload_url: Some(&put_url),
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let outcome = run(params, RetryPolicy::fast(3)).await.unwrap();
        assert_eq!(outcome, Outcome::Uploaded);
    }

    #[tokio::test]
    async fn dry_run_writes_bundle_and_does_no_network() {
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":["x"]}"#);
        let out = tmp.path().join("bundle.zip");

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        // Endpoint is deliberately unroutable — a dry run must not touch it.
        let params = Params {
            endpoint: "http://127.0.0.1:1/",
            app_token: None,
            payload_json: None,
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: Some(&out),
            dry_run: true,
        };
        let outcome = run(params, RetryPolicy::none()).await.unwrap();
        assert_eq!(outcome, Outcome::DryRun);

        let mut archive = ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        let mut got = String::new();
        archive
            .by_name("dependencies.json")
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, r#"{"deps":["x"]}"#);
    }

    #[tokio::test]
    async fn empty_entries_is_config_error() {
        let entries: Vec<Entry> = Vec::new();
        let params = Params {
            endpoint: "https://api.bugsee.com",
            app_token: Some("TKN"),
            payload_json: None,
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert_eq!(err.exit_code(), ExitCode::ConfigInvalid);
    }

    #[tokio::test]
    async fn rejected_app_token_maps_to_app_token_rejected() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);
        let payload = write(
            tmp.path(),
            "payload.json",
            r#"{"version":"1.0","build":"1"}"#,
        );

        // Real appserver wire shape: HTTP 200 with the error at the TOP LEVEL
        // (no `result` wrapper) — `register` finds it via its `unwrap_or(&value)`
        // fallback.
        Mock::given(method("POST"))
            .and(path("/v2/apps/BAD/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false,
                "error": { "type": "ApplicationNotFoundError", "message": "no such app" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: Some("BAD"),
            payload_json: Some(&payload),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert_eq!(err.exit_code(), ExitCode::AppTokenRejected);
    }

    #[tokio::test]
    async fn missing_endpoint_field_is_server_error() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);
        let payload = write(
            tmp.path(),
            "payload.json",
            r#"{"version":"1.0","build":"1"}"#,
        );

        // 200 but no build_info_upload_endpoint (feature flag off server-side).
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "build_id": "b1" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: Some("TKN"),
            payload_json: Some(&payload),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert_eq!(err.exit_code(), ExitCode::UploadServer);
    }

    #[tokio::test]
    async fn flat_envelope_endpoint_is_accepted() {
        // The registration response carries build_info_upload_endpoint at the
        // TOP LEVEL (no `result` wrapper). register() must still find it and
        // proceed to the PUT.
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);
        let payload = write(
            tmp.path(),
            "payload.json",
            r#"{"version":"1.0","build":"1"}"#,
        );

        let put_url = format!("{}/flat-put", server.uri());
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "build_info_upload_endpoint": put_url
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/flat-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: Some("TKN"),
            payload_json: Some(&payload),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let outcome = run(params, RetryPolicy::fast(3)).await.unwrap();
        assert_eq!(outcome, Outcome::Uploaded);
    }

    #[tokio::test]
    async fn failing_presigned_put_maps_to_upload_server() {
        // Registration succeeds, but the presigned PUT returns 500. run() must
        // surface UploadServer. RetryPolicy::none() keeps it single-shot so the
        // 500 is observed directly without burning retries.
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);
        let payload = write(
            tmp.path(),
            "payload.json",
            r#"{"version":"1.0","build":"1"}"#,
        );

        let put_url = format!("{}/will-500", server.uri());
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "build_info_upload_endpoint": put_url }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/will-500"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: Some("TKN"),
            payload_json: Some(&payload),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert_eq!(err.exit_code(), ExitCode::UploadServer);
    }

    #[tokio::test]
    async fn non_object_payload_json_is_input_invalid() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let deps = write(tmp.path(), "dependencies.json", r#"{"deps":[]}"#);
        // A JSON array, not an object → cannot inject the request flag.
        let payload = write(tmp.path(), "payload.json", r#"[1,2,3]"#);

        let entries = vec![Entry {
            name: "dependencies.json".into(),
            source: deps,
        }];
        let uri = server.uri();
        let params = Params {
            endpoint: &uri,
            app_token: Some("TKN"),
            payload_json: Some(&payload),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::Zstd(11),
            out: None,
            dry_run: false,
        };
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert_eq!(err.exit_code(), ExitCode::InputInvalid);
    }
}
