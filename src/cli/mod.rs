use clap::{Parser, Subcommand};

pub mod build_env;
pub mod debug_files;
pub mod dsym;
pub mod ios_deps;
pub mod pack;
pub mod size_check;
pub mod sourcemaps;
pub mod upload;
pub mod vcs_metadata;
pub mod xcactivitylog;
pub mod xcode;
pub mod xcode_ipa;

#[derive(Parser, Debug)]
#[command(
    name = "bugsee-cli",
    version,
    about = "Bugsee CLI — symbol collection, conversion, and upload.",
    long_about = "Cross-platform tool that collects debug information files \
                  (dSYM, ELF, PE/PDB, Portable PDB, Breakpad, R8/ProGuard, JS sourcemaps), \
                  normalizes them to a debug-id keyed format, and uploads them to Bugsee."
)]
pub struct Cli {
    /// Bugsee API endpoint (defaults to https://api.bugsee.com).
    /// Used only by upload-flavoured subcommands (`debug-files
    /// upload`, `upload build`, `upload build-info`). Metadata-resolving
    /// subcommands (`vcs-metadata`, `ios-deps`, `build-env`, `dsym`,
    /// `sourcemaps inject`) do no network I/O and ignore this flag.
    #[arg(long, env = "BUGSEE_ENDPOINT", global = true)]
    pub endpoint: Option<String>,

    /// Bugsee app token. Required by `debug-files upload`, `upload build`,
    /// and `upload build-info`; ignored by every other subcommand. Kept
    /// `global` so the same env-var-driven invocation shape works
    /// across all subcommands without per-call conditional plumbing
    /// in Python integrators.
    #[arg(long, env = "BUGSEE_APP_TOKEN", global = true)]
    pub app_token: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage debug information files (symbols, mappings, PDBs, dSYMs).
    #[command(subcommand)]
    DebugFiles(debug_files::DebugFilesCommand),

    /// Manage JavaScript source maps (inject debug IDs). Upload the injected
    /// maps with `debug-files upload --type sourcemaps`.
    #[command(subcommand)]
    Sourcemaps(sourcemaps::SourcemapsCommand),

    /// Build-time upload command tree — the single canonical origin for
    /// Bugsee build-time uploads: `build` (artefact single-PUT or chunked,
    /// with registration and build-info) and `build-info` (per-build metadata
    /// bundle). Producers shell to this instead of maintaining their own HTTP
    /// client, compression, retry, chunking, and presigned-URL handshake.
    #[command(subcommand)]
    Upload(upload::UploadCommand),

    /// Pack a build artefact (+ optional R8/ProGuard mapping) into the
    /// normalized upload ZIP the worker's size-analysis job consumes. The
    /// artefact is STORED verbatim; the mapping is zstd-compressed (method 93).
    /// Local-only — the producer uploads the resulting ZIP itself. Lets the
    /// Gradle plugin delegate compression here instead of bundling zstd-jni.
    Pack(pack::PackArgs),

    /// Resolve VCS metadata (provider, commit_sha, branch, PR number, repo)
    /// from CI provider env vars or a `git` fallback. Outputs JSON to stdout.
    ///
    /// Consumed by the Bugsee fastlane plugin's BugseeAgent and the iOS SDK's
    /// `tools.bundle/BugseeAgent` as a single canonical resolver — both Python
    /// scripts previously duplicated the same provider-detection logic.
    VcsMetadata(vcs_metadata::VcsMetadataArgs),

    /// Collect iOS dependency graph from Podfile.lock / Package.resolved /
    /// Cartfile.resolved / linked vendored frameworks. Outputs JSON to stdout.
    ///
    /// Consumed by the Bugsee fastlane plugin's BugseeAgent and the iOS SDK's
    /// `tools.bundle/BugseeAgent` as a single canonical parser — both Python
    /// scripts previously had near-identical implementations of all four
    /// parsers + the merger.
    #[command(subcommand)]
    IosDeps(ios_deps::IosDepsCommand),

