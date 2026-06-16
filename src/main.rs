use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cli;
mod compress;
mod daemon;
mod error;
mod exit_code;
mod inject;
mod symbols;
mod upload;

use exit_code::ExitCode;

// NOT `#[tokio::main]`: the async runtime must be built AFTER the daemonization
// fork (forking a live multi-threaded runtime is undefined behaviour — only the
// forking thread survives, leaving the runtime deadlock-prone). So we parse,
// optionally double-fork, THEN build the runtime in the surviving process.
fn main() {
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // clap's pretty-printer handles --help/--version; for those we exit 0.
            let kind = e.kind();
            let _ = e.print();
            let code = match kind {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => {
                    ExitCode::Success
                }
                _ => ExitCode::Usage,
            };
            std::process::exit(code.as_i32());
        }
    };

    // Detach into the background BEFORE any thread/runtime exists. Only
    // `xcode post-action` (without --force-foreground) daemonizes; on success
    // the original process has already `exit(0)`'d and only the detached daemon
    // returns here. A fork failure falls back to foreground so the work still
    // runs (a slow archive beats a missing build record).
    if cli::should_daemonize(&cli) {
        if let Err(e) = daemon::daemonize(&daemon::log_path()) {
            eprintln!("bugsee-cli: could not daemonize ({e}); running in foreground");
        }
    }

    // Tracing emits to stderr only — stdout is RESERVED for the subcommand's
    // structured output (JSON for metadata subcommands, upload progress text for
    // debug-files). The Python integrators (fastlane plugin's `BugseeAgent`, iOS
    // SDK's `tools.bundle/BugseeAgent`) parse stdout exclusively, so info-level
    // chatter on stderr is invisible to them. In the daemon, stderr is the
    // redirected log file (see `daemon::redirect_standard_fds`).
    //
    // CONTRACT for future subcommand authors: never emit `println!`, `print!`,
    // or `tracing::*` to stdout from a metadata subcommand (vcs-metadata /
    // ios-deps / build-env / dsym / sourcemaps). Their stdout is parsed by
    // `json.loads(result.stdout)` on the Python side; a stray line would break
    // that. debug-files is the only subcommand intended for interactive use and
    // may emit progress text to stdout — it has no Python parser to break.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "bugsee_cli=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to start async runtime: {e}");
            std::process::exit(ExitCode::Unexpected.as_i32());
        }
    };

    let code = runtime.block_on(async {
        match cli::dispatch(cli).await {
            Ok(()) => ExitCode::Success,
            Err(err) => {
                eprintln!("error: {err:#}");
                error::classify(&err)
            }
        }
    });
    std::process::exit(code.as_i32());
}
