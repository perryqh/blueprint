use anyhow::Result;
use blueprint::auth::EnvFile;
use blueprint::cli;
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    // Parsed and passed down explicitly rather than installed into the process
    // environment. `set_var` would race this runtime's worker threads, which
    // `#[tokio::main]` has already spawned by the time we get here.
    let env = EnvFile::load()?;

    let args = cli::Cli::parse();
    cli::run(args, env).await
}
