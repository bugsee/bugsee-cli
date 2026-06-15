//! Chunked artefact upload — the BUILDS chunked protocol.
//!
//! For artefacts too large for a single PUT. Mirrors the Gradle plugin's
//! `ChunkedBundleUploader` and the appserver `chunks.service.js` contract:
//!
//!   1. `GET  /v2/apps/<token>/builds/chunk-options` → `{chunk_size, max_chunks, …}`
//!   2. Slice the upload ZIP into `chunk_size` blocks; SHA-1 (hex) each.
//!   3. `POST /v2/apps/<token>/builds/chunks/check` `{sha1_list:[…]}` →
//!      `{missing:[…], upload_urls:{sha1:presigned_put_url}}`
//!   4. PUT each MISSING chunk (deduped by first index) to its presigned URL
//!      with `Content-Type: application/octet-stream`.
//!   5. `POST /v2/apps/<token>/builds/chunked` `{…metadata, chunks:[sha1…]}` →
//!      `{build_id, build_info_upload_endpoint, …}`; the server stitches the
//!      chunks into `final/builds/<id>-<tid>.zip` via S3 UploadPartCopy.
//!
//! The producer owns *what* (the artefact + metadata); the CLI owns *how*
//! (chunking, hashing, the presigned PUTs, retries). All network I/O flows
//! through [`crate::upload::http`].

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::{Map, Value};
use sha1::{Digest, Sha1};

use crate::error::{Error, Result};
use crate::upload::build_info;
use crate::upload::http::{self, RetryPolicy};

/// 64 KiB streaming buffer for hashing — keeps memory bounded regardless of
/// artefact size.
const HASH_BUF_BYTES: usize = 64 * 1024;

/// Result of a chunked build submission — the same fields the single-PUT
/// registration yields, so the caller handles the build-info bundle uniformly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ChunkedOutcome {
    pub build_id: String,
    pub build_info_endpoint: String,
}

struct ChunkOptions {
    chunk_size: usize,
    max_chunks: usize,
}

/// Upload `artifact_zip` via the chunked protocol and submit the build.
/// `metadata` is the registration body (uuid / package_id / version / build /
/// format + `request_*_upload` flags); the CLI appends `chunks`.
pub async fn upload(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    artifact_zip: &Path,
    metadata: &Map<String, Value>,
) -> Result<ChunkedOutcome> {
    let opts = get_chunk_options(client, policy, endpoint, app_token).await?;

    let hashes = compute_chunk_hashes(artifact_zip, opts.chunk_size)?;
    if hashes.is_empty() {
        return Err(Error::InputInvalid(
            "artefact has zero bytes; nothing to upload".into(),
        ));
    }
    if hashes.len() > opts.max_chunks {
        return Err(Error::InputInvalid(format!(
            "artefact too large for chunked upload: {} chunks > server max {}",
            hashes.len(),
            opts.max_chunks
        )));
    }
    tracing::info!(
        chunks = hashes.len(),
        chunk_size = opts.chunk_size,
        "computed chunk hashes"
    );

    let (missing, upload_urls) = check_chunks(client, policy, endpoint, app_token, &hashes).await?;
    tracing::info!(
        missing = missing.len(),
        total = hashes.len(),
        "chunks/check"
    );

    // PUT each missing chunk exactly once. The positional `hashes` list can
    // repeat a sha1 (an archive with identical content blocks); `missing` lists
    // each unique sha1 once, so we PUT the FIRST occurrence and skip the rest —
    // the server stitches by sha1, so one upload per unique chunk suffices.
    let missing_set: HashSet<&str> = missing.iter().map(String::as_str).collect();
    let mut uploaded: HashSet<&str> = HashSet::new();
    for (index, sha1) in hashes.iter().enumerate() {
        if !missing_set.contains(sha1.as_str()) || !uploaded.insert(sha1.as_str()) {
            continue;
        }
        let url = upload_urls.get(sha1).ok_or_else(|| Error::UploadServer {
            status: 0,
            message: format!("chunks/check returned no upload_url for missing chunk {sha1}"),
        })?;
        put_chunk(client, policy, artifact_zip, index, opts.chunk_size, url).await?;
    }

    submit_chunked(client, policy, endpoint, app_token, metadata, &hashes).await
}

