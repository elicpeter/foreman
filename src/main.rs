use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use pitboss::cli::{self, Cli};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(&cli);
    match cli::dispatch(cli).await {
        Ok(code) => code.into_process(),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
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

    let registry = tracing_subscriber::registry().with(filter);
    if cli.is_tui_mode() {
        // Skip the fmt layer entirely. The TUI owns the alternate screen and
        // any concurrent stdout/stderr writes corrupt ratatui's cell-diff
        // (cells it thinks are unchanged stop getting repainted), leaving
        // stale log fragments stuck on the dashboard. The runner publishes
        // its progress on a broadcast channel that the TUI subscribes to, so
        // nothing is lost; drop `--tui` if you need the log stream.
        registry.init();
    } else {
        registry.with(fmt::layer().with_target(false)).init();
    }
}
