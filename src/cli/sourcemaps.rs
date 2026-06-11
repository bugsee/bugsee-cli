use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum SourcemapsCommand {
    /// Inject a deterministic debug ID into JS bundles and their corresponding source maps.
    ///
    /// Rewrites every `.js`/`.cjs`/`.mjs` file to append a `//# debugId=<uuid>` comment plus a
    /// tiny runtime stub that registers the debug ID with `globalThis._bugseeDebugIds`, and
    /// rewrites every matching `.map` file to embed the same `debug_id`. Re-running on
    /// already-injected files is a no-op.
    Inject {
        /// One or more directories or files to inject (typically a JS dist output folder).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Dry-run — report what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Upload JS source maps (and their compiled bundles) to Bugsee.
    ///
    /// Matching at lookup time is by debug ID — `inject` must have run beforehand. No release
    /// name or dist value is required.
    Upload {
        /// One or more directories or files to upload.
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Disable Zstd compression (debug only — default is Zstd level 11).
        #[arg(long)]
        no_zstd: bool,
    },
}

pub async fn dispatch(
    cmd: SourcemapsCommand,
    _endpoint: Option<String>,
    _app_token: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        SourcemapsCommand::Inject { paths, dry_run } => {
            tracing::info!(
                ?paths,
                dry_run,
                "sourcemaps inject — implementation pending"
            );
            anyhow::bail!("not yet implemented");
        }
        SourcemapsCommand::Upload { paths, .. } => {
            tracing::info!(?paths, "sourcemaps upload — implementation pending");
            anyhow::bail!("not yet implemented");
        }
    }
}
