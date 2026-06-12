use clap::{Parser, Subcommand};

pub mod build_env;
pub mod debug_files;
pub mod ios_deps;
pub mod sourcemaps;
pub mod vcs_metadata;

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
    #[arg(long, env = "BUGSEE_ENDPOINT", global = true)]
    pub endpoint: Option<String>,

    /// Bugsee app token (required by most commands).
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

    /// Manage JavaScript source maps (inject debug IDs, upload).
    #[command(subcommand)]
    Sourcemaps(sourcemaps::SourcemapsCommand),

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
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::DebugFiles(cmd) => debug_files::dispatch(cmd, cli.endpoint, cli.app_token).await,
        Command::Sourcemaps(cmd) => sourcemaps::dispatch(cmd, cli.endpoint, cli.app_token).await,
        Command::VcsMetadata(args) => vcs_metadata::dispatch(args),
        Command::IosDeps(cmd) => ios_deps::dispatch(cmd),
        Command::BuildEnv(cmd) => build_env::dispatch(cmd),
    }
}
