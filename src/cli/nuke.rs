//! `pitboss nuke` — completely remove pitboss from the workspace.
//!
//! Deletes the `.pitboss/` directory (config, plan, deferred items, state,
//! per-run logs, grind state — everything) after an explicit `y/N` prompt.
//! Refuses to run when there's nothing to delete. Leaves the `.pitboss/`
//! entry in `.gitignore` alone — adding it again on a future `pitboss
//! config` would be a no-op anyway, and the entry shouldn't matter to
//! anything else.

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

pub async fn run(workspace: PathBuf) -> Result<()> {
    let pitboss_dir = workspace.join(".pitboss");

    if !pitboss_dir.is_dir() {
        anyhow::bail!(
            "nothing to nuke: {:?} has no `.pitboss/` directory.",
            workspace
        );
    }

    // Show what's about to die so the user can sanity-check before
    // confirming. Lists the workspace path + the top-level contents that
    // are about to be removed.
    println!();
    println!("  About to delete:");
    println!("    {}", pitboss_dir.display());
    if let Ok(entries) = fs::read_dir(&pitboss_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let suffix = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                "/"
            } else {
                ""
            };
            println!("      .pitboss/{}{}", name, suffix);
        }
    }
    println!();
    println!("  This removes all config, plan content, deferred items, run state,");
    println!("  agent logs, and grind state. The action cannot be undone.");
    println!();

    if !confirm()? {
        eprintln!("nuke cancelled — nothing removed");
        return Ok(());
    }

    fs::remove_dir_all(&pitboss_dir)
        .with_context(|| format!("nuke: removing {:?}", pitboss_dir))?;

    println!();
    println!("  Pitboss deleted from project.");
    println!("  Run `pitboss config` to add it again.");
    println!();
    Ok(())
}

/// Prompt the user with `Are you sure you want to delete? (y/N): ` and
/// return `true` only when the answer starts with `y`/`Y`. Empty answer or
/// any other input is treated as no. Non-interactive stdin (piped input)
/// bails — there's no safe default for a destructive action without an
/// explicit human.
fn confirm() -> Result<bool> {
    if !is_interactive() {
        anyhow::bail!("pitboss nuke requires an interactive terminal to confirm the deletion.");
    }

    print!("  Are you sure you want to delete? (y/N): ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("nuke: reading confirmation")?;
    let answer = line.trim().to_lowercase();
    Ok(answer.starts_with('y'))
}

#[cfg(not(test))]
fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

// In `cargo test`, stdin is still wired to the real TTY, so the production
// `is_terminal()` check returns true and `read_line` blocks forever. Force
// non-interactive under cfg(test) so the bail path is exercisable.
#[cfg(test)]
fn is_interactive() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn bails_when_no_pitboss_dir() {
        let dir = tempdir().unwrap();
        let err = run(dir.path().to_path_buf()).await.unwrap_err();
        assert!(
            err.to_string().contains("nothing to nuke"),
            "expected 'nothing to nuke' message, got: {err}"
        );
    }

    #[tokio::test]
    async fn refuses_non_interactive_when_pitboss_exists() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".pitboss/play")).unwrap();
        // No TTY in tests — the confirm step must refuse rather than nuke
        // by accident.
        let err = run(dir.path().to_path_buf()).await.unwrap_err();
        assert!(
            err.to_string().contains("interactive terminal"),
            "expected interactive-terminal bail, got: {err}"
        );
        // Critical: .pitboss/ must still exist after the bail.
        assert!(dir.path().join(".pitboss").is_dir());
    }
}
