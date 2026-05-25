//! `pitboss config` — interactive TUI wizard for `.pitboss/config.toml`.
//!
//! On a fresh workspace, walks the user through every config knob (models,
//! budget, sweeps, auditor, tests) and scaffolds the rest of `.pitboss/`.
//!
//! On an existing workspace, opens straight to a summary of the current
//! settings pre-populated from disk. From there the user can save (no-op)
//! or step back through every screen to edit individual values.
//!
//! Aliased as `pitboss setup` for backwards compatibility — the wizard-
//! execution body is exposed as the crate-internal helper
//! [`run_config_wizard`] so `pitboss start` can reuse the create flow.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::tui::wizard::{run_wizard, run_wizard_existing, WizardResult};
use crate::util::write_atomic;

/// Outcome of [`run_config_wizard`] — lets callers decide what trailing
/// messaging or follow-up commands to issue.
pub(crate) enum ConfigOutcome {
    /// User cancelled the wizard (Esc / Ctrl+C). No files were written.
    Cancelled,
    /// Wizard wrote `.pitboss/config.toml` (and, on a fresh workspace, the
    /// rest of the scaffolding).
    Completed,
}

/// Whether `pitboss config` is creating a workspace from scratch or editing
/// an existing one. Callers pass this in explicitly so `pitboss start` can
/// always request the create flow even if `.pitboss/` happens to exist.
pub(crate) enum ConfigMode {
    /// No `.pitboss/` present — scaffold directories, run the full wizard
    /// starting at Welcome, write all template content files.
    Create,
    /// `.pitboss/` already exists — pre-populate the wizard from the current
    /// `config.toml`, start on the summary screen, only rewrite
    /// `config.toml` on save.
    Edit,
}

pub async fn run(workspace: PathBuf) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "pitboss config requires an interactive terminal.\n\
             For non-interactive scaffolding, use `pitboss init`."
        );
    }

    let pitboss_dir = workspace.join(".pitboss");
    let mode = if pitboss_dir.is_dir() {
        ConfigMode::Edit
    } else {
        ConfigMode::Create
    };

    match run_config_wizard(&workspace, mode).await? {
        ConfigOutcome::Cancelled => {
            eprintln!("config cancelled — no changes written");
        }
        ConfigOutcome::Completed => {
            // Tight pointer at what to do next — the user just configured
            // pitboss, the obvious next thing is to write/edit a plan.
            println!("  Next: run `pitboss start` to create or edit a plan,");
            println!("        or edit .pitboss/play/plan.md directly.");
            println!();
        }
    }

    Ok(())
}

/// Run the config wizard end-to-end against `workspace` in either mode.
///
/// - [`ConfigMode::Create`] scaffolds directories, runs the full wizard
///   starting at Welcome, then writes `config.toml`, `plan.md`, and
///   `deferred.md`. Used by `pitboss config` on a fresh repo and by
///   `pitboss start`'s new-user branch.
/// - [`ConfigMode::Edit`] loads the current `config.toml`, opens the wizard
///   on the summary screen pre-populated from it, and rewrites only
///   `config.toml` on save. `plan.md` and `deferred.md` are untouched.
pub(crate) async fn run_config_wizard(workspace: &Path, mode: ConfigMode) -> Result<ConfigOutcome> {
    match mode {
        ConfigMode::Create => {
            // Full init scaffolding — directories, state.json, `.gitignore`
            // entry. Matches what `pitboss init` produces, minus the template
            // content files which the wizard writes itself below.
            super::init::scaffold_dirs(workspace)?;

            let result = match run_wizard(workspace).await? {
                Some(r) => r,
                None => return Ok(ConfigOutcome::Cancelled),
            };

            write_config(workspace, &result)?;
            write_plan(workspace, &result)?;
            write_deferred(workspace)?;
            print_workspace_summary(workspace)?;
            Ok(ConfigOutcome::Completed)
        }
        ConfigMode::Edit => {
            let cfg = crate::config::load(workspace)
                .with_context(|| format!("config: loading {:?}", workspace))?;

            let result = match run_wizard_existing(workspace, &cfg).await? {
                Some(r) => r,
                None => return Ok(ConfigOutcome::Cancelled),
            };

            // Rewrite config.toml from the (possibly edited) wizard result.
            // Skip plan.md / deferred.md — those are run content, not config.
            write_config(workspace, &result)?;
            println!();
            println!("  Updated .pitboss/config.toml");
            println!();

            // Re-run prereq check against the freshly-saved config in case
            // the user changed agent backend or other PATH-affecting bits.
            let cfg = crate::config::load(workspace)
                .with_context(|| format!("config: reloading {:?}", workspace))?;
            let prereqs = crate::cli::start::check_prereqs(workspace, &cfg);
            crate::cli::start::print_prereqs(&prereqs);

            Ok(ConfigOutcome::Completed)
        }
    }
}

