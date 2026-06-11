use clap::{Subcommand, ValueEnum};
use std::path::PathBuf;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::compress::{self, Strategy, ZipEntry};
use crate::error::{config_invalid, input_invalid, input_not_found};
use crate::symbols::{dsym, elf, proguard};
use crate::upload::presigned;

const DEFAULT_ENDPOINT: &str = "https://api.bugsee.com";

#[derive(Subcommand, Debug)]
pub enum DebugFilesCommand {
    /// Discover, package, and upload debug information files from one or more paths.
    Upload {
        /// One or more directories or files to scan.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Restrict discovery to a specific debug-file type. If unset, defaults to `proguard`
        /// for v0.1; other types are still scaffold-only.
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
                other => {
                    return Err(config_invalid(format!(
                        "v0.1 supports --type proguard, --type elf, and --type dsym only; \
                         got {other:?} (other formats are scaffold-only)"
                    )));
                }
            }

            let strategy = resolve_strategy(no_zstd, zstd_level)?;

            let parsed_override = match uuid_override.as_deref() {
                None => None,
                Some(s) => Some(
                    Uuid::parse_str(s)
                        .map_err(|e| input_invalid(format!("--uuid is not a valid UUID: {e}")))?,
                ),
            };

            if matches!(kind, DebugFileType::Elf | DebugFileType::Dsym) && icon.is_some() {
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

fn resolve_strategy(no_zstd: bool, zstd_level: Option<i64>) -> anyhow::Result<Strategy> {
    if no_zstd {
        if zstd_level.is_some() {
            return Err(config_invalid(
                "--zstd-level is incompatible with --no-zstd",
            ));
        }
        return Ok(Strategy::Deflate);
    }
    let level = zstd_level.unwrap_or(compress::DEFAULT_ZSTD_LEVEL);
    if !(1..=22).contains(&level) {
        return Err(config_invalid(format!(
            "--zstd-level must be in 1..=22, got {level}"
        )));
    }
    if level < compress::MIN_PRODUCTION_ZSTD_LEVEL {
        return Err(config_invalid(format!(
            "--zstd-level {} is below the production floor of {}; pass --no-zstd if intentional",
            level,
            compress::MIN_PRODUCTION_ZSTD_LEVEL,
        )));
    }
    Ok(Strategy::Zstd(level))
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
        Some(presigned::build_client()?)
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
                ZipEntry {
                    name: "mapping.txt",
                    source: &mapping,
                },
                ZipEntry {
                    name: &icon_entry_name,
                    source: icon_path,
                },
            ]
        } else {
            vec![ZipEntry {
                name: "mapping.txt",
                source: &mapping,
            }]
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
        let outcome = presigned::upload(client, endpoint, app_token, &metadata, &zip_path).await?;
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
        Some(presigned::build_client()?)
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
        let outcome = presigned::upload(client, endpoint, app_token, &metadata, archive).await?;
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
        Some(presigned::build_client()?)
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
            .map(|(name, path)| ZipEntry {
                name: name.as_str(),
                source: path.as_path(),
            })
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
        let outcome = presigned::upload(client, endpoint, app_token, &metadata, &zip_path).await?;
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