/// `.../builds<suffix>` — mirrors the Gradle plugin's `ApiEndpoint.buildsUrl`.
fn builds_url(endpoint: &str, app_token: &str, suffix: &str) -> String {
    format!("{}{suffix}", build_info::builds_url(endpoint, app_token))
}

/// Unwrap the v2 `{ ok, result: {...} }` envelope, tolerating the flat shape.
fn unwrap_result(value: &Value) -> &Value {
    value
        .get("result")
        .filter(|r| r.is_object())
        .unwrap_or(value)
}

async fn get_chunk_options(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
) -> Result<ChunkOptions> {
    let url = builds_url(endpoint, app_token, "/chunk-options");
    // Idempotent GET — safe to retry on transport AND retriable status.
    let resp =
        http::send_with_retry(policy, "chunk-options GET", true, || client.get(&url)).await?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::UploadTransport(format!("reading chunk-options: {e}")))?;
    if !status.is_success() {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: http::truncate_for_log(&text, 512),
        });
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| Error::UploadServer {
        status: status.as_u16(),
        message: format!("chunk-options was not valid JSON: {e}"),
    })?;
    let result = unwrap_result(&value);
    let chunk_size = result
        .get("chunk_size")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::UploadServer {
            status: status.as_u16(),
            message: "chunk-options missing numeric `chunk_size`".into(),
        })? as usize;
    let max_chunks = result
        .get("max_chunks")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::UploadServer {
            status: status.as_u16(),
            message: "chunk-options missing numeric `max_chunks`".into(),
        })? as usize;
    if chunk_size == 0 {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: "chunk-options returned chunk_size=0".into(),
        });
    }
    Ok(ChunkOptions {
        chunk_size,
        max_chunks,
    })
}

