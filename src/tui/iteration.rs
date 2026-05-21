//! Interactive TUI iteration wizard for `pitboss start` when `.pitboss/`
//! already exists. Shows current run state (budget, phase progress, deferred
//! item count, configured models) and lets the user pick the next action:
//! continue the run, run a one-shot sweep, or start a new plan.
//!
//! The "new plan" path optionally runs an in-wizard design interview against
//! the planner agent (toggle + question-range picker on the goal-input
//! screen), then dispatches the planner and shows the generated phase list so
//! the user can launch `play --tui`, save the plan for later, or terminate
//! it. The agent dispatches are performed by helpers in
//! [`crate::cli::start`] — see `dispatch_questioner_silent` and
//! `dispatch_planner_silent` — so no agent output leaks into the TUI.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::Agent;
use crate::cli::start::{
    dispatch_planner_silent, dispatch_questioner_silent, format_qa_spec, goal_with_spec,
};
use crate::config::Config;
use crate::plan::{Phase, PhaseId, Plan};
use crate::tui::{TerminalGuard, TICK_INTERVAL};

/// Shorthand for the trait object the wizard hands to spawned tasks.
type SharedAgent = Arc<dyn Agent + Send + Sync>;

/// Read-only summary of the workspace shown on the iteration wizard's first
/// screen. Built by `cli::start::load_snapshot`.
pub struct WorkspaceSnapshot {
    pub branch: Option<String>,
    pub current_phase: Option<PhaseId>,
    pub completed_count: usize,
    pub total_phases: usize,
    pub tokens_used: u64,
    pub tokens_cap: Option<u64>,
    pub usd_used: f64,
    pub usd_cap: Option<f64>,
    pub deferred_count: usize,
    pub planner_model: String,
    pub implementer_model: String,
    /// True when `state.json` is missing or `null` — no run has started yet.
    /// Affects whether Continue dispatches to `play` (fresh) or `rebuy`
    /// (resume) downstream.
    pub no_state: bool,
    /// True when `plan.md` is missing or unparseable. The wizard restricts
    /// the action list to "New plan" only in this case.
    pub no_plan: bool,
}

/// Question-count bounds passed to the planner's questioner prompt. Maps to
/// the (`min`, `max`) tuple consumed by `prompts::questioner_ranged`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum QuestionRange {
    OneToFive,
    FiveToTen,
    TenToTwenty,
    /// Hard bounds are `(10, 50)` despite the label. An uncapped range lets
    /// some models emit 100+ questions; the 50-question ceiling keeps the
    /// interview session tractable while still giving the agent wide latitude.
    AsManyAsNeeded,
}

impl QuestionRange {
    pub fn bounds(self) -> (u32, u32) {
        match self {
            Self::OneToFive => (1, 5),
            Self::FiveToTen => (5, 10),
            Self::TenToTwenty => (10, 20),
            Self::AsManyAsNeeded => (10, 50),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::OneToFive => "1–5 questions",
            Self::FiveToTen => "5–10 questions",
            Self::TenToTwenty => "10–20 questions",
            Self::AsManyAsNeeded => "as many as needed",
        }
    }
    fn all() -> [Self; 4] {
        [
            Self::OneToFive,
            Self::FiveToTen,
            Self::TenToTwenty,
            Self::AsManyAsNeeded,
        ]
    }
}

/// What the wizard ultimately decided. `Continue`/`Sweep` are dispatched by
/// `cli::start::dispatch_action`; the `NewPlan*` variants signal that the
/// wizard has already generated (and possibly archived) `plan.md` and that
/// the caller's job is only to launch play or print a final hint.
pub enum IterationOutcome {
    Continue {
        reset_budget: bool,
    },
    Sweep {
        reset_budget: bool,
    },
    /// Plan generated and accepted; launch `play --tui` against the new plan.
    NewPlanLaunchPlay {
        plan: Plan,
    },
    /// Plan generated and accepted; the user wants to start it later. The
    /// caller prints a summary to stdout and exits.
    NewPlanWait {
        plan: Plan,
    },
    /// User rejected the generated plan. By the time this is returned,
    /// `plan.md` has been archived and reset to the seed template so the
    /// workspace is back to its pre-generation state.
    NewPlanTerminated,
}

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Continue,
    Sweep,
    NewPlan,
}

#[derive(Clone, Copy, PartialEq)]
enum PostPlanChoice {
    LaunchPlay,
    WaitForLater,
    Terminate,
}

impl PostPlanChoice {
    fn all() -> [Self; 3] {
        [Self::LaunchPlay, Self::WaitForLater, Self::Terminate]
    }
    fn label(self) -> &'static str {
        match self {
            Self::LaunchPlay => "Run it now  (launches play --tui)",
            Self::WaitForLater => "Wait until later  (plan saved, exit to terminal)",
            Self::Terminate => "Terminate plan  (not recommended — archives plan.md)",
        }
    }
}

#[derive(Clone, PartialEq)]
enum Step {
    /// Summary + action picker + reset-budget toggle.
    Summary,
    /// Goal text + interview toggle + range picker.
    NewPlanInput,
    /// Questioner agent dispatching; spinner.
    InterviewThinking,
    /// One question shown at a time; user types an answer.
    InterviewQA,
    /// Planner agent dispatching; spinner.
    PlanThinking,
    /// Generated phase list + Run/Wait/Terminate picker.
    PlanReview,
}

/// Which focusable field on the NewPlanInput screen has keyboard focus.
#[derive(Clone, Copy, PartialEq)]
enum NewPlanField {
    Goal,
    InterviewToggle,
    RangePicker,
}

/// Async task currently in flight. The render loop waits on the join handle
/// in `tokio::select!` and transitions to the next step on completion.
enum PendingTask {
    Questioner {
        cancel: CancellationToken,
        handle: JoinHandle<Result<Vec<String>>>,
    },
    Planner {
        cancel: CancellationToken,
        handle: JoinHandle<Result<Plan>>,
    },
}

impl PendingTask {
    /// Signal the `CancellationToken`. Returns immediately — the spawned task
    /// checks the token on its next `tokio::select!` iteration and exits
    /// cleanly rather than being abruptly aborted.
    fn cancel(&self) {
        match self {
            Self::Questioner { cancel, .. } => cancel.cancel(),
            Self::Planner { cancel, .. } => cancel.cancel(),
        }
    }
}

