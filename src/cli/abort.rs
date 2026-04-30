//! `pitboss abort` — mark the active run aborted, optionally restoring the
//! pre-run branch.
//!
//! Sets `state.aborted = true` so subsequent `pitboss run` and `pitboss
//! resume` invocations refuse the workspace. The state file is preserved as a
//! breadcrumb (run id, branch, attempts, token usage) — clearing it is left to
//! the user, since deleting state is irreversible. With
//! `--checkout-original`, after the state update the original branch
//! recorded by `pitboss run` (when known) is checked out via the shell git
//! integration.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::git::{Git, ShellGit};
use crate::state;
use crate::style::{self, col};

/// Top-level entry point for the `abort` subcommand.
///
/// `checkout_original` controls whether HEAD is moved back to the
/// pre-run branch after the abort flag is persisted.
pub async fn run(workspace: PathBuf, checkout_original: bool) -> Result<()> {
    let c = style::use_color_stdout();
    let mut state = match state::load(&workspace)
        .with_context(|| format!("abort: loading state in {:?}", workspace))?
    {
        Some(s) => s,
        None => bail!(
            "no active run to abort: .pitboss/state.json is empty in {:?}",
            workspace
        ),
    };

    if state.aborted {
        // Idempotent: a second `pitboss abort` is not an error, but we still
        // honor `--checkout-original` so users can use it to restore the
        // branch even after a prior abort.
        let stdout = std::io::stdout();
        let mut h = stdout.lock();
        let _ = writeln!(
            h,
            "{} run {} on {} was already aborted",
            col(c, style::YELLOW, "warning:"),
            state.run_id,
            state.branch
        );
        if checkout_original {
            checkout_original_branch(&workspace, &state, c).await?;
        }
        return Ok(());
    }

    state.aborted = true;
    state::save(&workspace, Some(&state))
        .with_context(|| format!("abort: persisting state in {:?}", workspace))?;

    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    let _ = writeln!(
        h,
        "{} run {} on branch {}",
        col(c, style::BOLD_RED, "aborted"),
        state.run_id,
        state.branch
    );

    if checkout_original {
        checkout_original_branch(&workspace, &state, c).await?;
    }

    Ok(())
}

async fn checkout_original_branch(
    workspace: &Path,
    state: &state::RunState,
    c: bool,
) -> Result<()> {
    let Some(original) = state.original_branch.as_deref() else {
        bail!(
            "abort: --checkout-original requested but no original branch is recorded for run {}",
            state.run_id
        );
    };
    let git = ShellGit::new(workspace.to_path_buf());
    git.checkout(original)
        .await
        .with_context(|| format!("abort: checking out original branch {:?}", original))?;
    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    let _ = writeln!(
        h,
        "{} {} {}",
        col(c, style::GREEN, "checked out"),
        original,
        col(c, style::DIM, &format!("(was on {})", state.branch))
    );
    Ok(())
}