/// SHA-1 (lowercase hex) of each `chunk_size` block of `file`, streamed through
/// a 64 KiB buffer so memory stays bounded. Concatenating the chunks in order
/// reproduces the file exactly (the server stitches by this order).
fn compute_chunk_hashes(file: &Path, chunk_size: usize) -> Result<Vec<String>> {
    let mut f = std::fs::File::open(file)?;
    let mut buf = vec![0u8; HASH_BUF_BYTES];
    let mut hashes = Vec::new();
    let mut hasher = Sha1::new();
    let mut chunk_filled = 0usize;
    loop {
        let want = HASH_BUF_BYTES.min(chunk_size - chunk_filled);
        let n = f.read(&mut buf[..want])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        chunk_filled += n;
        if chunk_filled == chunk_size {
            let digest: [u8; 20] = hasher.finalize_reset().into();
            hashes.push(hex(&digest));
            chunk_filled = 0;
        }
    }
    if chunk_filled > 0 {
        let digest: [u8; 20] = hasher.finalize().into();
        hashes.push(hex(&digest));
    }
    Ok(hashes)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

async fn check_chunks(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    hashes: &[String],
) -> Result<(Vec<String>, std::collections::HashMap<String, String>)> {
    let url = builds_url(endpoint, app_token, "/chunks/check");
    let body = serde_json::json!({ "sha1_list": hashes });
    // Idempotent (HEAD probes server-side) — retry on transport + status.
    let resp = http::send_with_retry(policy, "chunks/check POST", true, || {
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
        .map_err(|e| Error::UploadTransport(format!("reading chunks/check: {e}")))?;
    if !status.is_success() {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: http::truncate_for_log(&text, 512),
        });
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| Error::UploadServer {
        status: status.as_u16(),
        message: format!("chunks/check was not valid JSON: {e}"),
    })?;
    let result = unwrap_result(&value);
    let missing: Vec<String> = result
        .get("missing")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let upload_urls: std::collections::HashMap<String, String> = result
        .get("upload_urls")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Ok((missing, upload_urls))
}

async fn put_chunk(
    client: &reqwest::Client,
    policy: RetryPolicy,
    file: &Path,
    index: usize,
    chunk_size: usize,
    presigned_url: &str,
) -> Result<()> {
    // Read exactly this chunk (chunk_size bytes, fewer on the final chunk) into
    // memory so retries can re-issue the body. One chunk at a time keeps peak
    // memory at ~chunk_size regardless of artefact size.
    let mut f = std::fs::File::open(file)?;
    f.seek(SeekFrom::Start(index as u64 * chunk_size as u64))?;
    let mut chunk = vec![0u8; chunk_size];
    let mut filled = 0usize;
    while filled < chunk_size {
        let n = f.read(&mut chunk[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    chunk.truncate(filled);

    // Idempotent overwrite of the same S3 key — retry on transport + status.
    let resp = http::send_with_retry(policy, "chunk PUT", true, || {
        client
            .put(presigned_url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(chunk.clone())
    })
    .await?;
    if !resp.status().is_success() {
        let s = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::UploadServer {
            status: s,
            message: http::truncate_for_log(&text, 512),
        });
    }
    Ok(())
}

async fn submit_chunked(
    client: &reqwest::Client,
    policy: RetryPolicy,
    endpoint: &str,
    app_token: &str,
    metadata: &Map<String, Value>,
    hashes: &[String],
) -> Result<ChunkedOutcome> {
    let url = builds_url(endpoint, app_token, "/chunked");
    let mut body = metadata.clone();
    body.insert(
        "chunks".into(),
        Value::Array(hashes.iter().cloned().map(Value::String).collect()),
    );
    // NOT retried on a retriable status: the submit registers the build AND
    // triggers the server-side stitch; a status-retry could re-stitch. Transport
    // retries are safe — the appserver dedups on the build `uuid`.
    let resp = http::send_with_retry(policy, "builds/chunked POST", false, || {
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
        .map_err(|e| Error::UploadTransport(format!("reading /builds/chunked: {e}")))?;
    if !status.is_success() {
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: http::truncate_for_log(&text, 512),
        });
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| Error::UploadServer {
        status: status.as_u16(),
        message: format!("/builds/chunked was not valid JSON: {e}"),
    })?;
    let result = unwrap_result(&value);
    let build_id = result
        .get("build_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if build_id.is_empty() {
        // A 2xx without build_id means the worker pipeline has no key — fail
        // loud so the caller falls back to single-PUT.
        return Err(Error::UploadServer {
            status: status.as_u16(),
            message: "/builds/chunked returned 2xx without build_id".into(),
        });
    }
    Ok(ChunkedOutcome {
        build_id,
        build_info_endpoint: result
            .get("build_info_upload_endpoint")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn hashes_match_sha1_of_each_block_and_are_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("artefact.zip");
        // 2.5 chunks of distinct bytes (chunk_size=4): forces a trailing
        // partial chunk and distinct per-block hashes.
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).unwrap();
        drop(f);

        let h1 = compute_chunk_hashes(&path, 4).unwrap();
        let h2 = compute_chunk_hashes(&path, 4).unwrap();
        assert_eq!(h1, h2, "hashing must be deterministic");
        assert_eq!(h1.len(), 3, "10 bytes / 4 = 2 full + 1 partial chunk");

        // Each hash equals SHA-1 of the corresponding block.
        let expect = |bytes: &[u8]| -> String {
            let d: [u8; 20] = Sha1::digest(bytes).into();
            hex(&d)
        };
        assert_eq!(h1[0], expect(&[1, 2, 3, 4]));
        assert_eq!(h1[1], expect(&[5, 6, 7, 8]));
        assert_eq!(h1[2], expect(&[9, 10]));
    }

    #[test]
    fn duplicate_content_blocks_yield_repeated_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dup.zip");
        // Two identical 4-byte blocks → identical hashes at indices 0 and 1.
        std::fs::write(&path, [7, 7, 7, 7, 7, 7, 7, 7]).unwrap();
        let h = compute_chunk_hashes(&path, 4).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0], h[1], "identical blocks must hash identically");
    }

    #[test]
    fn empty_file_yields_no_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("empty.zip");
        std::fs::write(&path, []).unwrap();
        assert!(compute_chunk_hashes(&path, 4).unwrap().is_empty());
    }

    #[test]
    fn hex_encodes_lowercase() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn builds_url_appends_suffix() {
        assert_eq!(
            builds_url("https://api.bugsee.com", "TKN", "/chunk-options"),
            "https://api.bugsee.com/v2/apps/TKN/builds/chunk-options"
        );
    }

    #[tokio::test]
    async fn full_chunked_flow_options_check_put_submit() {
        use wiremock::matchers::{body_partial_json, header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        // 8 bytes of distinct content, chunk_size 4 → 2 distinct chunks.
        let artefact = tmp.path().join("upload.zip");
        std::fs::write(&artefact, [10, 20, 30, 40, 50, 60, 70, 80]).unwrap();
        let hashes = compute_chunk_hashes(&artefact, 4).unwrap();
        assert_eq!(hashes.len(), 2);

        let put_url = format!("{}/chunk-put", server.uri());

        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/chunk-options"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "chunk_size": 4, "max_chunks": 100 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Both chunks missing; both point at the same presigned mock path.
        let mut upload_urls = serde_json::Map::new();
        upload_urls.insert(hashes[0].clone(), Value::String(put_url.clone()));
        upload_urls.insert(hashes[1].clone(), Value::String(put_url.clone()));
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds/chunks/check"))
            .and(header("X-Bugsee-Uploader", "cli"))
            .and(body_partial_json(
                serde_json::json!({ "sha1_list": hashes }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "missing": hashes, "upload_urls": upload_urls }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/chunk-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2) // one PUT per distinct missing chunk
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds/chunked"))
            .and(body_partial_json(serde_json::json!({
                "uuid": "u1", "chunks": hashes
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "build_id": "b1", "build_info_upload_endpoint": "" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = http::build_client().unwrap();
        let mut metadata = Map::new();
        metadata.insert("uuid".into(), Value::String("u1".into()));
        let uri = server.uri();
        let out = upload(
            &client,
            RetryPolicy::fast(3),
            &uri,
            "TKN",
            &artefact,
            &metadata,
        )
        .await
        .unwrap();
        assert_eq!(out.build_id, "b1");
    }

    #[tokio::test]
    async fn submit_without_build_id_is_an_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let artefact = tmp.path().join("upload.zip");
        std::fs::write(&artefact, [1, 2, 3, 4]).unwrap();
        let hashes = compute_chunk_hashes(&artefact, 4).unwrap();

        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/chunk-options"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "chunk_size": 4, "max_chunks": 100 }
            })))
            .mount(&server)
            .await;
        // No missing chunks → no PUTs needed.
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds/chunks/check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "missing": [], "upload_urls": {} }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds/chunked"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": { "build_id": "" } // 2xx but no build_id
            })))
            .mount(&server)
            .await;

        let client = http::build_client().unwrap();
        let mut metadata = Map::new();
        metadata.insert("uuid".into(), Value::String("u1".into()));
        let _ = hashes;
        let uri = server.uri();
        let err = upload(
            &client,
            RetryPolicy::none(),
            &uri,
            "TKN",
            &artefact,
            &metadata,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::UploadServer { .. }), "got: {err:?}");
    }
}