struct IterState {
    step: Step,
    cursor: usize,
    reset_budget: bool,
    actions: Vec<Action>,

    // NewPlanInput state
    goal_input: String,
    /// Cursor position in `goal_input`, measured in Unicode scalar values.
    goal_cursor: usize,
    /// When `Some`, the user has manually scrolled the goal input via
    /// PageUp/PageDown. Render uses this offset instead of auto-following
    /// the cursor. Reset to `None` on any keystroke that mutates the text
    /// so typing snaps back to following the cursor.
    goal_scroll: Option<u16>,
    /// Actual text for each `[Pasted text #N]` placeholder in `goal_input`.
    /// Expanded back to full text before the goal is dispatched to the planner.
    pastes: Vec<String>,
    new_plan_field: NewPlanField,
    interview_enabled: bool,
    range_cursor: usize,

    // Interview state
    questions: Vec<String>,
    answers: Vec<(String, String)>,
    current_q: usize,
    current_answer: String,
    /// Cursor position in `current_answer`, measured in Unicode scalar values.
    answer_cursor: usize,
    /// Same idea as `goal_scroll` but for the per-question answer field.
    answer_scroll: Option<u16>,

    // Plan review state
    generated_plan: Option<Plan>,
    review_cursor: usize,

    // Error captured from a failed task — shown in a small banner over the
    // current screen.
    error: Option<String>,

    // Spinner animation tick.
    tick: u64,
}

impl IterState {
    fn new(snapshot: &WorkspaceSnapshot) -> Self {
        // Restrict to NewPlan when plan.md is absent: Continue needs a
        // parseable plan to know which phase to resume from, and Sweep needs
        // it to anchor deferred items to a phase context.
        let actions = if snapshot.no_plan {
            vec![Action::NewPlan]
        } else {
            vec![Action::Continue, Action::Sweep, Action::NewPlan]
        };
        Self {
            step: Step::Summary,
            cursor: 0,
            reset_budget: false,
            actions,
            goal_input: String::new(),
            goal_cursor: 0,
            goal_scroll: None,
            pastes: Vec::new(),
            new_plan_field: NewPlanField::Goal,
            interview_enabled: false,
            range_cursor: 0,
            questions: Vec::new(),
            answers: Vec::new(),
            current_q: 0,
            current_answer: String::new(),
            answer_cursor: 0,
            answer_scroll: None,
            generated_plan: None,
            review_cursor: 0,
            error: None,
            tick: 0,
        }
    }

    fn selected_action(&self) -> Action {
        self.actions[self.cursor]
    }

    fn selected_range(&self) -> QuestionRange {
        QuestionRange::all()[self.range_cursor]
    }

    fn focus_advance(&mut self) {
        // Tab order: Goal → InterviewToggle (always, so the user can turn the
        // interview on even when it's currently off) → RangePicker (only when
        // interview is enabled) → Goal. The `if interview_enabled` guard on the
        // Goal arm is redundant — both branches go to InterviewToggle — but kept
        // for symmetry with `focus_back`.
        self.new_plan_field = match self.new_plan_field {
            NewPlanField::Goal if self.interview_enabled => NewPlanField::InterviewToggle,
            NewPlanField::Goal => NewPlanField::InterviewToggle,
            NewPlanField::InterviewToggle if self.interview_enabled => NewPlanField::RangePicker,
            NewPlanField::InterviewToggle => NewPlanField::Goal,
            NewPlanField::RangePicker => NewPlanField::Goal,
        };
    }

    fn focus_back(&mut self) {
        self.new_plan_field = match self.new_plan_field {
            NewPlanField::Goal if self.interview_enabled => NewPlanField::RangePicker,
            NewPlanField::Goal => NewPlanField::InterviewToggle,
            NewPlanField::InterviewToggle => NewPlanField::Goal,
            NewPlanField::RangePicker => NewPlanField::InterviewToggle,
        };
    }
}

enum LoopEv {
    Continue,
    Quit,
    Done(IterationOutcome),
    /// Caller should kick off the questioner agent for the current goal.
    StartQuestioner,
    /// Caller should kick off the planner agent (after collecting Q&A).
    StartPlanner,
    /// Caller should archive plan.md + restore seed, then return
    /// `NewPlanTerminated`.
    StartTerminate,
}

