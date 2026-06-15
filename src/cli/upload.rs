//! `bugsee-cli upload` — the unified build-time upload command tree.
//!
//! Phase A lands the `build-info` class (the per-build metadata bundle:
//! dependencies + timings + future sidecars). The `symbols` and `artefact`
//! classes are added in later passes; until then symbol uploads continue
//! through `debug-files upload`.

use clap::Subcommand;
use std::path::PathBuf;

use crate::compress;
use crate::error::{config_invalid, input_not_found};
use crate::upload::build;
use crate::upload::build_info::{self, Entry, Params};
use crate::upload::http::RetryPolicy;

const DEFAULT_ENDPOINT: &str = "https://api.bugsee.com";

#[derive(Subcommand, Debug)]
pub enum UploadCommand {
    /// Bundle per-build metadata sidecars (dependencies.json, timings.json,
    /// future *.json) into one zstd ZIP and upload with a single PUT.
    BuildInfo {
        /// Registration metadata JSON — the POST body for
        /// `/v2/apps/<token>/builds`. The CLI injects
        /// `request_build_info_upload: true` before POSTing. Required unless
        /// --upload-url is given.
        #[arg(long)]
        payload_json: Option<PathBuf>,

        /// Path to dependencies.json (packed as the `dependencies.json` entry).
        #[arg(long)]
        deps: Option<PathBuf>,

        /// Path to timings.json (packed as the `timings.json` entry).
        #[arg(long)]
        timings: Option<PathBuf>,

        /// Additional sidecar entry as `NAME=PATH` (repeatable). The bundle is
        /// additive; the worker tolerates unknown entry names.
        #[arg(long, value_name = "NAME=PATH")]
        sidecar: Vec<String>,

        /// PUT directly to this presigned URL and skip the registration POST.
        /// Use when the producer already registered the build and received the
        /// build-info upload URL in that response.
        #[arg(long)]
        upload_url: Option<String>,

        /// Disable Zstd compression (diagnostic only — default is Zstd level 11).
        #[arg(long)]
        no_zstd: bool,

        /// Zstd level (1..=22). Defaults to 11; values below 9 are rejected.
        #[arg(long)]
        zstd_level: Option<i64>,

        /// With --dry-run, write the would-be-uploaded ZIP to this path.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Pack the bundle but skip all network I/O. Combine with --out to
        /// inspect the exact bytes that would be uploaded.
        #[arg(long)]
        dry_run: bool,
    },

