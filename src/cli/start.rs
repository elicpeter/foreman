//! `pitboss start` — universal entry point.
//!
//! Auto-detects whether `.pitboss/` exists in the current directory:
//!
//! - **Missing:** runs the new-user setup wizard (shared with `pitboss setup`)
//!   and, if the user opted into AI plan generation, chains into
//!   `pitboss play --tui`. End-to-end install → configure → plan → play in
//!   one command.
//! - **Present:** runs the iteration wizard ([`crate::tui::iteration`]) which
//!   shows a snapshot of the current run (budget used, phase progress,
//!   deferred items) and offers continue / sweep / new-plan paths, each with
//!   an optional reset-budget toggle.
//!
//! This command is purely additive: it composes existing pitboss commands
//! (`setup`, `plan`, `play`, `rebuy`, `sweep`). It does not change their
//! behavior or schemas.

use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::agent::{self, Agent, AgentEvent, AgentRequest, Role, StopReason};
use crate::config::{self, Config};
use crate::deferred;
use crate::plan::{self, Plan};
use crate::prompts;
use crate::runner;
use crate::state::{self, TokenUsage};
use crate::tui::iteration::{run_iteration_wizard, IterationOutcome, WorkspaceSnapshot};
use crate::util::{paths, write_atomic};

use super::config::{run_config_wizard, ConfigMode, ConfigOutcome};
use super::init::PLAN_TEMPLATE;
use super::sweep::SweepArgs;

/// Wall-clock cap for an in-wizard questioner dispatch. Mirrors
/// `interview::QUESTIONER_TIMEOUT` — short text-only task.
const QUESTIONER_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Wall-clock cap for an in-wizard planner dispatch. Mirrors
/// `plan::PLANNER_TIMEOUT`.
const PLANNER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub async fn run(workspace: PathBuf) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "pitboss start requires an interactive terminal.\n\
             For non-interactive scaffolding, use `pitboss init` followed by `pitboss plan` and `pitboss play`."
        );
    }

    let pitboss_dir = workspace.join(".pitboss");
    if pitboss_dir.is_dir() {
        run_existing_user_flow(workspace).await
    } else {
        run_new_user_flow(workspace).await
    }
}

/// New-user branch: scaffold the workspace via the shared setup wizard.
/// The user always writes (or edits) `plan.md` themselves after setup —
/// `pitboss start` doesn't dispatch the planner agent here. To AI-generate a
/// plan they can re-run `pitboss start` once the workspace exists; the
/// iteration wizard's "New plan" path runs the planner (with an optional
/// design interview).
async fn run_new_user_flow(workspace: PathBuf) -> Result<()> {
    match run_config_wizard(&workspace, ConfigMode::Create).await? {
        ConfigOutcome::Cancelled => {
            eprintln!("start cancelled — no files written");
        }
        ConfigOutcome::Completed => {
            println!("  Next steps:");
            println!("    1. Edit .pitboss/play/plan.md to describe the phases.");
            println!("    2. Run `pitboss start` again — the iteration wizard");
            println!("       can also generate plan.md from a goal via the");
            println!("       planner agent (New plan → enable interview).");
            println!();
        }
    }
    Ok(())
}

/// Existing-user branch: build a workspace snapshot, open the iteration
/// wizard, dispatch the chosen outcome.
async fn run_existing_user_flow(workspace: PathBuf) -> Result<()> {
    let snapshot = load_snapshot(&workspace)?;
    let had_state = !snapshot.no_state;

    // The wizard's "new plan" path needs an agent to call the questioner +
    // planner inside the TUI. We build it once and hand it in as an `Arc`
    // so it can be cheaply cloned into spawned tasks.
    let cfg = config::load(&workspace)
        .with_context(|| format!("start: loading config in {:?}", workspace))?;
    let agent: Arc<dyn Agent + Send + Sync> = Arc::from(agent::build_agent(&cfg)?);

    let outcome = match run_iteration_wizard(&snapshot, &workspace, &cfg, agent.clone()).await? {
        Some(o) => o,
        None => {
            eprintln!("start cancelled — no changes made");
            return Ok(());
        }
    };

    dispatch_outcome(workspace, outcome, had_state, &cfg).await
}