pub async fn run_iteration_wizard(
    snapshot: &WorkspaceSnapshot,
    workspace: &Path,
    cfg: &Config,
    agent: SharedAgent,
) -> Result<Option<IterationOutcome>> {
    let mut guard = TerminalGuard::setup()?;
    // Force a real clear-screen escape. The `Clear` widget alone diffs
    // against an empty initial buffer and sends nothing on first draw, so
    // prior terminal contents leak around the centered dialog. Calling
    // `terminal.clear()` emits the actual erase-display sequence.
    guard.terminal().clear()?;
    let mut state = IterState::new(snapshot);
    let mut input = EventStream::new();
    let mut pending: Option<PendingTask> = None;
    let workspace = workspace.to_path_buf();

    let result = loop {
        guard.terminal().draw(|f| render(f, snapshot, &state))?;

        if let Some(task) = pending.as_mut() {
            // A background dispatch is running. Wait for input (to cancel),
            // a tick (to animate the spinner), or task completion.
            tokio::select! {
                ev = input.next() => {
                    match ev {
                        Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                            if is_cancel(key.code, key.modifiers) {
                                task.cancel();
                            }
                        }
                        Some(Ok(CtEvent::Paste(_))) => {}  // ignore paste during dispatch
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(e.into()),
                        None => break None,
                    }
                }
                done = poll_task(task) => {
                    match done {
                        TaskFinished::Questioner(res) => {
                            pending = None;
                            match res {
                                Ok(qs) if qs.is_empty() => {
                                    state.error = Some(
                                        "Questioner returned no parseable questions — skipping interview."
                                            .to_string(),
                                    );
                                    // Fall through to planner directly with the bare goal.
                                    state.step = Step::PlanThinking;
                                    let cancel = CancellationToken::new();
                                    let handle = spawn_planner(
                                        agent.clone(), cfg.clone(), workspace.clone(),
                                        state.goal_input.clone(), cancel.clone(),
                                    );
                                    pending = Some(PendingTask::Planner { cancel, handle });
                                }
                                Ok(qs) => {
                                    state.questions = qs;
                                    state.current_q = 0;
                                    state.current_answer.clear();
                                    state.answer_cursor = 0;
                                    state.step = Step::InterviewQA;
                                }
                                Err(e) => {
                                    state.error = Some(format!("Questioner failed: {e:#}"));
                                    state.step = Step::NewPlanInput;
                                }
                            }
                        }
                        TaskFinished::Planner(res) => {
                            pending = None;
                            match res {
                                Ok(plan) => {
                                    state.generated_plan = Some(plan);
                                    state.review_cursor = 0;
                                    state.step = Step::PlanReview;
                                }
                                Err(e) => {
                                    state.error = Some(format!("Planner failed: {e:#}"));
                                    state.step = Step::NewPlanInput;
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep(TICK_INTERVAL) => {
                    state.tick = state.tick.wrapping_add(1);
                }
            }
        } else {
            tokio::select! {
                ev = input.next() => {
                    match ev {
                        Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                            match on_key(&mut state, key.code, key.modifiers) {
                                LoopEv::Continue => {}
                                LoopEv::Quit => break None,
                                LoopEv::Done(o) => break Some(o),
                                LoopEv::StartQuestioner => {
                                    // Expand paste placeholders before dispatching so the
                                    // planner receives the actual text, not `[Pasted text #N]`.
                                    if !state.pastes.is_empty() {
                                        state.goal_input = expand_pastes(&state.goal_input, &state.pastes);
                                        state.pastes.clear();
                                        state.goal_cursor = state.goal_input.chars().count();
                                    }
                                    let cancel = CancellationToken::new();
                                    let (min, max) = state.selected_range().bounds();
                                    let goal = state.goal_input.trim().to_string();
                                    let handle = spawn_questioner(
                                        agent.clone(), cfg.clone(), workspace.clone(),
                                        goal, min, max, cancel.clone(),
                                    );
                                    state.step = Step::InterviewThinking;
                                    pending = Some(PendingTask::Questioner { cancel, handle });
                                }
                                LoopEv::StartPlanner => {
                                    if !state.pastes.is_empty() {
                                        state.goal_input = expand_pastes(&state.goal_input, &state.pastes);
                                        state.pastes.clear();
                                        state.goal_cursor = state.goal_input.chars().count();
                                    }
                                    let cancel = CancellationToken::new();
                                    let spec = format_qa_spec(&state.answers);
                                    let goal = goal_with_spec(state.goal_input.trim(), &spec);
                                    let handle = spawn_planner(
                                        agent.clone(), cfg.clone(), workspace.clone(),
                                        goal, cancel.clone(),
                                    );
                                    state.step = Step::PlanThinking;
                                    pending = Some(PendingTask::Planner { cancel, handle });
                                }
                                LoopEv::StartTerminate => {
                                    if let Err(e) =
                                        crate::cli::start::archive_plan_and_restore_seed(&workspace)
                                    {
                                        state.error =
                                            Some(format!("Termination failed: {e:#}"));
                                    } else {
                                        break Some(IterationOutcome::NewPlanTerminated);
                                    }
                                }
                            }
                        }
                        Some(Ok(CtEvent::Paste(ref text))) => {
                            handle_paste(&mut state, text);
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(e.into()),
                        None => break None,
                    }
                }
                _ = tokio::time::sleep(TICK_INTERVAL) => {
                    state.tick = state.tick.wrapping_add(1);
                }
            }
        }
    };

    // Cancel any in-flight task before tearing down the terminal so we don't
    // leak a spawned subprocess waiting on its output channel.
    if let Some(task) = pending.take() {
        task.cancel();
    }

    guard.restore()?;
    Ok(result)
}

enum TaskFinished {
    Questioner(Result<Vec<String>>),
    Planner(Result<Plan>),
}

async fn poll_task(task: &mut PendingTask) -> TaskFinished {
    match task {
        PendingTask::Questioner { handle, .. } => {
            let res = handle
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("questioner task panicked: {e}")));
            TaskFinished::Questioner(res)
        }
        PendingTask::Planner { handle, .. } => {
            let res = handle
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("planner task panicked: {e}")));
            TaskFinished::Planner(res)
        }
    }
}

fn spawn_questioner(
    agent: SharedAgent,
    cfg: Config,
    workspace: std::path::PathBuf,
    goal: String,
    min: u32,
    max: u32,
    cancel: CancellationToken,
) -> JoinHandle<Result<Vec<String>>> {
    tokio::spawn(async move {
        let repo_summary = collect_summary_safe(&workspace);
        dispatch_questioner_silent(
            &workspace,
            &cfg,
            agent.as_ref(),
            &goal,
            &repo_summary,
            min,
            max,
            cancel,
        )
        .await
    })
}

fn spawn_planner(
    agent: SharedAgent,
    cfg: Config,
    workspace: std::path::PathBuf,
    goal: String,
    cancel: CancellationToken,
) -> JoinHandle<Result<Plan>> {
    tokio::spawn(async move {
        let repo_summary = collect_summary_safe(&workspace);
        dispatch_planner_silent(
            &workspace,
            &cfg,
            agent.as_ref(),
            &goal,
            &repo_summary,
            cancel,
        )
        .await
    })
}

/// Collect the repo summary, silently degrading to a placeholder on error.
/// Failures here (permission denied, empty workspace) are non-fatal — the
/// agent dispatch continues with a reduced context rather than aborting the wizard.
fn collect_summary_safe(workspace: &Path) -> String {
    crate::cli::plan::collect_repo_summary(workspace)
        .unwrap_or_else(|_| "(repo summary unavailable)".to_string())
}

fn is_cancel(code: KeyCode, mods: KeyModifiers) -> bool {
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        return true;
    }
    matches!(code, KeyCode::Esc)
}