    /// Register a build and upload its artefact in one flow: POST the metadata
    /// to /v2/apps/<token>/builds, then PUT the artefact (STORED) + optional
    /// zstd mapping as the normalized upload ZIP. When --deps/--timings are
    /// given AND the org's build-info flag signs the endpoint, the build-info
    /// bundle ships from the same registration (no second POST). Single-PUT
    /// only; large artefacts use the chunked path (added separately).
    Build {
        /// Registration metadata JSON — the POST body for
        /// `/v2/apps/<token>/builds`. The CLI injects `request_artifact_upload`
        /// (and `request_build_info_upload` when sidecars are present).
        #[arg(long)]
        payload_json: PathBuf,

        /// Build artefact (`.aab`/`.apk`/`.ipa`), STORED verbatim in the ZIP.
        #[arg(long)]
        artifact: PathBuf,

        /// Optional R8/ProGuard mapping.txt, zstd-packed alongside the artefact.
        #[arg(long)]
        mapping: Option<PathBuf>,

        /// Optional dependencies.json sidecar (build-info bundle component).
        #[arg(long)]
        deps: Option<PathBuf>,

        /// Optional timings.json sidecar (build-info bundle component).
        #[arg(long)]
        timings: Option<PathBuf>,

        /// Disable Zstd compression (diagnostic only — default is Zstd level 11).
        #[arg(long)]
        no_zstd: bool,

        /// Zstd level (1..=22). Defaults to 11; values below 9 are rejected.
        #[arg(long)]
        zstd_level: Option<i64>,

        /// Upload the artefact via the chunked protocol (for large artefacts)
        /// instead of a single PUT.
        #[arg(long)]
        chunked: bool,

        /// With --dry-run, write the would-be-uploaded artefact ZIP to this path.
        #[arg(long)]
        out: Option<PathBuf>,

        /// Pack the upload ZIP but skip registration + all network I/O.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn dispatch(
    cmd: UploadCommand,
    endpoint: Option<String>,
    app_token: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        UploadCommand::BuildInfo {
            payload_json,
            deps,
            timings,
            sidecar,
            upload_url,
            no_zstd,
            zstd_level,
            out,
            dry_run,
        } => {
            let strategy = compress::resolve_strategy(no_zstd, zstd_level)?;

            // Assemble entries in a stable order: deps, timings, then sidecars
            // in CLI order. Names double as ZIP entry names the worker dispatches on.
            let mut entries: Vec<Entry> = Vec::new();
            if let Some(source) = deps {
                entries.push(Entry {
                    name: "dependencies.json".into(),
                    source,
                });
            }
            if let Some(source) = timings {
                entries.push(Entry {
                    name: "timings.json".into(),
                    source,
                });
            }
            for spec in &sidecar {
                let (name, source) = parse_sidecar(spec)?;
                entries.push(Entry { name, source });
            }

            if entries.is_empty() {
                return Err(config_invalid(
                    "nothing to upload: pass at least one of --deps / --timings / --sidecar",
                ));
            }

            // Reject duplicate entry names — the worker keys per-asset processing
            // on the entry name, so a collision would silently drop a sidecar.
            for i in 0..entries.len() {
                for j in (i + 1)..entries.len() {
                    if entries[i].name == entries[j].name {
                        return Err(config_invalid(format!(
                            "duplicate bundle entry name: {}",
                            entries[i].name
                        )));
                    }
                }
            }

            // Every source must exist and be a regular file.
            for e in &entries {
                if !e.source.is_file() {
                    return Err(input_not_found(format!(
                        "bundle entry source does not exist or is not a file: {} (entry: {})",
                        e.source.display(),
                        e.name
                    )));
                }
            }

            if out.is_some() && !dry_run {
                return Err(config_invalid("--out is only valid with --dry-run"));
            }

            let endpoint = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

            let params = Params {
                endpoint: &endpoint,
                app_token: app_token.as_deref(),
                payload_json: payload_json.as_deref(),
                upload_url: upload_url.as_deref(),
                entries: &entries,
                strategy,
                out: out.as_deref(),
                dry_run,
            };
            build_info::run(params, RetryPolicy::default()).await?;
            Ok(())
        }

        UploadCommand::Build {
            payload_json,
            artifact,
            mapping,
            deps,
            timings,
            no_zstd,
            zstd_level,
            chunked,
            out,
            dry_run,
        } => {
            let strategy = compress::resolve_strategy(no_zstd, zstd_level)?;
            if out.is_some() && !dry_run {
                return Err(config_invalid("--out is only valid with --dry-run"));
            }
            let app_token = app_token.as_deref().ok_or_else(|| {
                config_invalid("--app-token (or BUGSEE_APP_TOKEN) is required for `upload build`")
            })?;
            // Fail fast on missing optional sidecars (artefact itself is checked
            // in build::run); a missing file the producer named is a real error,
            // not a silently-skipped upload.
            for (label, p) in [
                ("--mapping", &mapping),
                ("--deps", &deps),
                ("--timings", &timings),
            ] {
                if let Some(path) = p {
                    if !path.is_file() {
                        return Err(input_not_found(format!(
                            "{label} does not exist or is not a file: {}",
                            path.display()
                        )));
                    }
                }
            }
            let endpoint = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
            let params = build::Params {
                endpoint: &endpoint,
                app_token,
                payload_json: &payload_json,
                artifact: &artifact,
                mapping: mapping.as_deref(),
                deps: deps.as_deref(),
                timings: timings.as_deref(),
                strategy,
                chunked,
                dry_run,
                out: out.as_deref(),
            };
            if let build::Outcome::Uploaded { build_id } =
                build::run(params, RetryPolicy::default()).await?
            {
                // stdout carries the build id so the producer can correlate.
                println!("{build_id}");
            }
            Ok(())
        }
    }
}

/// Parse a `NAME=PATH` sidecar spec. The name must be a clean filename (no path
/// separators / `..`) since it becomes a ZIP entry name the worker dispatches on.
fn parse_sidecar(spec: &str) -> anyhow::Result<(String, PathBuf)> {
    let (name, path) = spec
        .split_once('=')
        .ok_or_else(|| config_invalid(format!("--sidecar must be NAME=PATH, got: {spec}")))?;
    let name = name.trim();
    if name.is_empty() {
        return Err(config_invalid(format!(
            "--sidecar name is empty in: {spec}"
        )));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(config_invalid(format!(
            "--sidecar name must be a plain filename (no path separators): {name}"
        )));
    }
    if path.is_empty() {
        return Err(config_invalid(format!(
            "--sidecar path is empty in: {spec}"
        )));
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sidecar_accepts_clean_name() {
        let (name, path) = parse_sidecar("vcs.json=/tmp/vcs.json").unwrap();
        assert_eq!(name, "vcs.json");
        assert_eq!(path, PathBuf::from("/tmp/vcs.json"));
    }

    #[test]
    fn parse_sidecar_allows_equals_in_path() {
        // split_once stops at the first '=', so query-ish paths survive.
        let (name, path) = parse_sidecar("a.json=/tmp/x=y.json").unwrap();
        assert_eq!(name, "a.json");
        assert_eq!(path, PathBuf::from("/tmp/x=y.json"));
    }

    #[test]
    fn parse_sidecar_rejects_missing_separator() {
        assert!(parse_sidecar("vcs.json").is_err());
    }

    #[test]
    fn parse_sidecar_rejects_path_traversal_and_separators() {
        assert!(parse_sidecar("../evil=/tmp/x").is_err());
        assert!(parse_sidecar("a/b.json=/tmp/x").is_err());
        assert!(parse_sidecar("a\\b.json=/tmp/x").is_err());
    }

    #[test]
    fn parse_sidecar_rejects_empty_sides() {
        assert!(parse_sidecar("=/tmp/x").is_err());
        assert!(parse_sidecar("a.json=").is_err());
    }
}
