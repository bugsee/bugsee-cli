//! Build upload (single-PUT) — the design's "Artefact" upload class.
//!
//! Registers the build (`POST /v2/apps/<token>/builds`) and PUTs the build
//! artefact to the presigned URL the registration returns. One registration
//! also yields the signed build-info endpoint, so when `--deps`/`--timings`
//! are supplied this drives the build-info bundle in the SAME flow (pre-signed
//! mode) — no second registration.
//!
//! The producer (Gradle plugin) owns *what* to upload (the metadata body, which
//! files); the CLI owns *how* (ZIP packing, zstd, the registration handshake,
//! presigned PUTs, retries, telemetry). All network I/O flows through
//! [`crate::upload::http`].
//!
//! Chunked artefact upload (large `.aab`/`.apk`) is a separate path — see
//! [`crate::upload::chunked`]. This module is the single-PUT case only.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::compress::{self, Strategy, ZipEntry};
use crate::error::{Error, Result};
use crate::upload::http::{self, RetryPolicy};
use crate::upload::{build_info, chunked};

/// Outcome of a build upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Artefact PUT to the presigned URL; carries the server's build id.
    Uploaded { build_id: String },
    /// `--dry-run`: artefact ZIP packed but no network I/O performed.
    DryRun,
}

/// Parameters for a single-PUT build upload.
pub struct Params<'a> {
    pub endpoint: &'a str,
    pub app_token: &'a str,
    /// Registration metadata JSON — the POST body for `/v2/apps/<token>/builds`.
    pub payload_json: &'a Path,
    /// The build artefact (`.aab`/`.apk`/`.ipa`). STORED verbatim in the upload
    /// ZIP (already a compressed container).
    pub artifact: &'a Path,
    /// Optional R8/ProGuard `mapping.txt`, packed (zstd) alongside the artefact.
    pub mapping: Option<&'a Path>,
    /// Optional build-info sidecars — when present, the build-info bundle is
    /// uploaded to the signed endpoint from the same registration.
    pub deps: Option<&'a Path>,
    pub timings: Option<&'a Path>,
    /// Compression strategy for the mapping entry + the build-info bundle.
    pub strategy: Strategy,
    /// Upload the artefact via the chunked protocol (`/builds/chunked`) instead
    /// of a single PUT — for large artefacts. Both paths produce the same build
    /// record + build-info handling.
    pub chunked: bool,
    /// Pack the upload ZIP but skip all network I/O.
    pub dry_run: bool,
    /// In `--dry-run`, write the packed upload ZIP here for inspection.
    pub out: Option<&'a Path>,
}