/// Read config, state, plan, and deferred from `workspace` and project them
/// into the flat [`WorkspaceSnapshot`] the iteration wizard renders.
///
/// Missing or malformed `plan.md` / `state.json` is handled gracefully — the
/// snapshot exposes `no_plan` / `no_state` flags so the wizard can adjust the
/// available actions without bailing.
fn load_snapshot(workspace: &Path) -> Result<WorkspaceSnapshot> {
    let config = config::load(workspace)
        .with_context(|| format!("start: loading config in {:?}", workspace))?;
    let state = state::load(workspace)
        .with_context(|| format!("start: loading state in {:?}", workspace))?;

    // Plan: tolerate missing or malformed files so the wizard can offer
    // "New plan" as a recovery path.
    let plan_path = paths::plan_path(workspace);
    let plan = match fs::read_to_string(&plan_path) {
        Ok(text) => plan::parse(&text).ok(),
        Err(_) => None,
    };

    // Deferred: empty file or missing file → zero pending items.
    let deferred_path = paths::deferred_path(workspace);
    let deferred_count = match fs::read_to_string(&deferred_path) {
        Ok(text) => deferred::parse(&text)
            .map(|doc| doc.items.iter().filter(|i| !i.done).count())
            .unwrap_or(0),
        Err(_) => 0,
    };

    let (tokens_used, usd_used) = state
        .as_ref()
        .map(|s| runner::budget_totals(&config, &s.token_usage))
        .unwrap_or((0, 0.0));

    let (total_phases, current_phase) = match &plan {
        Some(p) => (p.phases.len(), Some(p.current_phase.clone())),
        None => (0, None),
    };

    Ok(WorkspaceSnapshot {
        branch: state.as_ref().map(|s| s.branch.clone()),
        current_phase,
        completed_count: state.as_ref().map(|s| s.completed.len()).unwrap_or(0),
        total_phases,
        tokens_used,
        tokens_cap: config.budgets.max_total_tokens,
        usd_used,
        usd_cap: config.budgets.max_total_usd,
        deferred_count,
        planner_model: config.models.planner.clone(),
        implementer_model: config.models.implementer.clone(),
        no_state: state.is_none(),
        no_plan: plan.is_none(),
    })
}

/// Run the outcome the user picked in the iteration wizard. For Continue and
/// Sweep we delegate to existing commands; for the NewPlan* variants, the
/// wizard has already generated `plan.md` (and possibly archived it), so we
/// just launch play, print a summary, or print a recovery hint.
async fn dispatch_outcome(
    workspace: PathBuf,
    outcome: IterationOutcome,
    had_state: bool,
    cfg: &Config,
) -> Result<()> {
    match outcome {
        IterationOutcome::Continue { reset_budget } => {
            if reset_budget {
                reset_budget_in_state(&workspace)?;
            }
            if had_state {
                super::rebuy::run(workspace, true, false, false, false, false).await
            } else {
                super::play::run(workspace, true, false, false, false, false).await
            }
        }
        IterationOutcome::Sweep { reset_budget } => {
            if reset_budget {
                reset_budget_in_state(&workspace)?;
            }
            super::sweep::run(workspace, default_sweep_args())
                .await
                .map(|_| ())
        }
        IterationOutcome::NewPlanLaunchPlay { plan: _ } => {
            // Wizard archived state.json and wrote a fresh plan.md already.
            // Note: state archive happens here on launch so resuming a fresh
            // plan starts with zero accumulated usage.
            archive_state(&workspace)?;
            super::play::run(workspace, true, false, false, false, false).await
        }
        IterationOutcome::NewPlanWait { plan } => {
            // Plan is on disk; user wants to start later. Print a tight
            // summary so they know exactly what to do next.
            archive_state(&workspace)?;
            print_wait_summary(&workspace, &plan, cfg);
            Ok(())
        }
        IterationOutcome::NewPlanTerminated => {
            // The wizard already archived plan.md and restored the seed.
            // Tell the user the workspace is back to a clean state and what
            // to do if they change their mind.
            println!();
            println!("  No plan instilled — workspace is back to the init seed.");
            println!("  Run `pitboss start` again when you're ready to try a new plan.");
            println!();
            Ok(())
        }
    }
}

