use clap::Parser;
use hashrate_multiplexer::{HashrateMultiplexerConfig, HashrateMultiplexerDaemon};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[clap(version, author, about = "Stratum V2 Hashrate Multiplexer")]
struct Args {
    /// Path to configuration file
    #[clap(short, long, default_value = "config.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hashrate_multiplexer=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Parse CLI arguments
    let args = Args::parse();

    // Load configuration
    let config = HashrateMultiplexerConfig::load_from_file(&args.config)?;

    // Create and run daemon
    let daemon = HashrateMultiplexerDaemon::new(config)?;
    daemon.run().await?;

    Ok(())
}