/// "Workspace ready" panel printed after the create flow writes its files.
/// Mirrors what `pitboss init` produces so the user sees the full picture.
fn print_workspace_summary(workspace: &Path) -> Result<()> {
    println!();
    println!("  Workspace ready.");
    println!("    .pitboss/config.toml");
    println!("    .pitboss/play/plan.md");
    println!("    .pitboss/play/deferred.md");
    println!("    .pitboss/play/state.json");
    println!("    .pitboss/play/{{snapshots,logs}}/");
    println!("    .pitboss/grind/{{prompts,rotations,runs}}/");
    println!("    .gitignore  (added `.pitboss/`)");
    println!();

    let cfg = crate::config::load(workspace)
        .with_context(|| format!("config: loading {:?}", workspace))?;
    let prereqs = crate::cli::start::check_prereqs(workspace, &cfg);
    crate::cli::start::print_prereqs(&prereqs);
    Ok(())
}

/// Build the full `.pitboss/config.toml` body. Emits every config section
/// the runner reads, filling in pitboss's documented defaults for any value
/// the wizard didn't collect. This way the file is self-documenting: a user
/// can read it to see what every knob does and what value it currently
/// holds, without having to remember which defaults are baked into the
/// loader.
///
/// Concretely: when the user doesn't override `trigger_max_items` (the
/// wizard only asks for the min threshold), we still write
/// `trigger_max_items = N` to the file so the `min <= max` invariant
/// becomes visible — that's the bug that bit us when the wizard previously
/// wrote `trigger_min_items = 10` and let the loader supply the
/// invisible-default `8`.
fn write_config(workspace: &Path, result: &WizardResult) -> Result<()> {
    let planner = result.model_preset.planner();
    let worker = result.model_preset.worker();

    let audit_enabled = result.audit_enabled;
    let sweep_enabled = result.sweep_enabled;
    let sweep_min = result.sweep_threshold.unwrap_or(5);
    // Pitboss requires `trigger_min_items <= trigger_max_items` (default
    // max = 8). The wizard only collects min, so when the user picks a
    // threshold above 8 we lift max alongside it with a small buffer so
    // the agent still has headroom to drain a few extra items per sweep.
    let sweep_max = sweep_min.saturating_add(3).max(8);

    let caveman_intensity = "full"; // pitboss default; wizard doesn't ask
    let caveman_enabled = false;

    let mut config = format!(
        "# pitboss configuration — every section / field with its current\n\
         # value. Defaults shown explicitly so you can edit any knob without\n\
         # having to remember which value pitboss assumes for missing keys.\n\
         # See README for the full meaning of each setting.\n\
         \n\
         # ── Models ──────────────────────────────────────────────────────────\n\
         # Per-role model IDs. Strings pass verbatim to the agent CLI, so they\n\
         # must be valid model identifiers.\n\
         [models]\n\
         planner     = \"{planner}\"\n\
         implementer = \"{worker}\"\n\
         auditor     = \"{worker}\"\n\
         fixer       = \"{worker}\"\n\
         \n\
         # ── Retries ─────────────────────────────────────────────────────────\n\
         # Bounded — pitboss never loops forever. Once a budget is exhausted\n\
         # the runner halts with a clear reason.\n\
         [retries]\n\
         fixer_max_attempts = 2   # 0 disables the fixer entirely\n\
         max_phase_attempts = 3   # all roles combined per phase\n\
         \n\
         # ── Auditor ─────────────────────────────────────────────────────────\n\
         # The auditor reviews each phase's diff before commit. Small findings\n\
         # are inlined; larger ones go to deferred.md for the next sweep.\n\
         [audit]\n\
         enabled              = {audit_enabled}\n\
         small_fix_line_limit = 30   # diff-line threshold: inline vs. defer\n\
         \n\
         # ── Sweeps ──────────────────────────────────────────────────────────\n\
         # Sweeps drain deferred items between regular phases. Triggered when\n\
         # the unchecked-item count crosses trigger_min_items. trigger_max_items\n\
         # is the soft cap surfaced to the sweep agent's prompt. The runner\n\
         # enforces trigger_min_items <= trigger_max_items.\n\
         [sweep]\n\
         enabled                    = {sweep_enabled}\n\
         trigger_min_items          = {sweep_min}\n\
         trigger_max_items          = {sweep_max}\n\
         max_consecutive            = 1   # back-to-back sweeps before a real phase\n\
         escalate_after             = 3   # sweep attempts before staleness flag\n\
         audit_enabled              = true   # run auditor after each sweep too\n\
         final_sweep_enabled        = true   # drain loop after the final phase\n\
         final_sweep_max_iterations = 3   # cap on the drain loop\n\
         \n\
         # ── Git ─────────────────────────────────────────────────────────────\n\
         # The per-run branch is `<branch_prefix><utc_timestamp>`.\n\
         [git]\n\
         branch_prefix = \"pitboss/play/\"\n\
         create_pr     = false   # `true` opens a PR via `gh` after the final phase\n\
         \n\
         # ── Caveman mode ────────────────────────────────────────────────────\n\
         # Prepends a `talk-like-caveman` directive to every agent system\n\
         # prompt to cut output tokens. Intensity: lite | full | ultra.\n\
         [caveman]\n\
         enabled   = {caveman_enabled}\n\
         intensity = \"{caveman_intensity}\"\n\
         \n\
         # ── Grind ───────────────────────────────────────────────────────────\n\
         # Defaults for `pitboss grind` (rotating prompt runner).\n\
         [grind]\n\
         max_parallel              = 1\n\
         consecutive_failure_limit = 3\n\
         hook_timeout_secs         = 60\n"
    );

    // Tests section: always present so the user can see what command will be
    // used. Blank `command` = pitboss auto-detects from project layout.
    config.push_str(
        "\n\
         # ── Tests ───────────────────────────────────────────────────────────\n\
         # Pitboss runs this after every phase. Comment out / delete `command`\n\
         # to fall back to autodetection (cargo / npm / pytest / go test).\n\
         [tests]\n",
    );
    match &result.test_command_override {
        Some(cmd) => {
            let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
            config.push_str(&format!("command = \"{escaped}\"\n"));
        }
        None => {
            config.push_str(
                "# command = \"cargo test --workspace\"   # autodetected when commented\n",
            );
        }
    }

    // Budgets: always present, with caps written when set and commented
    // when not. The pricing tables match pitboss's built-in defaults so
    // anyone can see / edit per-model rates.
    config.push_str(
        "\n\
         # ── Budgets ─────────────────────────────────────────────────────────\n\
         # Setting either cap activates budget enforcement: the runner halts\n\
         # before the next dispatch that would exceed it.\n\
         [budgets]\n",
    );
    match result.max_total_usd {
        Some(usd) => config.push_str(&format!("max_total_usd    = {usd:.2}\n")),
        None => config.push_str("# max_total_usd    = 5.00     # uncomment to enforce a USD cap\n"),
    }
    match result.max_run_tokens {
        Some(tokens) => config.push_str(&format!("max_total_tokens = {tokens}\n")),
        None => {
            config.push_str("# max_total_tokens = 1_000_000   # uncomment to enforce a token cap\n")
        }
    }
    config.push_str(
        "\n\
         # Per-model price points (USD per million tokens). Override or add\n\
         # entries to teach pitboss about new / non-default models.\n\
         [budgets.pricing.claude-opus-4-7]\n\
         input_per_million_usd  = 15.0\n\
         output_per_million_usd = 75.0\n\
         \n\
         [budgets.pricing.claude-sonnet-4-6]\n\
         input_per_million_usd  = 3.0\n\
         output_per_million_usd = 15.0\n\
         \n\
         [budgets.pricing.claude-haiku-4-5]\n\
         input_per_million_usd  = 1.0\n\
         output_per_million_usd = 5.0\n",
    );

    let path = workspace.join(".pitboss/config.toml");
    write_atomic(&path, config.as_bytes()).with_context(|| format!("config: writing {:?}", path))
}

