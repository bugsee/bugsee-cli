use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::compress::{self, Strategy, ZipEntry};
use crate::error::{config_invalid, input_invalid, input_not_found};
use crate::symbols::{dsym, elf, proguard, sourcemap};
use crate::upload::http::{self, RetryPolicy};
use crate::upload::presigned;

const DEFAULT_ENDPOINT: &str = "https://api.bugsee.com";

#[derive(Subcommand, Debug)]
pub enum DebugFilesCommand {
    /// Discover, package, and upload debug information files from one or more paths.
    Upload {
        /// One or more directories or files to scan.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Restrict discovery to a specific debug-file type. If unset, defaults to `proguard`.
        /// Supported: `proguard`, `elf`, `dsym`, `sourcemaps`; other types are scaffold-only.
        #[arg(long, value_enum)]
        r#type: Option<DebugFileType>,

        /// App version (`android:versionName`) — recorded on the symbol document.
        #[arg(long)]
        version: String,

        /// Build number (`android:versionCode`) — recorded on the symbol document.
        #[arg(long)]
        build: String,

        /// Override the auto-computed debug-id with a caller-supplied UUID.
        ///
        /// Required when the caller owns the canonical UUID upstream (e.g. the
        /// Android Gradle plugin's `BugseeBuildIdResolveTask` writes the same
        /// UUID into the SDK's asset channel — the upload-side UUID MUST match
        /// or crash symbolication never resolves). If the supplied value differs
        /// from what the CLI would have computed, a warning is logged but the
        /// override wins.
        #[arg(long)]
        uuid: Option<String>,

        /// Attach a launcher icon to the symbol zip. Matches the existing
        /// Gradle-plugin layout (entry name: `icon.<ext>` next to `mapping.txt`).
        #[arg(long)]
        icon: Option<PathBuf>,

        /// Disable Zstd compression (debug only — default is Zstd level 11).
        #[arg(long)]
        no_zstd: bool,

        /// Zstd level (1..=22). Defaults to 11; values below 9 are rejected.
        #[arg(long)]
        zstd_level: Option<i64>,

        /// Force a re-upload even if the server reports the file already exists.
        /// (Server still dedupes by hash; this flag is reserved for future "skip cache" use.)
        #[arg(long)]
        force: bool,

        /// Dry-run — discover and pack files but skip the HTTP upload.
        #[arg(long)]
        dry_run: bool,
    },