fn on_key(s: &mut IterState, code: KeyCode, mods: KeyModifiers) -> LoopEv {
    // Clear any error banner on the next keypress.
    s.error = None;

    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        return LoopEv::Quit;
    }
    match s.step.clone() {
        Step::Summary => summary_key(s, code),
        Step::NewPlanInput => new_plan_key(s, code, mods),
        Step::InterviewQA => interview_qa_key(s, code, mods),
        Step::PlanReview => plan_review_key(s, code),
        // Thinking screens accept only cancel; cancel is handled in the
        // pending-task branch of the main loop, so we get nothing here.
        Step::InterviewThinking | Step::PlanThinking => LoopEv::Continue,
    }
}

fn summary_key(s: &mut IterState, code: KeyCode) -> LoopEv {
    match code {
        KeyCode::Up => {
            s.cursor = s.cursor.saturating_sub(1);
            LoopEv::Continue
        }
        KeyCode::Down => {
            if s.cursor + 1 < s.actions.len() {
                s.cursor += 1;
            }
            LoopEv::Continue
        }
        KeyCode::Char(' ') => {
            if !matches!(s.selected_action(), Action::NewPlan) {
                s.reset_budget = !s.reset_budget;
            }
            LoopEv::Continue
        }
        KeyCode::Enter => match s.selected_action() {
            Action::Continue => LoopEv::Done(IterationOutcome::Continue {
                reset_budget: s.reset_budget,
            }),
            Action::Sweep => LoopEv::Done(IterationOutcome::Sweep {
                reset_budget: s.reset_budget,
            }),
            Action::NewPlan => {
                s.step = Step::NewPlanInput;
                s.new_plan_field = NewPlanField::Goal;
                LoopEv::Continue
            }
        },
        KeyCode::Char('q') => LoopEv::Quit,
        _ => LoopEv::Continue,
    }
}

fn new_plan_key(s: &mut IterState, code: KeyCode, mods: KeyModifiers) -> LoopEv {
    // Ctrl shortcuts for the goal field — handled before the main match so
    // they don't fall through to the generic Char insertion arm.
    if s.new_plan_field == NewPlanField::Goal && mods.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('a') => {
                s.goal_cursor = 0;
                s.goal_scroll = None;
                return LoopEv::Continue;
            }
            KeyCode::Char('e') => {
                s.goal_cursor = s.goal_input.chars().count();
                s.goal_scroll = None;
                return LoopEv::Continue;
            }
            _ => return LoopEv::Continue,
        }
    }

    match (s.new_plan_field, code) {
        // ── goal cursor movement ──────────────────────────────────────────
        (NewPlanField::Goal, KeyCode::Left) => {
            if s.goal_cursor > 0 {
                s.goal_cursor -= 1;
            }
            LoopEv::Continue
        }
        (NewPlanField::Goal, KeyCode::Right) => {
            let len = s.goal_input.chars().count();
            if s.goal_cursor < len {
                s.goal_cursor += 1;
            }
            LoopEv::Continue
        }
        (NewPlanField::Goal, KeyCode::Home) => {
            s.goal_cursor = 0;
            s.goal_scroll = None;
            LoopEv::Continue
        }
        (NewPlanField::Goal, KeyCode::End) => {
            s.goal_cursor = s.goal_input.chars().count();
            s.goal_scroll = None;
            LoopEv::Continue
        }
        // ── goal viewport scroll (manual override) ────────────────────────
        (NewPlanField::Goal, KeyCode::PageUp) => {
            let baseline = s.goal_scroll.unwrap_or_else(|| {
                cursor_auto_scroll(&s.goal_input, s.goal_cursor, GOAL_INNER_W, GOAL_INNER_H)
            });
            s.goal_scroll = Some(baseline.saturating_sub(SCROLL_PAGE));
            LoopEv::Continue
        }
        (NewPlanField::Goal, KeyCode::PageDown) => {
            let baseline = s.goal_scroll.unwrap_or_else(|| {
                cursor_auto_scroll(&s.goal_input, s.goal_cursor, GOAL_INNER_W, GOAL_INNER_H)
            });
            let max = cursor_auto_scroll(
                &s.goal_input,
                s.goal_input.chars().count(),
                GOAL_INNER_W,
                GOAL_INNER_H,
            );
            s.goal_scroll = Some(baseline.saturating_add(SCROLL_PAGE).min(max));
            LoopEv::Continue
        }
        // ── global navigation ─────────────────────────────────────────────
        (_, KeyCode::Enter) => {
            if s.goal_input.trim().is_empty() {
                return LoopEv::Continue;
            }
            if s.interview_enabled {
                LoopEv::StartQuestioner
            } else {
                s.answers.clear();
                LoopEv::StartPlanner
            }
        }
        (_, KeyCode::Esc) => {
            s.step = Step::Summary;
            LoopEv::Continue
        }
        // Tab / BackTab always cycle focus regardless of which field is active.
        (_, KeyCode::Tab) => {
            s.focus_advance();
            LoopEv::Continue
        }
        (_, KeyCode::BackTab) => {
            s.focus_back();
            LoopEv::Continue
        }
        // Up/Down cycle focus only when the goal text box is NOT active,
        // so they don't steal the cursor from the goal field.
        (NewPlanField::InterviewToggle, KeyCode::Down)
        | (NewPlanField::RangePicker, KeyCode::Down) => {
            s.focus_advance();
            LoopEv::Continue
        }
        (NewPlanField::InterviewToggle, KeyCode::Up) | (NewPlanField::RangePicker, KeyCode::Up) => {
            s.focus_back();
            LoopEv::Continue
        }
        // ── goal text editing ─────────────────────────────────────────────
        (NewPlanField::Goal, KeyCode::Char(c)) => {
            text_insert_char(&mut s.goal_input, &mut s.goal_cursor, c);
            s.goal_scroll = None;
            LoopEv::Continue
        }
        (NewPlanField::Goal, KeyCode::Backspace) => {
            text_backspace(&mut s.goal_input, &mut s.goal_cursor);
            s.goal_scroll = None;
            LoopEv::Continue
        }
        (NewPlanField::Goal, KeyCode::Delete) => {
            text_delete_forward(&mut s.goal_input, s.goal_cursor);
            s.goal_scroll = None;
            LoopEv::Continue
        }
        // ── interview toggle & range picker ───────────────────────────────
        (NewPlanField::InterviewToggle, KeyCode::Char(' ')) => {
            s.interview_enabled = !s.interview_enabled;
            if s.interview_enabled {
                s.new_plan_field = NewPlanField::RangePicker;
            }
            LoopEv::Continue
        }
        (NewPlanField::RangePicker, KeyCode::Left) => {
            s.range_cursor = s.range_cursor.saturating_sub(1);
            LoopEv::Continue
        }
        (NewPlanField::RangePicker, KeyCode::Right) => {
            if s.range_cursor + 1 < QuestionRange::all().len() {
                s.range_cursor += 1;
            }
            LoopEv::Continue
        }
        _ => LoopEv::Continue,
    }
}

