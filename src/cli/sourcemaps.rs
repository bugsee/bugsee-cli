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
    /// already-injected files is a no-op. A bundle that already carries a debug ID another
    /// tool wrote (e.g. Rollup's `output.sourcemapDebugIds`) keeps that ID and only gains
    /// the runtime registration.
    ///
    /// Upload the injected maps with `bugsee-cli debug-files upload --type sourcemaps`.
    Inject {
        /// One or more directories or files to inject (typically a JS dist output folder).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Glob of files NOT to touch; repeatable. Matched against the path relative to each
        /// walked root and against the full path, so `--exclude '**/node_modules/**'` keeps
        /// `inject` out of vendored code inside a build output (a Nuxt `.output/server` carries
        /// 22 such `.mjs`), and `--exclude 'polyfills*.js'` skips one file by name.
        ///
        /// NOTE: a bundle with no source map is still stamped by design. A crash frame carrying a
        /// debug-id whose map was never uploaded marks the report `missing_sym`, which is what
        /// prompts you to upload it; an unstamped bundle is silently unsymbolicated instead.
        #[arg(long)]
        exclude: Vec<String>,

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
        SourcemapsCommand::Inject {
            paths,
            exclude,
            dry_run,
        } => {
            let stats = inject::inject_paths(&paths, &exclude, dry_run)?;
            tracing::info!(
                js_injected = stats.js_injected,
                js_already_injected = stats.js_already,
                js_restamped = stats.js_restamped,
                js_registered = stats.js_registered,
                js_excluded = stats.js_excluded,
                maps_updated = stats.maps_updated,
                dry_run,
                "sourcemaps inject complete"
            );
            Ok(())
        }
    }
}
