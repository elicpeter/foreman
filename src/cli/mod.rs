//! Clap command definitions and dispatch.
//!
//! Each subcommand currently returns `unimplemented!()`. They are filled in
//! across later phases (`init` in phase 5, `run` in phase 12, etc.).

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "foreman", version, about = "Orchestrate coding agents through a phased plan")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scaffold a new foreman workspace in the current directory.
    Init,
    /// Generate a `plan.md` for a goal using the planner agent.
    Plan {
        /// Free-form description of what to build.
        goal: String,
    },
    /// Execute the plan, advancing through phases until done or halted.
    Run,
    /// Print a summary of the current run.
    Status,
    /// Resume a halted run from where it left off.
    Resume,
}

/// Dispatch a parsed CLI invocation. All branches are stubs in phase 1.
pub async fn dispatch(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Init => unimplemented!("`foreman init` lands in phase 5"),
        Command::Plan { goal: _ } => unimplemented!("`foreman plan` lands in phase 15"),
        Command::Run => unimplemented!("`foreman run` lands in phase 12"),
        Command::Status => unimplemented!("`foreman status` lands in phase 17"),
        Command::Resume => unimplemented!("`foreman resume` lands in phase 17"),
    }
}