    /// Build environment resolvers — Xcode version, CI-aware machine label,
    /// Info.plist reader. Each subcommand prints its result to stdout (empty
    /// string / empty object on unresolved).
    ///
    /// Consumed by both Python BugseeAgents to eliminate the duplicated
    /// in-process helpers for these three concerns.
    #[command(subcommand)]
    BuildEnv(build_env::BuildEnvCommand),

    /// dSYM utilities — UUID extraction from a `.dSYM` bundle or a single
    /// Mach-O binary. Replaces the `dwarfdump -u` shell-outs in both Python
    /// BugseeAgents with one canonical Rust implementation keyed off the
    /// `symbolic-debuginfo` Mach-O parser.
    #[command(subcommand)]
    Dsym(dsym::DsymCommand),

    /// Xcode build-phase orchestration. `post-action` reads the Xcode build
    /// environment (exported as env vars by a "Run Script" build phase) and
    /// sequences the build-publish ops the CLI already owns — build-info
    /// registration + bundle upload and dSYM upload — so the iOS SDK's
    /// `tools.bundle/BugseeAgent` can delegate the whole flow to one command.
    #[command(subcommand)]
    Xcode(xcode::XcodeCommand),
}

/// Whether this invocation should detach into a background daemon BEFORE any
/// work (and before the async runtime starts — forking a live multi-threaded
/// runtime is unsafe).
///
/// Only `xcode post-action` daemonizes — the iOS post-action context, where the
/// archive must return fast. Every other command, including the user-facing
/// `debug-files upload` (which a developer may run directly from a terminal),
/// stays in the foreground. `--force-foreground` opts the post-action back into
/// synchronous execution so a size-check FAIL can gate CI. Always `false` on
/// non-unix (no `fork`).
pub fn should_daemonize(cli: &Cli) -> bool {
    if !cfg!(unix) {
        return false;
    }
    matches!(
        &cli.command,
        Command::Xcode(xcode::XcodeCommand::PostAction {
            force_foreground: false,
            ..
        })
    )
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::DebugFiles(cmd) => debug_files::dispatch(cmd, cli.endpoint, cli.app_token).await,
        Command::Sourcemaps(cmd) => sourcemaps::dispatch(cmd, cli.endpoint, cli.app_token).await,
        Command::Upload(cmd) => upload::dispatch(cmd, cli.endpoint, cli.app_token).await,
        Command::Pack(args) => pack::dispatch(args),
        Command::VcsMetadata(args) => vcs_metadata::dispatch(args),
        Command::IosDeps(cmd) => ios_deps::dispatch(cmd),
        Command::BuildEnv(cmd) => build_env::dispatch(cmd),
        Command::Dsym(cmd) => dsym::dispatch(cmd),
        Command::Xcode(cmd) => xcode::dispatch(cmd, cli.endpoint, cli.app_token).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("valid args")
    }

    #[cfg_attr(not(unix), ignore)]
    #[test]
    fn post_action_daemonizes_by_default() {
        let cli = parse(&["bugsee-cli", "xcode", "post-action"]);
        assert!(should_daemonize(&cli));
    }

    #[cfg_attr(not(unix), ignore)]
    #[test]
    fn post_action_force_foreground_stays_synchronous() {
        let cli = parse(&["bugsee-cli", "xcode", "post-action", "--force-foreground"]);
        assert!(!should_daemonize(&cli));
    }

    #[test]
    fn debug_files_upload_never_daemonizes() {
        // The user-facing dSYM/symbol upload must stay foreground even by
        // default — a developer may run it directly from a terminal.
        let cli = parse(&[
            "bugsee-cli",
            "debug-files",
            "upload",
            "--type",
            "dsym",
            "--app-token",
            "TKN",
            "--version",
            "1.0",
            "--build",
            "1",
            ".",
        ]);
        assert!(!should_daemonize(&cli));
    }

    #[test]
    fn metadata_commands_never_daemonize() {
        let cli = parse(&["bugsee-cli", "vcs-metadata"]);
        assert!(!should_daemonize(&cli));
    }
}