fn interview_qa_key(s: &mut IterState, code: KeyCode, mods: KeyModifiers) -> LoopEv {
    // Ctrl shortcuts for cursor movement in the answer field.
    if mods.contains(KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('a') => {
                s.answer_cursor = 0;
                s.answer_scroll = None;
                return LoopEv::Continue;
            }
            KeyCode::Char('e') => {
                s.answer_cursor = s.current_answer.chars().count();
                s.answer_scroll = None;
                return LoopEv::Continue;
            }
            _ => return LoopEv::Continue,
        }
    }

    match code {
        // ── cursor movement ───────────────────────────────────────────────
        KeyCode::Left => {
            if s.answer_cursor > 0 {
                s.answer_cursor -= 1;
            }
            LoopEv::Continue
        }
        KeyCode::Right => {
            let len = s.current_answer.chars().count();
            if s.answer_cursor < len {
                s.answer_cursor += 1;
            }
            LoopEv::Continue
        }
        KeyCode::Home => {
            s.answer_cursor = 0;
            s.answer_scroll = None;
            LoopEv::Continue
        }
        KeyCode::End => {
            s.answer_cursor = s.current_answer.chars().count();
            s.answer_scroll = None;
            LoopEv::Continue
        }
        // ── viewport scroll ───────────────────────────────────────────────
        KeyCode::PageUp => {
            let baseline = s.answer_scroll.unwrap_or_else(|| {
                cursor_auto_scroll(
                    &s.current_answer,
                    s.answer_cursor,
                    GOAL_INNER_W,
                    GOAL_INNER_H,
                )
            });
            s.answer_scroll = Some(baseline.saturating_sub(SCROLL_PAGE));
            LoopEv::Continue
        }
        KeyCode::PageDown => {
            let baseline = s.answer_scroll.unwrap_or_else(|| {
                cursor_auto_scroll(
                    &s.current_answer,
                    s.answer_cursor,
                    GOAL_INNER_W,
                    GOAL_INNER_H,
                )
            });
            let max = cursor_auto_scroll(
                &s.current_answer,
                s.current_answer.chars().count(),
                GOAL_INNER_W,
                GOAL_INNER_H,
            );
            s.answer_scroll = Some(baseline.saturating_add(SCROLL_PAGE).min(max));
            LoopEv::Continue
        }
        // ── answer submission & navigation ────────────────────────────────
        KeyCode::Enter => {
            let q = s.questions[s.current_q].clone();
            let a = s.current_answer.trim().to_string();
            if !a.is_empty() {
                s.answers.push((q, a));
            }
            s.current_answer.clear();
            s.answer_cursor = 0;
            s.answer_scroll = None;
            s.current_q += 1;
            if s.current_q >= s.questions.len() {
                LoopEv::StartPlanner
            } else {
                LoopEv::Continue
            }
        }
        KeyCode::Tab => {
            s.current_answer.clear();
            s.answer_cursor = 0;
            s.answer_scroll = None;
            s.current_q += 1;
            if s.current_q >= s.questions.len() {
                LoopEv::StartPlanner
            } else {
                LoopEv::Continue
            }
        }
        KeyCode::Esc => {
            s.step = Step::NewPlanInput;
            s.questions.clear();
            s.answers.clear();
            s.current_q = 0;
            s.current_answer.clear();
            s.answer_cursor = 0;
            s.answer_scroll = None;
            LoopEv::Continue
        }
        // ── text editing ──────────────────────────────────────────────────
        KeyCode::Char(c) => {
            text_insert_char(&mut s.current_answer, &mut s.answer_cursor, c);
            s.answer_scroll = None;
            LoopEv::Continue
        }
        KeyCode::Backspace => {
            text_backspace(&mut s.current_answer, &mut s.answer_cursor);
            s.answer_scroll = None;
            LoopEv::Continue
        }
        KeyCode::Delete => {
            text_delete_forward(&mut s.current_answer, s.answer_cursor);
            s.answer_scroll = None;
            LoopEv::Continue
        }
        _ => LoopEv::Continue,
    }
}

fn plan_review_key(s: &mut IterState, code: KeyCode) -> LoopEv {
    match code {
        KeyCode::Up => {
            s.review_cursor = s.review_cursor.saturating_sub(1);
            LoopEv::Continue
        }
        KeyCode::Down => {
            if s.review_cursor + 1 < PostPlanChoice::all().len() {
                s.review_cursor += 1;
            }
            LoopEv::Continue
        }
        KeyCode::Enter => {
            let choice = PostPlanChoice::all()[s.review_cursor];
            let plan = s.generated_plan.clone();
            match (choice, plan) {
                (PostPlanChoice::LaunchPlay, Some(p)) => {
                    LoopEv::Done(IterationOutcome::NewPlanLaunchPlay { plan: p })
                }
                (PostPlanChoice::WaitForLater, Some(p)) => {
                    LoopEv::Done(IterationOutcome::NewPlanWait { plan: p })
                }
                (PostPlanChoice::Terminate, _) => LoopEv::StartTerminate,
                _ => LoopEv::Continue,
            }
        }
        KeyCode::Esc => {
            // Esc on the review screen does nothing — the user must pick
            // one of the three options. Pressing q quits without doing
            // anything (the plan is already written).
            LoopEv::Continue
        }
        KeyCode::Char('q') => LoopEv::Quit,
        _ => LoopEv::Continue,
    }
}

// ── text-input helpers ────────────────────────────────────────────────────────

