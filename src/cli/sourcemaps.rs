use clap::Subcommand;
use std::path::PathBuf;

use crate::inject;

#[derive(Subcommand, Debug)]
pub enum SourcemapsCommand {
    /// Inject a deterministic debug ID into JS bundles and their corresponding source maps.
    ///
    /// Rewrites every `.js`/`.cjs`/`.mjs` file to append a `//# debugId=<uuid>` comment plus a
    /// tiny runtime stub that registers the debug ID with `globalThis._bugseeDebugIds`, and
    /// rewrites every matching `.map` file to embed the same `debug_id`. Re-running on
    /// already-injected files is a no-op.
    ///
    /// Upload the injected maps with `bugsee-cli debug-files upload --type sourcemaps`.
    Inject {
        /// One or more directories or files to inject (typically a JS dist output folder).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Dry-run — report what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
}

pub async fn dispatch(
    cmd: SourcemapsCommand,
    _endpoint: Option<String>,
    _app_token: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        SourcemapsCommand::Inject { paths, dry_run } => {
            let stats = inject::inject_paths(&paths, dry_run)?;
            tracing::info!(
                js_injected = stats.js_injected,
                js_already_injected = stats.js_already,
                maps_updated = stats.maps_updated,
                dry_run,
                "sourcemaps inject complete"
            );
            Ok(())
        }
    }
}