/// Tight summary printed to plain stdout (after the alternate screen has
/// torn down) when the user picks "wait until later" on the post-plan
/// review screen. Tells them:
///   - that the plan is saved
///   - how many phases it contains (0/N completed)
///   - their current budget headroom
///   - how to launch it later
fn print_wait_summary(workspace: &Path, plan: &Plan, cfg: &Config) {
    let total = plan.phases.len();
    let usd_cap = cfg
        .budgets
        .max_total_usd
        .map(|c| format!("${:.2} cap", c))
        .unwrap_or_else(|| "no cap".to_string());
    let token_cap = cfg
        .budgets
        .max_total_tokens
        .map(|c| format!("{} token cap", c))
        .unwrap_or_else(|| "no token cap".to_string());

    println!();
    println!("  Plan saved to {}", paths::plan_path(workspace).display());
    println!(
        "  {} phase{} ready  •  0 / {} completed",
        total,
        if total == 1 { "" } else { "s" },
        total
    );
    println!("  Budget: {}  •  {}", usd_cap, token_cap);
    println!();
    println!("  When you're ready:");
    println!("    pitboss start            # opens the wizard again");
    println!("    pitboss play --tui       # launches the plan directly");
    println!();
}

/// Zero out `state.token_usage` in place. Used by the wizard's
/// "reset budget" toggle so a run that hit its cap can resume without
/// hand-editing state.json. No-op when no state file exists.
fn reset_budget_in_state(workspace: &Path) -> Result<()> {
    let mut state = match state::load(workspace)? {
        Some(s) => s,
        None => {
            debug!("reset_budget requested but state.json is missing — no-op");
            return Ok(());
        }
    };
    state.token_usage = TokenUsage::default();
    state::save(workspace, Some(&state))
        .with_context(|| format!("start: saving reset state in {:?}", workspace))?;
    println!("  Budget reset — token usage zeroed.");
    Ok(())
}

/// Move `state.json` to `state.<UTC-timestamp>.json.bak` and clear the live
/// file. Used by the "new plan" path so the next `play --tui` starts fresh
/// while the prior run is preserved for diagnostics.
fn archive_state(workspace: &Path) -> Result<()> {
    let live = paths::state_path(workspace);
    if !live.is_file() {
        // Nothing to archive; just ensure the file is absent.
        return state::save(workspace, None);
    }
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let backup = paths::play_dir(workspace).join(format!("state.{}.json.bak", stamp));
    fs::rename(&live, &backup)
        .with_context(|| format!("start: archiving state.json to {:?}", backup))?;
    println!("  Archived prior state to {}", backup.display());
    Ok(())
}

// ── Prerequisite checks ─────────────────────────────────────────────────────
//
// Run after the setup wizard writes files so the user immediately sees
// whether their workspace is actually ready for `pitboss play`. We only
// report — `pitboss play` enforces these at run time on its own.

/// Result of [`check_prereqs`].
pub(crate) struct Prereqs {
    /// `.git/` exists in the workspace. Pitboss play creates a per-run branch
    /// and refuses to operate without one (see README → Troubleshooting).
    pub git_repo: bool,
    /// CLI binary the configured backend wraps (e.g. "claude" for
    /// `claude_code`).
    pub agent_cli_name: &'static str,
    /// `agent_cli_name` was found on `$PATH`.
    pub agent_cli_found: bool,
}

/// Probe the workspace for the things `pitboss play` needs at run time:
/// a git repo and the agent CLI that matches `[agent] backend`. Both
/// failures are warnings, never errors — the user can `git init` or
/// install the CLI later and the workspace remains valid.
pub(crate) fn check_prereqs(workspace: &Path, cfg: &Config) -> Prereqs {
    let git_repo = workspace.join(".git").exists();

    let cli_name: &'static str = match cfg.agent.backend.as_deref() {
        Some("codex") => "codex",
        Some("aider") => "aider",
        Some("gemini") => "gemini",
        // Default and explicit "claude_code" → the Claude Code CLI is
        // installed as `claude`.
        _ => "claude",
    };
    let agent_cli_found = binary_on_path(cli_name);

    Prereqs {
        git_repo,
        agent_cli_name: cli_name,
        agent_cli_found,
    }
}