/// Convert a char index into a byte offset for the given string.
fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// Insert `c` at the cursor position, then advance the cursor.
fn text_insert_char(text: &mut String, cursor: &mut usize, c: char) {
    let pos = char_to_byte(text, *cursor);
    text.insert(pos, c);
    *cursor += 1;
}

/// Insert the entire string `s` at the cursor position, then advance.
fn text_insert_str(text: &mut String, cursor: &mut usize, s: &str) {
    let pos = char_to_byte(text, *cursor);
    text.insert_str(pos, s);
    *cursor += s.chars().count();
}

/// Delete the character before the cursor (backspace semantics).
fn text_backspace(text: &mut String, cursor: &mut usize) {
    if *cursor > 0 {
        *cursor -= 1;
        let pos = char_to_byte(text, *cursor);
        text.remove(pos);
    }
}

/// Delete the character at the cursor (forward-delete / Delete key semantics).
fn text_delete_forward(text: &mut String, cursor: usize) {
    if cursor < text.chars().count() {
        let pos = char_to_byte(text, cursor);
        text.remove(pos);
    }
}

/// Build the display string for a focused text input, inserting the block
/// cursor `█` at `cursor` chars into `value`.
fn display_cursor(value: &str, cursor: usize) -> String {
    let byte_pos = char_to_byte(value, cursor);
    format!("{}█{}", &value[..byte_pos], &value[byte_pos..])
}

/// Compute the vertical scroll offset that keeps the cursor visible.
/// `inner_w`/`inner_h` are the usable dimensions inside the box borders.
fn cursor_auto_scroll(text: &str, cursor: usize, inner_w: u16, inner_h: u16) -> u16 {
    let iw = inner_w.max(1) as usize;
    let ih = inner_h.max(1) as usize;
    let cursor_line = cursor / iw;
    let total = (text.chars().count() + 1).div_ceil(iw).max(1);
    let max_scroll = total.saturating_sub(ih) as u16;
    (cursor_line.saturating_sub(ih.saturating_sub(1)) as u16).min(max_scroll)
}

/// Expand `[Pasted text #N]` placeholders back to the original pasted text
/// before submitting the goal to the planner.
fn expand_pastes(text: &str, pastes: &[String]) -> String {
    let mut result = text.to_string();
    for (i, actual) in pastes.iter().enumerate() {
        result = result.replace(&format!("[Pasted text #{}]", i + 1), actual);
    }
    result
}

/// Handle a bracketed-paste event. Large pastes (> 80 chars or multiline) are
/// compressed to a `[Pasted text #N]` placeholder; the actual text is stored
/// in `state.pastes` and expanded before dispatch. This mirrors the approach
/// Claude Code uses: the terminal wraps clipboard pastes in `ESC[?2004h` /
/// `ESC[200~…ESC[201~` sequences (bracketed-paste mode), the app receives a
/// single `Event::Paste(text)` rather than individual keystrokes, and can
/// decide whether to inline or compress based on size.
fn handle_paste(s: &mut IterState, text: &str) {
    const THRESHOLD: usize = 80;
    match (&s.step, s.new_plan_field) {
        (Step::NewPlanInput, NewPlanField::Goal) => {
            if text.len() > THRESHOLD || text.contains('\n') {
                let n = s.pastes.len() + 1;
                let placeholder = format!("[Pasted text #{}]", n);
                s.pastes.push(text.to_string());
                text_insert_str(&mut s.goal_input, &mut s.goal_cursor, &placeholder);
            } else {
                text_insert_str(&mut s.goal_input, &mut s.goal_cursor, text);
            }
            s.goal_scroll = None;
        }
        (Step::InterviewQA, _) => {
            text_insert_str(&mut s.current_answer, &mut s.answer_cursor, text);
            s.answer_scroll = None;
        }
        _ => {}
    }
}

// ── rendering ────────────────────────────────────────────────────────────────

fn render(f: &mut ratatui::Frame<'_>, snapshot: &WorkspaceSnapshot, state: &IterState) {
    let area = f.area();
    // Clear the entire screen first — fixes scrollback bleeding through the
    // gaps outside the dialog rect (visible when the wizard is launched from
    // a terminal that already has output on screen).
    f.render_widget(Clear, area);

    let dialog = centered_rect(80, 30, area);
    match state.step {
        Step::Summary => render_summary(f, dialog, snapshot, state),
        Step::NewPlanInput => render_new_plan(f, dialog, state),
        Step::InterviewThinking => render_thinking(
            f,
            dialog,
            "generating design questions",
            "Asking the planner agent for targeted questions about your goal.",
            state.tick,
        ),
        Step::InterviewQA => render_interview_qa(f, dialog, state),
        Step::PlanThinking => render_thinking(
            f,
            dialog,
            "building plan",
            "The planner is turning your goal (and answers) into a phased plan.md.",
            state.tick,
        ),
        Step::PlanReview => render_plan_review(f, dialog, state),
    }

    if let Some(err) = &state.error {
        // Small error banner pinned to the bottom of the dialog.
        let banner_area = Rect {
            x: dialog.x,
            y: dialog.y + dialog.height.saturating_sub(3),
            width: dialog.width,
            height: 3,
        };
        f.render_widget(Clear, banner_area);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "  ⚠ Something went wrong",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("  {}", err),
                    Style::default().fg(Color::Red),
                )),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Red)),
            ),
            banner_area,
        );
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn dialog_block(title: &str) -> Block<'_> {
    Block::default()
        .title(format!(" {} ", title))
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
}

fn hint(text: &'static str) -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )]))
}

/// Approximate inner width / height of a scrollable input box at the
/// wizard's 80-col dialog size. Used by the key handlers to compute
/// auto-scroll baseline values without needing access to the render area.
/// (Margins: 80 dialog − 2 block borders − 2 layout margin − 2 input
/// borders = 74 inner cols. The box height is 10 → 8 inner rows.)
const GOAL_INNER_W: u16 = 74;
const GOAL_INNER_H: u16 = 8;
/// One PageUp/PageDown press scrolls this many wrapped lines.
const SCROLL_PAGE: u16 = 4;

