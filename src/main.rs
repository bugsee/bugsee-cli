use clap::Parser;
use tracing_subscriber::EnvFilter;

mod cli;
mod compress;
mod error;
mod exit_code;
mod inject;
mod symbols;
mod upload;

use exit_code::ExitCode;

#[tokio::main]
async fn main() {
    // Tracing emits to stderr only — stdout is RESERVED for the
    // subcommand's structured output (JSON for metadata subcommands,
    // upload progress text for debug-files). The Python integrators
    // (fastlane plugin's `BugseeAgent`, iOS SDK's
    // `tools.bundle/BugseeAgent`) parse stdout exclusively, so info-
    // level chatter on stderr is invisible to them.
    //
    // CONTRACT for future subcommand authors: never emit `println!`,
    // `print!`, or `tracing::*` to stdout from a metadata subcommand
    // (vcs-metadata / ios-deps / build-env / dsym / sourcemaps).
    // Their stdout is parsed by `json.loads(result.stdout)` on the
    // Python side; a stray line would break that. debug-files is the
    // only subcommand intended for interactive use and may emit
    // progress text to stdout — it has no Python parser to break.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "bugsee_cli=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

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

    let code = match cli::dispatch(cli).await {
        Ok(()) => ExitCode::Success,
        Err(err) => {
            eprintln!("error: {err:#}");
            error::classify(&err)
        }
    };
    std::process::exit(code.as_i32());
}