/// Return `true` if `name` resolves to a file in any directory on `$PATH`.
/// Cross-platform safe (also checks `.exe` on Windows-style PATH).
fn binary_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let direct = dir.join(name);
        if direct.is_file() {
            return true;
        }
        if cfg!(windows) {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return true;
            }
        }
        false
    })
}

/// Print the prereq panel after the wizard's "Workspace ready" summary so
/// users immediately know whether `pitboss play` will work.
pub(crate) fn print_prereqs(p: &Prereqs) {
    println!("  Prerequisites:");
    if p.git_repo {
        println!("    ✓  Git repository detected");
    } else {
        println!("    ⚠  Not a git repository");
        println!(
            "        Run `git init` before `pitboss play` — pitboss creates a per-run branch."
        );
    }
    if p.agent_cli_found {
        println!("    ✓  Agent CLI `{}` found on PATH", p.agent_cli_name);
    } else {
        println!("    ⚠  Agent CLI `{}` not found on PATH", p.agent_cli_name);
        println!(
            "        Install it before running pitboss play. See README → Runtime dependencies."
        );
    }
    println!();
}

/// Default `SweepArgs` matching `pitboss sweep` with no flags.
fn default_sweep_args() -> SweepArgs {
    SweepArgs {
        max_items: None,
        audit: false,
        no_audit: false,
        dry_run: false,
        after: None,
    }
}

// ── In-wizard agent dispatch helpers ─────────────────────────────────────────
//
// These mirror `interview::dispatch_questioner` and `plan::dispatch_planner`
// but collect the agent's stdout silently via the event channel so they don't
// write into the alternate-screen TUI. The wizard renders a "thinking" frame
// while these futures are in flight.

/// Dispatch the configured agent against the ranged questioner prompt and
/// return the parsed numbered list of design questions. `min`/`max` are
/// inclusive bounds passed verbatim into the prompt; the agent is instructed
/// to stay within them.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_questioner_silent(
    workspace: &Path,
    cfg: &Config,
    agent: &(dyn Agent + Send + Sync),
    goal: &str,
    repo_summary: &str,
    min: u32,
    max: u32,
    cancel: CancellationToken,
) -> Result<Vec<String>> {
    let logs_dir = paths::play_logs_dir(workspace);
    std::fs::create_dir_all(&logs_dir).context("start: creating logs dir")?;

    let user_prompt = prompts::questioner_ranged(goal, repo_summary, min, max);
    let request = AgentRequest {
        role: Role::Planner,
        model: cfg.models.planner.clone(),
        system_prompt: prompts::caveman::system_prompt(&cfg.caveman),
        user_prompt,
        workdir: workspace.to_path_buf(),
        log_path: logs_dir.join("wizard_questioner.log"),
        timeout: QUESTIONER_TIMEOUT,
        env: std::collections::HashMap::new(),
    };

    let body = run_agent_collect(agent, request, cancel).await?;
    let questions = parse_numbered_questions(&body);
    if questions.is_empty() {
        bail!("questioner produced no parseable questions");
    }
    Ok(questions)
}