fn write_plan(workspace: &Path, _result: &WizardResult) -> Result<()> {
    // The wizard no longer collects a goal — write the canonical init seed
    // so the workspace has a parseable plan.md ready for the user to fill
    // in (or for `pitboss start` → New plan to overwrite).
    let path = workspace.join(".pitboss/play/plan.md");
    write_atomic(&path, super::init::PLAN_TEMPLATE.as_bytes())
        .with_context(|| format!("config: writing {:?}", path))
}

fn write_deferred(workspace: &Path) -> Result<()> {
    let path = workspace.join(".pitboss/play/deferred.md");
    if path.exists() {
        return Ok(());
    }
    write_atomic(&path, b"## Deferred items\n\n## Deferred phases\n")
        .with_context(|| format!("config: writing {:?}", path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::wizard::ModelPreset;
    use tempfile::tempdir;

    fn default_result() -> WizardResult {
        // Every field at its "user accepted the default" value, matching
        // what WizState::new produces before any input.
        WizardResult {
            model_preset: ModelPreset::Quality,
            max_run_tokens: None,
            max_total_usd: None,
            sweep_enabled: true,
            sweep_threshold: None,
            audit_enabled: true,
            test_command_override: None,
        }
    }

    /// The emitted config.toml must round-trip through `config::load` so the
    /// wizard never produces a file pitboss refuses to read.
    #[test]
    fn write_config_defaults_load_cleanly() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".pitboss")).unwrap();
        write_config(ws, &default_result()).expect("write_config");
        let cfg = crate::config::load(ws).expect("load round-trips");
        assert_eq!(cfg.sweep.trigger_min_items, 5);
        assert_eq!(cfg.sweep.trigger_max_items, 8);
        assert!(cfg.sweep.trigger_min_items <= cfg.sweep.trigger_max_items);
        assert_eq!(cfg.models.planner, "claude-opus-4-7");
        assert!(cfg.audit.enabled);
        assert!(cfg.sweep.enabled);
        assert_eq!(cfg.git.branch_prefix, "pitboss/play/");
    }

    /// Regression for the bug that triggered this rewrite: a high sweep
    /// threshold (10) must auto-bump `trigger_max_items` so the loader's
    /// `min <= max` validator doesn't bail.
    #[test]
    fn write_config_high_sweep_threshold_passes_validation() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".pitboss")).unwrap();
        let mut r = default_result();
        r.sweep_threshold = Some(10);
        write_config(ws, &r).expect("write_config");
        let cfg = crate::config::load(ws).expect("load round-trips");
        assert_eq!(cfg.sweep.trigger_min_items, 10);
        assert!(
            cfg.sweep.trigger_max_items >= cfg.sweep.trigger_min_items,
            "max ({}) must be >= min ({})",
            cfg.sweep.trigger_max_items,
            cfg.sweep.trigger_min_items
        );
    }

    /// User-supplied budget caps must reach the config; the missing-cap path
    /// must still be loadable (commented placeholders, not invalid TOML).
    #[test]
    fn write_config_with_budget_caps_load_cleanly() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".pitboss")).unwrap();
        let mut r = default_result();
        r.max_total_usd = Some(12.50);
        r.max_run_tokens = Some(1_000_000);
        write_config(ws, &r).expect("write_config");
        let cfg = crate::config::load(ws).expect("load round-trips");
        assert_eq!(cfg.budgets.max_total_usd, Some(12.50));
        assert_eq!(cfg.budgets.max_total_tokens, Some(1_000_000));
    }

    /// Setting a custom test command must surface in the loaded config.
    #[test]
    fn write_config_with_test_command_override_round_trips() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".pitboss")).unwrap();
        let mut r = default_result();
        r.test_command_override = Some("pnpm test --run".to_string());
        write_config(ws, &r).expect("write_config");
        let cfg = crate::config::load(ws).expect("load round-trips");
        assert_eq!(cfg.tests.command.as_deref(), Some("pnpm test --run"));
    }
}
