use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use foreman::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    cli::dispatch(cli).await
}

/// Configure `tracing-subscriber` from the environment.
///
/// `FOREMAN_LOG` takes precedence over `RUST_LOG` so users can target the
/// foreman binary without touching the broader Rust ecosystem default.
fn init_tracing() {
    let filter = EnvFilter::try_from_env("FOREMAN_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .init();
}