/// Endpoints the registration POST returns. Empty strings mean "not signed"
/// (the corresponding `request_*_upload` flag was off or the feature is gated).
#[derive(Debug, Default)]
struct Registration {
    build_id: String,
    artifact_endpoint: String,
    build_info_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct ErrorPayload {
    #[serde(default, rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

pub async fn run(params: Params<'_>, policy: RetryPolicy) -> Result<Outcome> {
    if !params.artifact.is_file() {
        return Err(Error::InputNotFound(format!(
            "artifact does not exist or is not a file: {}",
            params.artifact.display()
        )));
    }

    // 1. Pack the upload ZIP: artefact STORED + optional mapping zstd. Same
    //    container the worker's size-analysis job consumes (see `pack`).
    let tmpdir = tempfile::tempdir()?;
    let zip_path: PathBuf = match (params.dry_run, params.out) {
        (true, Some(out)) => out.to_path_buf(),
        _ => tmpdir.path().join("upload.zip"),
    };
    let artifact_name = params
        .artifact
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::ConfigInvalid("artifact path has no usable file name".into()))?;
    let mut entries = vec![ZipEntry::stored(artifact_name, params.artifact)];
    if let Some(m) = params.mapping {
        if !m.is_file() {
            return Err(Error::InputNotFound(format!(
                "mapping does not exist or is not a file: {}",
                m.display()
            )));
        }
        entries.push(ZipEntry::compressed("mapping.txt", m));
    }
    let zip_size = compress::pack_entries(&entries, &zip_path, params.strategy)?;
    tracing::info!(zip_size, artifact = %artifact_name, "packed build upload ZIP");

    if params.dry_run {
        tracing::info!(
            zip_size,
            path = params.out.map(|p| p.display().to_string()),
            "dry-run: packed build upload ZIP; skipping registration + upload"
        );
        return Ok(Outcome::DryRun);
    }

    let client = http::build_client()?;

    // Registration metadata, built once and shared by both transports: the
    // producer's JSON + the `request_*_upload` flags the CLI owns.
    let want_build_info = params.deps.is_some() || params.timings.is_some();
    let metadata = build_metadata(&params, want_build_info).await?;

    // 2. Upload the artefact — chunked (large) or single-PUT — and learn the
    //    build id + the signed build-info endpoint. Both transports yield the
    //    same two facts, so step 3 is identical for either.
    let (build_id, build_info_endpoint) = if params.chunked {
        let out = chunked::upload(
            &client,
            policy,
            params.endpoint,
            params.app_token,
            &zip_path,
            &metadata,
        )
        .await?;
        (out.build_id, out.build_info_endpoint)
    } else {
        let reg = register_single(
            &client,
            policy,
            params.endpoint,
            params.app_token,
            &metadata,
        )
        .await?;
        if reg.artifact_endpoint.is_empty() {
            return Err(Error::UploadServer {
                status: 0,
                message: "registration returned no artefact `endpoint` \
                          (was request_artifact_upload set?)"
                    .into(),
            });
        }
        // PUT the artefact ZIP. Idempotent overwrite of the same S3 key, so a
        // retriable 5xx is safe to retry.
        let body = tokio::fs::read(&zip_path).await?;
        tracing::debug!(endpoint = %reg.artifact_endpoint, body_len = body.len(), "PUT artefact");
        let put = http::send_with_retry(policy, "artefact PUT", true, || {
            client
                .put(&reg.artifact_endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(body.clone())
        })
        .await?;
        if !put.status().is_success() {
            let s = put.status().as_u16();
            let text = put.text().await.unwrap_or_default();
            return Err(Error::UploadServer {
                status: s,
                message: http::truncate_for_log(&text, 512),
            });
        }
        (reg.build_id, reg.build_info_endpoint)
    };
    tracing::info!(%build_id, chunked = params.chunked, "artefact uploaded");

    // 3. Build-info bundle, from the SAME registration (pre-signed mode) — no
    //    second build registration. Only when sidecars were supplied AND the
    //    server signed the endpoint (the org's build-info flag is on);
    //    otherwise the producer's native/legacy path covers deps/timings.
    if want_build_info && !build_info_endpoint.is_empty() {
        let mut bi_entries: Vec<build_info::Entry> = Vec::new();
        if let Some(d) = params.deps {
            bi_entries.push(build_info::Entry {
                name: "dependencies.json".into(),
                source: d.to_path_buf(),
            });
        }
        if let Some(t) = params.timings {
            bi_entries.push(build_info::Entry {
                name: "timings.json".into(),
                source: t.to_path_buf(),
            });
        }
        let bi = build_info::Params {
            endpoint: params.endpoint,
            app_token: Some(params.app_token),
            payload_json: None,
            upload_url: Some(build_info_endpoint.as_str()),
            entries: &bi_entries,
            strategy: params.strategy,
            out: None,
            dry_run: false,
        };
        build_info::run(bi, policy).await?;
        tracing::info!("build-info bundle uploaded (same registration)");
    } else if want_build_info {
        tracing::info!(
            "build-info sidecars supplied but no signed build_info_upload_endpoint; \
             leaving deps/timings to the producer's legacy path"
        );
    }

    Ok(Outcome::Uploaded { build_id })
}

/// Build the registration metadata body: the producer's JSON plus the
/// `request_*_upload` flags the CLI owns (it is uploading an artefact, and a
/// build-info bundle when sidecars are present). Shared by both transports.
async fn build_metadata(params: &Params<'_>, want_build_info: bool) -> Result<Map<String, Value>> {
    let raw = tokio::fs::read(params.payload_json).await?;
    let mut body: Map<String, Value> = serde_json::from_slice(&raw).map_err(|e| {
        Error::InputInvalid(format!(
            "--payload-json is not a JSON object: {e} (path: {})",
            params.payload_json.display()
        ))
    })?;
    body.insert("request_artifact_upload".into(), Value::Bool(true));
    if want_build_info {
        body.insert("request_build_info_upload".into(), Value::Bool(true));
    }
    Ok(body)
}

/// POST the metadata to `/v2/apps/<token>/builds` and read back the signed
/// endpoints + `build_id` (single-PUT path).
///
/// Not retried on a retriable HTTP status (a 5xx means the server saw it —
/// status-retry could double-register); IS retried on transport errors, which
/// is safe because the appserver dedups on the build `uuid` (replace-then-create).
async fn register_single(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    metadata: &Map<String, Value>,
) -> Result<Registration> {
    let url = build_info::builds_url(endpoint, app_token);
    tracing::debug!(%url, "POST build registration");
    let resp = http::send_with_retry(policy, "build registration POST", false, || {
        client
            .post(&url)
            .header(http::TELEMETRY_HEADER, http::TELEMETRY_VALUE)
            .json(metadata)
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
    // Tolerate the v2 `{ ok, result: {...} }` envelope and the flat shape.
    let result = value
        .get("result")
        .filter(|r| r.is_object())
        .unwrap_or(&value);

    // A signed artefact endpoint (or build_id) means success.
    let str_field = |k: &str| {
        result
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let reg = Registration {
        build_id: str_field("build_id"),
        artifact_endpoint: str_field("endpoint"),
        build_info_endpoint: str_field("build_info_upload_endpoint"),
    };
    if !reg.artifact_endpoint.is_empty() || !reg.build_id.is_empty() {
        return Ok(reg);
    }

    // No usable fields — surface a targeted error for a rejected app token.
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
        message: "registration response had neither `endpoint` nor `build_id`".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zip::ZipArchive;

    fn write(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    fn base_params<'a>(endpoint: &'a str, payload: &'a Path, artifact: &'a Path) -> Params<'a> {
        Params {
            endpoint,
            app_token: "TKN",
            payload_json: payload,
            artifact,
            mapping: None,
            deps: None,
            timings: None,
            strategy: Strategy::Zstd(11),
            chunked: false,
            dry_run: false,
            out: None,
        }
    }

    #[tokio::test]
    async fn dry_run_packs_zip_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let artifact = write(tmp.path(), "app.aab", b"PK\x03\x04 fake aab");
        let mapping = write(tmp.path(), "mapping.txt", b"a -> b\n");
        let payload = write(tmp.path(), "p.json", br#"{"uuid":"abc"}"#);
        let out = tmp.path().join("upload.zip");

        let mut params = base_params("http://127.0.0.1:1", &payload, &artifact);
        params.mapping = Some(&mapping);
        params.dry_run = true;
        params.out = Some(&out);

        // No mock server — a network call would fail. dry-run must not make one.
        let outcome = run(params, RetryPolicy::none()).await.unwrap();
        assert_eq!(outcome, Outcome::DryRun);

        let mut zip = ZipArchive::new(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(
            zip.by_name("app.aab").unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        assert_eq!(
            zip.by_name("mapping.txt").unwrap().compression(),
            zip::CompressionMethod::Zstd
        );
    }

    #[tokio::test]
    async fn registers_then_puts_artefact() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let artifact = write(tmp.path(), "app.aab", b"PK\x03\x04 fake aab bytes");
        let mapping = write(tmp.path(), "mapping.txt", &b"x -> y\n".repeat(50));
        let payload = write(tmp.path(), "p.json", br#"{"uuid":"abc","version":"1.0"}"#);
        let put_url = format!("{}/artefact-put", server.uri());

        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .and(header("X-Bugsee-Uploader", "cli"))
            .and(body_partial_json(serde_json::json!({
                "request_artifact_upload": true, "uuid": "abc"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "build_id": "b1", "endpoint": put_url }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/artefact-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        let mut params = base_params(&uri, &payload, &artifact);
        params.mapping = Some(&mapping);
        let outcome = run(params, RetryPolicy::fast(3)).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Uploaded {
                build_id: "b1".into()
            }
        );

        // The PUT body is the upload ZIP: artefact STORED + mapping zstd.
        let received = server.received_requests().await.unwrap();
        let put = received
            .iter()
            .find(|r| r.url.path() == "/artefact-put")
            .unwrap();
        let mut zip = ZipArchive::new(std::io::Cursor::new(put.body.clone())).unwrap();
        assert_eq!(
            zip.by_name("app.aab").unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        let mut map = zip.by_name("mapping.txt").unwrap();
        assert_eq!(map.compression(), zip::CompressionMethod::Zstd);
        let mut got = Vec::new();
        map.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"x -> y\n".repeat(50));
    }

    #[tokio::test]
    async fn uploads_build_info_bundle_from_same_registration() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let artifact = write(tmp.path(), "app.aab", b"PK\x03\x04 aab");
        let deps = write(tmp.path(), "deps.json", br#"{"deps":["a"]}"#);
        let payload = write(tmp.path(), "p.json", br#"{"uuid":"abc"}"#);
        let art_url = format!("{}/art", server.uri());
        let bi_url = format!("{}/buildinfo", server.uri());

        // One registration returns BOTH endpoints; deps present so the CLI must
        // inject request_build_info_upload too.
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .and(body_partial_json(serde_json::json!({
                "request_artifact_upload": true, "request_build_info_upload": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "build_id": "b2", "endpoint": art_url, "build_info_upload_endpoint": bi_url }
            })))
            .expect(1) // exactly ONE registration — no second POST for build-info
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/art"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/buildinfo"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        let mut params = base_params(&uri, &payload, &artifact);
        params.deps = Some(&deps);
        let outcome = run(params, RetryPolicy::fast(3)).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Uploaded {
                build_id: "b2".into()
            }
        );

        // build-info PUT body is a zstd ZIP with dependencies.json.
        let received = server.received_requests().await.unwrap();
        let bi = received
            .iter()
            .find(|r| r.url.path() == "/buildinfo")
            .unwrap();
        let mut zip = ZipArchive::new(std::io::Cursor::new(bi.body.clone())).unwrap();
        assert_eq!(
            zip.by_name("dependencies.json").unwrap().compression(),
            zip::CompressionMethod::Zstd
        );
    }

    #[tokio::test]
    async fn application_not_found_maps_to_app_token_rejected() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let artifact = write(tmp.path(), "app.aab", b"PK\x03\x04");
        let payload = write(tmp.path(), "p.json", br#"{"uuid":"abc"}"#);
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false, "result": { "error": { "type": "ApplicationNotFoundError" } }
            })))
            .mount(&server)
            .await;
        let uri = server.uri();
        let params = base_params(&uri, &payload, &artifact);
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert!(matches!(err, Error::AppTokenRejected), "got: {err:?}");
    }

    #[tokio::test]
    async fn missing_artifact_errors_before_network() {
        let tmp = tempfile::tempdir().unwrap();
        let payload = write(tmp.path(), "p.json", br#"{"uuid":"abc"}"#);
        let missing = tmp.path().join("nope.aab");
        let params = base_params("http://127.0.0.1:1", &payload, &missing);
        let err = run(params, RetryPolicy::none()).await.unwrap_err();
        assert!(matches!(err, Error::InputNotFound(_)), "got: {err:?}");
    }
}