/// Render a text input box.
///
/// `cursor_pos` – `Some(char_index)` when the box is focused; the `█` cursor
/// is placed at that position and the viewport auto-scrolls to keep it
/// visible. `None` renders the box unfocused (gray border, no cursor).
/// `scroll_override` – a manually-set viewport offset (PageUp/PageDown);
/// takes precedence over the auto-scroll when `Some`.
fn text_input_widget(
    value: &str,
    cursor_pos: Option<usize>,
    area: Rect,
    scroll_override: Option<u16>,
) -> Paragraph<'_> {
    let (display, focused) = match cursor_pos {
        Some(pos) => (display_cursor(value, pos), true),
        None => (value.to_string(), false),
    };
    let border = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let inner_h = area.height.saturating_sub(2).max(1) as usize;
    let total_lines = display.chars().count().div_ceil(inner_w).max(1);
    let max_scroll = total_lines.saturating_sub(inner_h) as u16;
    let auto_scroll = match cursor_pos {
        Some(pos) => {
            let cursor_line = pos / inner_w;
            let min_scroll = cursor_line.saturating_sub(inner_h.saturating_sub(1)) as u16;
            min_scroll.min(max_scroll)
        }
        None => max_scroll,
    };
    let scroll = scroll_override
        .map(|s| s.min(max_scroll))
        .unwrap_or(auto_scroll);
    Paragraph::new(display)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

fn fmt_usd(amount: f64) -> String {
    format!("${:.2}", amount)
}

fn fmt_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

fn spinner_char(tick: u64) -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick as usize) % FRAMES.len()]
}

fn render_summary(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    snapshot: &WorkspaceSnapshot,
    state: &IterState,
) {
    let block = dialog_block("pitboss  start");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Length(6), // status block
            Constraint::Length(1), // spacer
            Constraint::Length(1), // "What next?" label
            Constraint::Min(3),    // action list
            Constraint::Length(1), // reset-budget toggle
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    let header = if let Some(branch) = &snapshot.branch {
        format!("Workspace ready  •  branch: {}", branch)
    } else if snapshot.no_state {
        "Workspace configured  •  no run started yet".to_string()
    } else {
        "Workspace ready".to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            header,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );

    let phase_line = if snapshot.no_plan {
        "Plan progress     plan.md missing or unparseable".to_string()
    } else {
        let current = snapshot
            .current_phase
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "—".to_string());
        format!(
            "Plan progress     {} / {} phases   (current: {})",
            snapshot.completed_count, snapshot.total_phases, current
        )
    };
    let budget_line = match snapshot.usd_cap {
        Some(cap) => format!(
            "Budget            {} used / {} cap",
            fmt_usd(snapshot.usd_used),
            fmt_usd(cap)
        ),
        None => format!(
            "Budget            {} used (no cap)",
            fmt_usd(snapshot.usd_used)
        ),
    };
    let tokens_line = match snapshot.tokens_cap {
        Some(cap) => format!(
            "Tokens            {} / {} tokens",
            fmt_tokens(snapshot.tokens_used),
            fmt_tokens(cap)
        ),
        None => format!(
            "Tokens            {} tokens (no cap)",
            fmt_tokens(snapshot.tokens_used)
        ),
    };
    let deferred_line = format!("Deferred items    {} pending", snapshot.deferred_count);
    let models_line = format!(
        "Models            planner: {}  ·  worker: {}",
        snapshot.planner_model, snapshot.implementer_model
    );

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(phase_line, Style::default().fg(Color::White))),
            Line::from(Span::styled(budget_line, Style::default().fg(Color::White))),
            Line::from(Span::styled(tokens_line, Style::default().fg(Color::Gray))),
            Line::from(Span::styled(
                deferred_line,
                Style::default().fg(Color::White),
            )),
            Line::from(Span::styled(models_line, Style::default().fg(Color::Gray))),
        ]),
        chunks[2],
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "What next?",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[4],
    );

    let items: Vec<ListItem> = state
        .actions
        .iter()
        .enumerate()
        .map(|(i, action)| {
            let label = match action {
                Action::Continue => {
                    if snapshot.no_state {
                        "Start the run  (no prior state)".to_string()
                    } else {
                        "Continue run".to_string()
                    }
                }
                Action::Sweep => {
                    format!("Run sweep  ({} deferred items)", snapshot.deferred_count)
                }
                Action::NewPlan => "New plan  (archives current state)".to_string(),
            };
            let prefix = if i == state.cursor { "►  " } else { "   " };
            let style = if i == state.cursor {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!("{}{}", prefix, label),
                style,
            )]))
        })
        .collect();
    f.render_widget(List::new(items), chunks[5]);

    let toggle_visible = !matches!(state.selected_action(), Action::NewPlan);
    let toggle_line = if toggle_visible {
        let mark = if state.reset_budget { "[x]" } else { "[ ]" };
        Line::from(vec![Span::styled(
            format!("{} Reset budget before launching", mark),
            if state.reset_budget {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Gray)
            },
        )])
    } else {
        Line::from(vec![Span::styled(
            "    (budget is reset automatically when starting a new plan)",
            Style::default().fg(Color::DarkGray),
        )])
    };
    f.render_widget(Paragraph::new(toggle_line), chunks[6]);

    f.render_widget(
        hint("[↑↓] select  [Space] toggle reset  [Enter] launch  [q] quit"),
        chunks[7],
    );
}

