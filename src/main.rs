use anyhow::Result;
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

    // Pulls ~/.blueprint/env into the process environment (GitHub OAuth creds,
    // SESSION_SECRET, OAUTH_CALLBACK_URL, BLUEPRINT_PORT). Missing file = auth disabled.
    blueprint::auth::load_env_file()?;

    let args = cli::Cli::parse();
    cli::run(args).await
}
