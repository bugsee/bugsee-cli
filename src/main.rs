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