/// Dispatch the planner agent with the (possibly Q&A-augmented) goal and
/// write the resulting `plan.md` atomically. Returns the parsed [`Plan`] so
/// the wizard can render the generated phase list on its review screen.
///
/// Mirrors `plan::run_with_agent` but without the eprintln/println status
/// chatter — safe to call from inside the wizard's alternate screen.
pub(crate) async fn dispatch_planner_silent(
    workspace: &Path,
    cfg: &Config,
    agent: &(dyn Agent + Send + Sync),
    goal: &str,
    repo_summary: &str,
    cancel: CancellationToken,
) -> Result<Plan> {
    let logs_dir = paths::play_logs_dir(workspace);
    std::fs::create_dir_all(&logs_dir).context("start: creating logs dir")?;

    let user_prompt = prompts::planner(goal, repo_summary);
    let request = AgentRequest {
        role: Role::Planner,
        model: cfg.models.planner.clone(),
        system_prompt: prompts::caveman::system_prompt(&cfg.caveman),
        user_prompt,
        workdir: workspace.to_path_buf(),
        log_path: logs_dir.join("wizard_planner.log"),
        timeout: PLANNER_TIMEOUT,
        env: std::collections::HashMap::new(),
    };

    let body = run_agent_collect(agent, request, cancel).await?;
    let parsed = plan::parse(&body).map_err(|e| anyhow!("planner output failed to parse: {e}"))?;

    let plan_path = paths::plan_path(workspace);
    write_atomic(&plan_path, body.as_bytes())
        .with_context(|| format!("start: writing {}", plan_path.display()))?;

    Ok(parsed)
}

/// Spawn `agent.run(request)`, accumulate every `Stdout` chunk into a
/// `String`, and return the concatenated body when the agent finishes.
/// Shared between [`dispatch_questioner_silent`] and
/// [`dispatch_planner_silent`].
async fn run_agent_collect(
    agent: &(dyn Agent + Send + Sync),
    request: AgentRequest,
    cancel: CancellationToken,
) -> Result<String> {
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    let collector = tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::Stdout(chunk) = ev {
                buf.push_str(&chunk);
            }
        }
        buf
    });

    let outcome = agent.run(request, tx, cancel).await?;
    let body = collector.await.unwrap_or_default();

    match outcome.stop_reason {
        StopReason::Completed if outcome.exit_code == 0 => Ok(body),
        StopReason::Completed => {
            bail!("agent exited with code {}", outcome.exit_code)
        }
        StopReason::Timeout => bail!("agent timed out"),
        StopReason::Cancelled => bail!("agent was cancelled"),
        StopReason::Error(msg) => bail!("agent failed: {msg}"),
    }
}

/// Extract questions from an agent's numbered-list response. Recognises
/// lines of the form `1. Question text` (leading whitespace allowed) and
/// preserves source order. Mirrors `interview::parse_questions`.
pub(crate) fn parse_numbered_questions(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some((prefix, rest)) = trimmed.split_once(". ") {
            if prefix.parse::<u32>().is_ok() {
                let q = rest.trim().to_string();
                if !q.is_empty() {
                    out.push(q);
                }
            }
        }
    }
    out
}

/// Format Q&A pairs as the spec snippet appended to the planner goal.
/// Mirrors `interview::format_spec`.
pub(crate) fn format_qa_spec(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (q, a)) in pairs.iter().enumerate() {
        out.push_str(&format!("Q{n}: {q}\nA{n}: {a}\n\n", n = i + 1));
    }
    out.trim_end().to_string()
}

/// Combine a base goal with an optional Q&A spec the way `pitboss plan
/// --interview` does, so the planner gets the design context.
pub(crate) fn goal_with_spec(goal: &str, spec: &str) -> String {
    if spec.is_empty() {
        goal.to_string()
    } else {
        format!("{goal}\n\n## Design Specification\n\n{spec}")
    }
}

/// Archive the live `plan.md` to `plan.<UTC-timestamp>.md.bak` next to it
/// and rewrite `plan.md` with the canonical [`PLAN_TEMPLATE`] seed.
///
/// Used by the wizard's "terminate plan" path so a user who rejects a
/// generated plan leaves the workspace in the same state `pitboss init`
/// would have produced — recoverable from the backup but cleanly
/// re-pickable by `pitboss start`.
pub(crate) fn archive_plan_and_restore_seed(workspace: &Path) -> Result<()> {
    let live = paths::plan_path(workspace);
    if live.is_file() {
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
        let backup = paths::play_dir(workspace).join(format!("plan.{}.md.bak", stamp));
        fs::rename(&live, &backup)
            .with_context(|| format!("start: archiving plan.md to {:?}", backup))?;
        println!("  Archived generated plan to {}", backup.display());
    }
    write_atomic(&live, PLAN_TEMPLATE.as_bytes())
        .with_context(|| format!("start: writing seed plan to {}", live.display()))?;
    Ok(())
}