fn render_new_plan(f: &mut ratatui::Frame<'_>, area: Rect, state: &IterState) {
    let block = dialog_block("pitboss  start  ›  new plan");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Auto-grow: start at 1 content line (3 total with borders); grow as the
    // text wraps, capped at 8 content lines (10 total).
    let visual_lines = state
        .goal_input
        .chars()
        .count()
        .div_ceil(GOAL_INNER_W as usize)
        .max(1);
    let goal_box_h = (visual_lines.min(8) + 2) as u16;

    let interview_rows = if state.interview_enabled { 6 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1),              // label
            Constraint::Length(1),              // spacer
            Constraint::Length(2),              // archive notice
            Constraint::Length(1),              // spacer
            Constraint::Length(1),              // goal label
            Constraint::Length(goal_box_h),     // goal input (auto-grows, scrolls)
            Constraint::Length(1),              // spacer
            Constraint::Length(1),              // interview toggle
            Constraint::Length(interview_rows), // range picker (collapses when off)
            Constraint::Min(1),
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Describe what you want pitboss to build next.",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "Current state.json will be archived to",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "  .pitboss/play/state.<timestamp>.json.bak",
                Style::default().fg(Color::Gray),
            )),
        ]),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Goal",
            Style::default()
                .fg(field_color(state.new_plan_field, NewPlanField::Goal))
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[4],
    );
    let goal_focused = state.new_plan_field == NewPlanField::Goal;
    f.render_widget(
        text_input_widget(
            &state.goal_input,
            if goal_focused {
                Some(state.goal_cursor)
            } else {
                None
            },
            chunks[5],
            state.goal_scroll,
        ),
        chunks[5],
    );

    let toggle_mark = if state.interview_enabled {
        "[x]"
    } else {
        "[ ]"
    };
    let toggle_focused = state.new_plan_field == NewPlanField::InterviewToggle;
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("{} Run interview questions (Space to toggle)", toggle_mark),
            Style::default().fg(field_color(
                state.new_plan_field,
                NewPlanField::InterviewToggle,
            )),
        )]))
        .block(
            Block::default()
                .borders(Borders::NONE)
                .style(if toggle_focused {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                }),
        ),
        chunks[7],
    );

    if state.interview_enabled {
        let picker_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // label
                Constraint::Length(1), // spacer
                Constraint::Min(1),    // list
            ])
            .split(chunks[8]);

        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "How many questions?",
                Style::default()
                    .fg(field_color(state.new_plan_field, NewPlanField::RangePicker))
                    .add_modifier(Modifier::BOLD),
            )])),
            picker_chunks[0],
        );
        let items: Vec<ListItem> = QuestionRange::all()
            .iter()
            .enumerate()
            .map(|(i, range)| {
                let prefix = if i == state.range_cursor {
                    "►  "
                } else {
                    "   "
                };
                let focused = state.new_plan_field == NewPlanField::RangePicker;
                let style = if i == state.range_cursor && focused {
                    Style::default().fg(Color::Yellow)
                } else if i == state.range_cursor {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                ListItem::new(Line::from(vec![Span::styled(
                    format!("{}{}", prefix, range.label()),
                    style,
                )]))
            })
            .collect();
        f.render_widget(List::new(items), picker_chunks[2]);
    }

    f.render_widget(
        hint("[←→] cursor  [Tab] cycle  [PgUp/PgDn] scroll  [Enter] launch  [Esc] back"),
        chunks[10],
    );
}

fn field_color(focused: NewPlanField, want: NewPlanField) -> Color {
    if focused == want {
        Color::Yellow
    } else {
        Color::Gray
    }
}

fn render_thinking(f: &mut ratatui::Frame<'_>, area: Rect, title: &str, body: &str, tick: u64) {
    let title = format!("pitboss  start  ›  {}", title);
    let block = dialog_block(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(3), // spinner + message
            Constraint::Min(1),
            Constraint::Length(1), // footer hint
        ])
        .split(inner);

    let spinner = spinner_char(tick);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("  {}  {}", spinner, body),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  This can take 30s–3m depending on the model.",
                Style::default().fg(Color::DarkGray),
            )),
        ]),
        chunks[1],
    );
    f.render_widget(hint("[Esc] cancel  [Ctrl+C] quit"), chunks[3]);
}

fn render_interview_qa(f: &mut ratatui::Frame<'_>, area: Rect, state: &IterState) {
    let title = format!(
        "pitboss  start  ›  interview ({}/{})",
        state.current_q + 1,
        state.questions.len()
    );
    let block = dialog_block(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let q = state
        .questions
        .get(state.current_q)
        .cloned()
        .unwrap_or_default();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Length(4), // question text (up to ~3 wrapped lines)
            Constraint::Length(1), // spacer
            Constraint::Length(8), // answer input (6 visible lines + borders, scrolls)
            Constraint::Min(1),
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!(
                "Question {} of {}",
                state.current_q + 1,
                state.questions.len()
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(q, Style::default().fg(Color::White)))
            .wrap(Wrap { trim: false }),
        chunks[2],
    );
    f.render_widget(
        text_input_widget(
            &state.current_answer,
            Some(state.answer_cursor),
            chunks[4],
            state.answer_scroll,
        ),
        chunks[4],
    );
    f.render_widget(
        hint("[Enter] next  [Tab] skip  [PgUp/PgDn] scroll  [Esc] cancel"),
        chunks[6],
    );
}

fn render_plan_review(f: &mut ratatui::Frame<'_>, area: Rect, state: &IterState) {
    let block = dialog_block("pitboss  start  ›  plan ready");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let phases: Vec<&Phase> = state
        .generated_plan
        .as_ref()
        .map(|p| p.phases.iter().collect())
        .unwrap_or_default();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // spacer
            Constraint::Min(3),    // phase list
            Constraint::Length(1), // spacer
            Constraint::Length(1), // "What next?" label
            Constraint::Length(5), // choices
            Constraint::Length(1), // footer
        ])
        .split(inner);

    let header = format!(
        "Pitboss built a {}-phase plan from your interview.",
        phases.len()
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            header,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );

    let phase_items: Vec<ListItem> = phases
        .iter()
        .map(|p| {
            ListItem::new(Line::from(vec![Span::styled(
                format!("  Phase {}: {}", p.id, p.title),
                Style::default().fg(Color::White),
            )]))
        })
        .collect();
    f.render_widget(List::new(phase_items), chunks[2]);

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "What next?",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[4],
    );

    let choice_items: Vec<ListItem> = PostPlanChoice::all()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let prefix = if i == state.review_cursor {
                "►  "
            } else {
                "   "
            };
            let color = match (c, i == state.review_cursor) {
                (PostPlanChoice::Terminate, true) => Color::Red,
                (_, true) => Color::Yellow,
                _ => Color::White,
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!("{}{}", prefix, c.label()),
                Style::default().fg(color),
            )]))
        })
        .collect();
    f.render_widget(List::new(choice_items), chunks[5]);

    f.render_widget(
        hint("[↑↓] select  [Enter] confirm  [Ctrl+C] quit"),
        chunks[6],
    );
}