    /// Convert a debug-file to Bugsee's legacy BMF/BSF format (for existing deployments only).
    Convert {
        #[arg(required = true)]
        input: PathBuf,

        #[arg(long, value_enum)]
        to: LegacyFormat,

        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugFileType {
    Dsym,
    Elf,
    Pe,
    Pdb,
    PortablePdb,
    Breakpad,
    Proguard,
    Jvm,
    Sourcebundle,
    Wasm,
    /// JS source maps (React Native / web). Keyed by the debug-id embedded by
    /// `bugsee-cli sourcemaps inject` (or a caller-supplied `--uuid`).
    Sourcemaps,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum LegacyFormat {
    Bmf,
    Bsf,
}

pub async fn dispatch(
    cmd: DebugFilesCommand,
    endpoint: Option<String>,
    app_token: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        DebugFilesCommand::Upload {
            paths,
            r#type,
            version,
            build,
            uuid: uuid_override,
            icon,
            no_zstd,
            zstd_level,
            force: _,
            dry_run,
        } => {
            let kind = r#type.unwrap_or(DebugFileType::Proguard);
            match kind {
                DebugFileType::Proguard => {}
                DebugFileType::Elf => {}
                DebugFileType::Dsym => {}
                DebugFileType::Sourcemaps => {}
                other => {
                    return Err(config_invalid(format!(
                        "supported types are --type proguard, elf, dsym, and sourcemaps; \
                         got {other:?} (other formats are scaffold-only)"
                    )));
                }
            }

            let strategy = compress::resolve_strategy(no_zstd, zstd_level)?;

            let parsed_override = match uuid_override.as_deref() {
                None => None,
                Some(s) => Some(
                    Uuid::parse_str(s)
                        .map_err(|e| input_invalid(format!("--uuid is not a valid UUID: {e}")))?,
                ),
            };

            if kind != DebugFileType::Proguard && icon.is_some() {
                return Err(config_invalid(
                    "--icon is only valid for --type proguard (mapping files attach the launcher icon)",
                ));
            }
            if kind == DebugFileType::Dsym && parsed_override.is_some() {
                return Err(config_invalid(
                    "--uuid does not apply to --type dsym — the server extracts Mach-O UUIDs \
                     from the dSYM bundle itself, one per architecture slice",
                ));
            }

            if let Some(ref icon_path) = icon {
                if !icon_path.is_file() {
                    return Err(input_not_found(format!(
                        "--icon path does not exist or is not a file: {}",
                        icon_path.display()
                    )));
                }
            }

            let endpoint = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
            let app_token = if dry_run {
                app_token.unwrap_or_default()
            } else {
                app_token.ok_or_else(|| {
                    config_invalid(
                        "--app-token (or BUGSEE_APP_TOKEN) is required when not in --dry-run mode",
                    )
                })?
            };

            if kind == DebugFileType::Elf {
                let uuid_for_elf = parsed_override.ok_or_else(|| {
                    // ELF symbols carry no inherent UUID at the archive level — the
                    // SDK's runtime-reported BUILD_UUID (from the asset channel) is
                    // the only key that ties symbols back to a crash. The caller
                    // (the Gradle plugin's NativeUploadTask) owns this value.
                    config_invalid(
                        "--uuid is required when --type elf (the resolved BUILD_UUID from the SDK's asset channel)",
                    )
                })?;
                return run_elf_upload(
                    &paths,
                    &endpoint,
                    &app_token,
                    &version,
                    &build,
                    uuid_for_elf,
                    dry_run,
                )
                .await;
            }

            if kind == DebugFileType::Dsym {
                return run_dsym_upload(
                    &paths, &endpoint, &app_token, &version, &build, strategy, dry_run,
                )
                .await;
            }

            if kind == DebugFileType::Sourcemaps {
                return run_sourcemap_upload(
                    &paths,
                    &endpoint,
                    &app_token,
                    &version,
                    &build,
                    parsed_override,
                    strategy,
                    dry_run,
                )
                .await;
            }

            run_proguard_upload(
                &paths,
                &endpoint,
                &app_token,
                &version,
                &build,
                parsed_override,
                icon.as_deref(),
                strategy,
                dry_run,
            )
            .await
        }
        DebugFilesCommand::Convert { input, to, output } => {
            tracing::info!(
                ?input,
                ?to,
                ?output,
                "debug-files convert — not yet implemented"
            );
            anyhow::bail!("convert not yet implemented")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_proguard_upload(
    paths: &[PathBuf],
    endpoint: &str,
    app_token: &str,
    version: &str,
    build: &str,
    uuid_override: Option<Uuid>,
    icon: Option<&std::path::Path>,
    strategy: Strategy,
    dry_run: bool,
) -> anyhow::Result<()> {
    let candidates = discover_mappings(paths);
    if candidates.is_empty() {
        return Err(input_not_found(format!(
            "no mapping files found under: {}",
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let client = if dry_run {
        None
    } else {
        Some(http::build_client()?)
    };

    let mut uploaded = 0u32;
    let mut already_existed = 0u32;
    for mapping in candidates {
        tracing::info!(path = %mapping.display(), "processing mapping");
        let identity = proguard::identify(&mapping)?;
        let resolved_uuid = match uuid_override {
            Some(supplied) => {
                if supplied != identity.debug_id {
                    tracing::warn!(
                        supplied = %supplied,
                        computed = %identity.debug_id,
                        "supplied --uuid differs from the CLI-computed debug-id; using \
                         the supplied value (this is normal when the caller owns the \
                         UUID upstream, but may indicate algorithm drift if not)."
                    );
                } else {
                    tracing::debug!(
                        uuid = %supplied,
                        "supplied --uuid matches the CLI-computed debug-id"
                    );
                }
                supplied
            }
            None => identity.debug_id,
        };
        tracing::info!(
            uuid = %resolved_uuid,
            sha1 = %identity.content_sha1_hex,
            "identified"
        );

        let tmpdir = tempfile::tempdir()?;
        let zip_path = tmpdir.path().join(format!("{}.zip", resolved_uuid));

        // Build entry list. Order matches the existing Kotlin layout:
        // mapping.txt first, icon (if any) second.
        let icon_entry_name;
        let entries: Vec<ZipEntry<'_>> = if let Some(icon_path) = icon {
            let ext = icon_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin");
            icon_entry_name = format!("icon.{ext}");
            vec![
                ZipEntry::compressed("mapping.txt", &mapping),
                ZipEntry::compressed(&icon_entry_name, icon_path),
            ]
        } else {
            vec![ZipEntry::compressed("mapping.txt", &mapping)]
        };

        let zip_size = compress::pack_entries(&entries, &zip_path, strategy)?;
        tracing::info!(zip_size, entries = entries.len(), ?strategy, "packed");

        if dry_run {
            tracing::info!(
                "dry-run: would POST metadata + PUT {} ({} bytes)",
                zip_path.display(),
                zip_size
            );
            continue;
        }

        let resolved_uuid_str = resolved_uuid.to_string();
        let metadata = presigned::Metadata {
            uuid: Some(&resolved_uuid_str),
            version,
            build,
            hash: Some(&identity.content_sha1_hex),
            transform: None,
        };
        let client = client.as_ref().expect("client constructed when !dry_run");
        let outcome = presigned::upload(
            client,
            RetryPolicy::default(),
            endpoint,
            app_token,
            &metadata,
            &zip_path,
        )
        .await?;
        match outcome {
            presigned::Outcome::Uploaded => {
                uploaded += 1;
                tracing::info!(uuid = %resolved_uuid, "uploaded");
            }
            presigned::Outcome::AlreadyExists => {
                already_existed += 1;
                tracing::info!(uuid = %resolved_uuid, "already on server, skipped");
            }
        }
    }

    if dry_run {
        tracing::info!("dry-run complete");
    } else {
        tracing::info!(uploaded, already_existed, "upload complete");
    }
    Ok(())
}

fn discover_mappings(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_file() {
            // Explicit file — trust the user.
            out.push(p.clone());
            continue;
        }
        if !p.is_dir() {
            tracing::warn!(path = %p.display(), "path does not exist or is not a file/dir; skipping");
            continue;
        }
        for entry in WalkDir::new(p).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if proguard::looks_like_mapping_filename(name.as_ref()) {
                out.push(entry.into_path());
            }
        }
    }
    out
}

/// Discover `.map` source-map files under the given paths. Explicit file
/// arguments are trusted as-is (regardless of extension) so a caller can point
/// at a single non-`.map`-named map; directories are walked for `*.map`.
fn discover_sourcemaps(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_file() {
            out.push(p.clone());
            continue;
        }
        if !p.is_dir() {
            tracing::warn!(path = %p.display(), "path does not exist or is not a file/dir; skipping");
            continue;
        }
        for entry in WalkDir::new(p).into_iter().filter_map(|e| e.ok()) {
            if !entry.file_type().is_file() {
                continue;
            }
            if entry.path().extension().and_then(|e| e.to_str()) == Some("map") {
                out.push(entry.into_path());
            }
        }
    }
    out
}

/// Run the upload for one or more pre-packaged native-debug-symbols archives.
///
/// Phase 1 scope: each path must be a regular file (the AGP-produced
/// `native-debug-symbols.zip`). The CLI hashes the bytes, POSTs metadata
/// with `transform = breakpad`, then PUTs the file as-is — no Zstd
/// re-compression in this phase. Walking a directory of `.so` files is
/// the Gradle plugin's job (it pre-zips the
/// `build/intermediates/native_debug_metadata/<variant>/out` directory
/// before invoking the CLI).
#[allow(clippy::too_many_arguments)]
async fn run_elf_upload(
    paths: &[PathBuf],
    endpoint: &str,
    app_token: &str,
    version: &str,
    build: &str,
    build_uuid: Uuid,
    dry_run: bool,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Err(input_not_found("no input paths supplied"));
    }
    for p in paths {
        if !p.is_file() {
            // Directory-walk + re-pack is out of scope for Phase 1 — the
            // Gradle plugin pre-zips the intermediate-folder case before
            // invoking the CLI. Surface a clear config error rather than
            // attempting a half-supported flow.
            return Err(input_invalid(format!(
                "--type elf expects a pre-built zip file (typically AGP's \
                 native-debug-symbols.zip); got {}",
                p.display()
            )));
        }
    }

    let client = if dry_run {
        None
    } else {
        Some(http::build_client()?)
    };

    let build_uuid_str = build_uuid.to_string();
    let mut uploaded = 0u32;
    let mut already_existed = 0u32;
    for archive in paths {
        tracing::info!(path = %archive.display(), "processing native-debug-symbols archive");
        let identity = elf::identify(archive)?;
        tracing::info!(
            uuid = %build_uuid,
            sha1 = %identity.content_sha1_hex,
            size_bytes = identity.size_bytes,
            "identified"
        );

        if dry_run {
            tracing::info!(
                "dry-run: would POST metadata + PUT {} ({} bytes)",
                archive.display(),
                identity.size_bytes,
            );
            continue;
        }

        let metadata = presigned::Metadata {
            uuid: Some(&build_uuid_str),
            version,
            build,
            hash: Some(&identity.content_sha1_hex),
            transform: Some("breakpad"),
        };
        let client = client.as_ref().expect("client constructed when !dry_run");
        let outcome = presigned::upload(
            client,
            RetryPolicy::default(),
            endpoint,
            app_token,
            &metadata,
            archive,
        )
        .await?;
        match outcome {
            presigned::Outcome::Uploaded => {
                uploaded += 1;
                tracing::info!(uuid = %build_uuid, "uploaded");
            }
            presigned::Outcome::AlreadyExists => {
                already_existed += 1;
                tracing::info!(uuid = %build_uuid, "already on server, skipped");
            }
        }
    }

    if dry_run {
        tracing::info!("dry-run complete");
    } else {
        tracing::info!(uploaded, already_existed, "upload complete");
    }
    Ok(())
}

/// Pack and upload one or more JS source maps, keyed by their debug-id.
///
/// Each `.map` is keyed on the server by the debug-id `sourcemaps inject`
/// embedded (`debug_id` / `debugId`, legacy `uuid` fallback) — read back here
/// via [`sourcemap::identify`]. A map carrying no id is a hard error: the
/// caller must run `sourcemaps inject` first (or pass `--uuid` to key by a
/// caller-owned id). The map is packed as a single Zstd entry and uploaded
/// through the shared presigned protocol; the worker auto-detects the
/// `sourcemap` format from the unzipped JSON and re-derives the same debug-id
/// (`symbolfiles/sourcemap.py:parse`).
#[allow(clippy::too_many_arguments)]
async fn run_sourcemap_upload(
    paths: &[PathBuf],
    endpoint: &str,
    app_token: &str,
    version: &str,
    build: &str,
    uuid_override: Option<Uuid>,
    strategy: Strategy,
    dry_run: bool,
) -> anyhow::Result<()> {
    let candidates = discover_sourcemaps(paths);
    if candidates.is_empty() {
        return Err(input_not_found(format!(
            "no .map source-map files found under: {}",
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let client = if dry_run {
        None
    } else {
        Some(http::build_client()?)
    };

    let mut uploaded = 0u32;
    let mut already_existed = 0u32;
    for map_path in candidates {
        tracing::info!(path = %map_path.display(), "processing source map");
        let identity = sourcemap::identify(&map_path)?;

        // Resolve the keying id: an explicit --uuid override wins; otherwise the
        // embedded debug-id. A map with neither cannot be keyed — fail loudly
        // rather than uploading an unfindable symbol.
        let resolved_id = match uuid_override {
            Some(supplied) => {
                let supplied_str = supplied.to_string();
                if let Some(embedded) = identity.debug_id.as_deref() {
                    if embedded != supplied_str {
                        tracing::warn!(
                            supplied = %supplied_str,
                            embedded = %embedded,
                            "supplied --uuid differs from the map's embedded debug-id; \
                             using the supplied value"
                        );
                    }
                }
                supplied_str
            }
            None => identity.debug_id.clone().ok_or_else(|| {
                input_invalid(format!(
                    "source map has no debug_id/debugId/uuid: {} — run \
                     `bugsee-cli sourcemaps inject <bundle-dir>` first to embed one, \
                     or pass --uuid to key by a caller-owned id",
                    map_path.display()
                ))
            })?,
        };
        tracing::info!(
            debug_id = %resolved_id,
            sha1 = %identity.content_sha1_hex,
            size_bytes = identity.size_bytes,
            "identified"
        );

        let tmpdir = tempfile::tempdir()?;
        let zip_path = tmpdir.path().join("sourcemap.zip");

        // Single entry: just the `.map`. Keeping the zip to one file means the
        // worker's first-file extraction always lands on the map, and its
        // content-based format detection classifies it as `sourcemap`.
        let entry_name = map_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle.js.map");
        let entries = vec![ZipEntry::compressed(entry_name, &map_path)];
        let zip_size = compress::pack_entries(&entries, &zip_path, strategy)?;
        tracing::info!(zip_size, ?strategy, "packed");

        if dry_run {
            tracing::info!(
                "dry-run: would POST metadata + PUT {} ({} bytes)",
                zip_path.display(),
                zip_size
            );
            continue;
        }

        let metadata = presigned::Metadata {
            uuid: Some(&resolved_id),
            version,
            build,
            hash: Some(&identity.content_sha1_hex),
            transform: None,
        };
        let client = client.as_ref().expect("client constructed when !dry_run");
        let outcome = presigned::upload(
            client,
            RetryPolicy::default(),
            endpoint,
            app_token,
            &metadata,
            &zip_path,
        )
        .await?;
        match outcome {
            presigned::Outcome::Uploaded => {
                uploaded += 1;
                tracing::info!(debug_id = %resolved_id, "uploaded");
            }
            presigned::Outcome::AlreadyExists => {
                already_existed += 1;
                tracing::info!(debug_id = %resolved_id, "already on server, skipped");
            }
        }
    }

    if dry_run {
        tracing::info!("dry-run complete");
    } else {
        tracing::info!(uploaded, already_existed, "upload complete");
    }
    Ok(())
}

/// Pack and upload one or more Apple `.dSYM` bundles.
///
/// Phase 1 scope: each input path must be a `.dSYM` directory. Each bundle is
/// independently identified (UUIDs per Mach-O slice extracted for logging),
/// re-packed with the chosen compression strategy, and uploaded. The metadata
/// POST carries ONLY `version` + `build` — server-side `images[].uuid`
/// extraction matches BugseeAgent's wire protocol.
async fn run_dsym_upload(
    paths: &[PathBuf],
    endpoint: &str,
    app_token: &str,
    version: &str,
    build: &str,
    strategy: Strategy,
    dry_run: bool,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Err(input_not_found("no input paths supplied"));
    }
    for p in paths {
        if !p.is_dir() {
            return Err(input_invalid(format!(
                "--type dsym expects a `.dSYM` bundle (a directory); got {}",
                p.display()
            )));
        }
    }

    let client = if dry_run {
        None
    } else {
        Some(http::build_client()?)
    };

    let mut uploaded = 0u32;
    let mut already_existed = 0u32;
    for dsym_path in paths {
        tracing::info!(path = %dsym_path.display(), "processing dSYM bundle");

        let identity = dsym::identify(dsym_path)?;
        for slice in &identity.slices {
            tracing::info!(
                uuid = %slice.uuid,
                arch = %slice.arch,
                "extracted Mach-O slice",
            );
        }

        let entries = dsym::enumerate_bundle_entries(dsym_path)?;
        let zip_entries: Vec<ZipEntry<'_>> = entries
            .iter()
            .map(|(name, path)| ZipEntry::compressed(name.as_str(), path.as_path()))
            .collect();

        let tmpdir = tempfile::tempdir()?;
        let bundle_name = dsym_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("bundle.dSYM");
        let zip_path = tmpdir.path().join(format!("{}.zip", bundle_name));

        let zip_size = compress::pack_entries(&zip_entries, &zip_path, strategy)?;
        tracing::info!(
            zip_size,
            entries = zip_entries.len(),
            slices = identity.slices.len(),
            ?strategy,
            "packed",
        );

        if dry_run {
            tracing::info!(
                "dry-run: would POST metadata + PUT {} ({} bytes)",
                zip_path.display(),
                zip_size,
            );
            continue;
        }

        // dSYM metadata is intentionally minimal: only version + build.
        // The worker extracts Mach-O UUIDs from the zip via
        // `symbolic.debuginfo.Archive.iter_objects()` and stores one entry
        // per arch slice in `images[]`.
        let metadata = presigned::Metadata {
            uuid: None,
            version,
            build,
            hash: None,
            transform: None,
        };
        let client = client.as_ref().expect("client constructed when !dry_run");
        let outcome = presigned::upload(
            client,
            RetryPolicy::default(),
            endpoint,
            app_token,
            &metadata,
            &zip_path,
        )
        .await?;
        match outcome {
            presigned::Outcome::Uploaded => {
                uploaded += 1;
                tracing::info!(bundle = bundle_name, "uploaded");
            }
            presigned::Outcome::AlreadyExists => {
                already_existed += 1;
                tracing::info!(bundle = bundle_name, "already on server, skipped");
            }
        }
    }

    if dry_run {
        tracing::info!("dry-run complete");
    } else {
        tracing::info!(uploaded, already_existed, "upload complete");
    }
    Ok(())
}

#[cfg(test)]
mod sourcemap_upload_tests {
    use super::*;
    use std::io::Read;
    use wiremock::matchers::{header, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zip::ZipArchive;

    fn write(dir: &std::path::Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn discover_sourcemaps_walks_dirs_and_trusts_explicit_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a.js.map", b"{}");
        write(tmp.path(), "bundle.js", b"console.log(1)");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        write(&tmp.path().join("sub"), "c.map", b"{}");

        let mut found = discover_sourcemaps(&[tmp.path().to_path_buf()]);
        found.sort();
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            vec!["a.js.map", "c.map"],
            "only .map files discovered"
        );

        // An explicit non-.map file is trusted as-is.
        let explicit = write(tmp.path(), "weird-name", b"{}");
        let found2 = discover_sourcemaps(std::slice::from_ref(&explicit));
        assert_eq!(found2, vec![explicit]);
    }

    /// Drives the two-stage presigned upload and returns the captured metadata
    /// POST body plus the PUT'd zip bytes.
    async fn run_upload_capture(
        map_path: &std::path::Path,
        uuid_override: Option<Uuid>,
    ) -> (serde_json::Value, Vec<u8>) {
        let server = MockServer::start().await;
        let put_url = format!("{}/sourcemap-put", server.uri());

        Mock::given(method("POST"))
            .and(wm_path("/apps/TKN/symbols"))
            .and(header("X-Bugsee-Uploader", "cli"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "endpoint": put_url
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(wm_path("/sourcemap-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let uri = server.uri();
        run_sourcemap_upload(
            &[map_path.to_path_buf()],
            &uri,
            "TKN",
            "1.2.3",
            "42",
            uuid_override,
            Strategy::Zstd(11),
            false,
        )
        .await
        .unwrap();

        let received = server.received_requests().await.unwrap();
        let post = received
            .iter()
            .find(|r| r.url.path() == "/apps/TKN/symbols")
            .unwrap();
        let put = received
            .iter()
            .find(|r| r.url.path() == "/sourcemap-put")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&post.body).unwrap();
        (body, put.body.clone())
    }

    #[tokio::test]
    async fn keys_by_embedded_debug_id_and_puts_the_map() {
        let tmp = tempfile::tempdir().unwrap();
        let map = write(
            tmp.path(),
            "app.js.map",
            br#"{"version":3,"debug_id":"did-embedded","mappings":"AAAA"}"#,
        );

        let (body, put_zip) = run_upload_capture(&map, None).await;

        // Metadata keys the symbol by the embedded debug-id, with version/build.
        assert_eq!(body["uuid"], "did-embedded");
        assert_eq!(body["version"], "1.2.3");
        assert_eq!(body["build"], "42");
        // SHA-1 hash present for dedup.
        assert!(body["hash"].as_str().is_some_and(|h| h.len() == 40));

        // PUT body is a zip whose single entry is the map content, Zstd-compressed.
        let mut zip = ZipArchive::new(std::io::Cursor::new(put_zip)).unwrap();
        assert_eq!(zip.len(), 1);
        let mut entry = zip.by_name("app.js.map").unwrap();
        assert_eq!(entry.compression(), zip::CompressionMethod::Zstd);
        let mut got = String::new();
        entry.read_to_string(&mut got).unwrap();
        assert!(got.contains("\"debug_id\":\"did-embedded\""));
    }

    #[tokio::test]
    async fn uuid_override_wins_over_embedded() {
        let tmp = tempfile::tempdir().unwrap();
        let map = write(
            tmp.path(),
            "app.js.map",
            br#"{"version":3,"debug_id":"did-embedded","mappings":""}"#,
        );
        let override_id: Uuid = "11111111-1111-1111-1111-111111111111".parse().unwrap();

        let (body, _) = run_upload_capture(&map, Some(override_id)).await;
        assert_eq!(body["uuid"], "11111111-1111-1111-1111-111111111111");
    }

    #[tokio::test]
    async fn reads_legacy_top_level_uuid_when_no_debug_id() {
        let tmp = tempfile::tempdir().unwrap();
        let map = write(
            tmp.path(),
            "legacy.map",
            br#"{"version":3,"uuid":"legacy-sha1-key","mappings":""}"#,
        );
        let (body, _) = run_upload_capture(&map, None).await;
        assert_eq!(body["uuid"], "legacy-sha1-key");
    }

    #[tokio::test]
    async fn errors_when_map_has_no_id_and_no_override() {
        let tmp = tempfile::tempdir().unwrap();
        let map = write(tmp.path(), "nokey.map", br#"{"version":3,"mappings":""}"#);
        let err = run_sourcemap_upload(
            &[map],
            "http://127.0.0.1:1",
            "TKN",
            "1.0",
            "1",
            None,
            Strategy::Zstd(11),
            false,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no debug_id") && msg.contains("sourcemaps inject"),
            "error should guide the user to inject first: {msg}"
        );
    }

    #[tokio::test]
    async fn dry_run_packs_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let map = write(
            tmp.path(),
            "app.js.map",
            br#"{"version":3,"debug_id":"did-1","mappings":""}"#,
        );
        // Unroutable endpoint — a network call would error. dry-run must not make one.
        run_sourcemap_upload(
            &[map],
            "http://127.0.0.1:1",
            "TKN",
            "1.0",
            "1",
            None,
            Strategy::Zstd(11),
            true,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn errors_when_no_maps_found() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run_sourcemap_upload(
            &[tmp.path().to_path_buf()],
            "http://127.0.0.1:1",
            "TKN",
            "1.0",
            "1",
            None,
            Strategy::Zstd(11),
            true,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no .map source-map files found"));
    }
}
