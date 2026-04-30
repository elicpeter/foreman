use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use pitboss::cli::{self, Cli};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(&cli);
    cli::dispatch(cli).await
}

/// Configure `tracing-subscriber` from the CLI flags and environment.
///
/// Precedence (highest first):
/// 1. `PITBOSS_LOG` env var.
/// 2. `RUST_LOG` env var.
/// 3. `--verbose` / `-v` flag (`-v` → `debug`, `-vv`+ → `trace`).
/// 4. Built-in default (`info`).
///
/// Env vars win over `--verbose` so per-process tuning ("just this run, give
/// me trace on a single module") still works without removing the flag from a
/// shell wrapper.
fn init_tracing(cli: &Cli) {
    let filter = EnvFilter::try_from_env("PITBOSS_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new(cli.verbose_filter().unwrap_or("info")));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .init();
}
