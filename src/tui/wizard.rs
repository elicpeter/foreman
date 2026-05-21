//! Interactive TUI wizard for `pitboss setup`.
//!
//! Walks new users through pitboss's main configuration knobs — model tier,
//! budget caps, sweep behavior, auditor, test command — with one explanatory
//! screen per topic so the user understands what each setting does. Most
//! screens have sensible defaults (Enter accepts).

use std::path::Path;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::config::Config;
use crate::tui::{TerminalGuard, TICK_INTERVAL};

/// The result of a completed wizard run. No goal field — the wizard
/// collects configuration only; the plan goal is captured later via
/// `pitboss start` (iteration wizard's "New plan" path) or hand-written
/// into `plan.md`.
pub struct WizardResult {
    pub model_preset: ModelPreset,
    /// Token cap for `[budgets] max_total_tokens`. `None` = no cap.
    pub max_run_tokens: Option<u64>,
    /// USD cap for `[budgets] max_total_usd`. `None` = no cap.
    pub max_total_usd: Option<f64>,
    /// Whether `[sweep] enabled` is true (default true).
    pub sweep_enabled: bool,
    /// Override for `[sweep] trigger_min_items`. `None` = pitboss default (5).
    pub sweep_threshold: Option<u32>,
    /// Whether `[audit] enabled` is true (default true).
    pub audit_enabled: bool,
    /// Override for `[tests] command`. `None` = use auto-detection.
    pub test_command_override: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Default)]
pub enum ModelPreset {
    #[default]
    Quality,
    Balanced,
    Fast,
}

impl ModelPreset {
    pub fn planner(self) -> &'static str {
        match self {
            Self::Quality | Self::Balanced => "claude-opus-4-7",
            Self::Fast => "claude-sonnet-4-6",
        }
    }

    pub fn worker(self) -> &'static str {
        match self {
            Self::Quality => "claude-opus-4-7",
            Self::Balanced | Self::Fast => "claude-sonnet-4-6",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Quality => "Best quality   (Opus 4.7 — all roles)",
            Self::Balanced => "Balanced       (Opus plan · Sonnet rest)",
            Self::Fast => "Fastest        (Sonnet 4.6 — all roles)",
        }
    }

    fn explainer(self) -> &'static str {
        match self {
            Self::Quality => {
                "Opus everywhere. Strongest planning + implementation; highest cost."
            }
            Self::Balanced => {
                "Opus drafts the plan, Sonnet executes phases + fixes + audits. Good cost/quality middle ground."
            }
            Self::Fast => "Sonnet everywhere. Cheapest, fastest, slightly weaker on complex phases.",
        }
    }
}

#[derive(Clone, PartialEq)]
enum Step {
    Welcome,
    Models,
    Budget,
    Sweep,
    Audit,
    Tests,
    Confirm,
}

#[derive(Clone, Copy, PartialEq)]
enum BudgetField {
    Usd,
    Tokens,
}

#[derive(Clone, Copy, PartialEq)]
enum SweepField {
    Toggle,
    Threshold,
}

struct WizState {
    step: Step,
    /// When true, the wizard is editing an existing workspace's config —
    /// skip Welcome, start on Confirm (acting as a summary), pre-populate
    /// inputs from the loaded Config, and never touch plan.md / deferred.md.
    existing_mode: bool,

    // Models
    model_cursor: usize,

    // Budget
    budget_usd_input: String,
    budget_tokens_input: String,
    budget_field: BudgetField,

    // Sweep
    sweep_enabled: bool,
    sweep_threshold_input: String,
    sweep_field: SweepField,

    // Audit
    audit_enabled: bool,

    // Tests
    test_input: String,
    detected_test: String,

    // Confirm
    confirm_cursor: usize,
}

impl WizState {
    fn new(workspace: &Path) -> Self {
        Self {
            step: Step::Welcome,
            existing_mode: false,
            model_cursor: 0,
            budget_usd_input: String::new(),
            budget_tokens_input: String::new(),
            budget_field: BudgetField::Usd,
            sweep_enabled: true,
            sweep_threshold_input: String::new(),
            sweep_field: SweepField::Toggle,
            audit_enabled: true,
            test_input: String::new(),
            detected_test: detect_test(workspace),
            confirm_cursor: 0,
        }
    }

    /// Pre-populated from a loaded [`Config`]. Starts the wizard on the
    /// Confirm step — that screen doubles as a summary for users coming in
    /// via `pitboss config` on an existing workspace.
    fn from_existing(workspace: &Path, cfg: &Config) -> Self {
        // Match the user's current model selection back to one of the three
        // preset slots. Anything off-preset (custom model names) falls back
        // to Quality so the user can re-pick.
        let model_cursor = match (cfg.models.planner.as_str(), cfg.models.implementer.as_str()) {
            ("claude-opus-4-7", "claude-opus-4-7") => 0,
            ("claude-opus-4-7", "claude-sonnet-4-6") => 1,
            ("claude-sonnet-4-6", "claude-sonnet-4-6") => 2,
            _ => 0,
        };
        Self {
            step: Step::Confirm,
            existing_mode: true,
            model_cursor,
            budget_usd_input: cfg
                .budgets
                .max_total_usd
                .map(|c| format!("{c:.2}"))
                .unwrap_or_default(),
            budget_tokens_input: cfg
                .budgets
                .max_total_tokens
                .map(|c| c.to_string())
                .unwrap_or_default(),
            budget_field: BudgetField::Usd,
            sweep_enabled: cfg.sweep.enabled,
            sweep_threshold_input: cfg.sweep.trigger_min_items.to_string(),
            sweep_field: SweepField::Toggle,
            audit_enabled: cfg.audit.enabled,
            test_input: cfg.tests.command.clone().unwrap_or_default(),
            detected_test: detect_test(workspace),
            confirm_cursor: 0,
        }
    }

    fn model_preset(&self) -> ModelPreset {
        match self.model_cursor {
            0 => ModelPreset::Quality,
            1 => ModelPreset::Balanced,
            _ => ModelPreset::Fast,
        }
    }

    fn build_result(&self) -> WizardResult {
        WizardResult {
            model_preset: self.model_preset(),
            max_run_tokens: self.budget_tokens_input.trim().parse::<u64>().ok(),
            max_total_usd: self.budget_usd_input.trim().parse::<f64>().ok(),
            sweep_enabled: self.sweep_enabled,
            sweep_threshold: self.sweep_threshold_input.trim().parse::<u32>().ok(),
            audit_enabled: self.audit_enabled,
            test_command_override: {
                let t = self.test_input.trim();
                if t.is_empty() {
                    None
                } else {
                    Some(t.to_string())
                }
            },
        }
    }
}

enum Ev {
    Continue,
    Quit,
    Done(WizardResult),
}

/// Probe for the most likely test runner based on project layout. Probe order
/// is "most common in pitboss's expected user base" (Rust → Node → Python →
/// Go). Returns a display hint shown on the Tests screen; `build_result` uses
/// `test_input` (the user override), not this value directly.
fn detect_test(workspace: &Path) -> String {
    if workspace.join("Cargo.toml").is_file() {
        "cargo test".into()
    } else if workspace.join("package.json").is_file() {
        "npm test".into()
    } else if workspace.join("pyproject.toml").is_file() || workspace.join("setup.py").is_file() {
        "pytest".into()
    } else if workspace.join("go.mod").is_file() {
        "go test ./...".into()
    } else {
        "not detected".into()
    }
}

/// Entry point for a fresh-workspace setup. Walks the user through every
/// step starting at Welcome.
pub async fn run_wizard(workspace: &Path) -> Result<Option<WizardResult>> {
    run_wizard_inner(WizState::new(workspace)).await
}

/// Entry point for editing an existing workspace's config. Pre-populates
/// every field from the loaded [`Config`] and starts the wizard on the
/// Confirm step so the user sees a summary first.
pub async fn run_wizard_existing(workspace: &Path, cfg: &Config) -> Result<Option<WizardResult>> {
    run_wizard_inner(WizState::from_existing(workspace, cfg)).await
}

async fn run_wizard_inner(initial_state: WizState) -> Result<Option<WizardResult>> {
    let mut guard = TerminalGuard::setup()?;
    // Force a real clear-screen escape. `Clear` widget alone is a no-op on
    // first draw because the ratatui diff compares against an empty buffer —
    // so prior terminal contents leak through the gaps around our centered
    // dialog. `terminal.clear()` sends the actual `ED` sequence.
    guard.terminal().clear()?;
    let mut state = initial_state;
    let mut input = EventStream::new();

    let result = loop {
        guard.terminal().draw(|f| render(f, &state))?;

        tokio::select! {
            ev = input.next() => {
                match ev {
                    Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        match on_key(&mut state, key.code, key.modifiers) {
                            Ev::Continue => {}
                            Ev::Quit => break None,
                            Ev::Done(r) => break Some(r),
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => break None,
                }
            }
            _ = tokio::time::sleep(TICK_INTERVAL) => {}
        }
    };

    guard.restore()?;
    Ok(result)
}

fn on_key(s: &mut WizState, code: KeyCode, mods: KeyModifiers) -> Ev {
    if matches!(code, KeyCode::Char('c')) && mods.contains(KeyModifiers::CONTROL) {
        return Ev::Quit;
    }
    match s.step.clone() {
        Step::Welcome => welcome_key(s, code),
        Step::Models => models_key(s, code),
        Step::Budget => budget_key(s, code),
        Step::Sweep => sweep_key(s, code),
        Step::Audit => audit_key(s, code),
        Step::Tests => tests_key(s, code),
        Step::Confirm => confirm_key(s, code),
    }
}

fn welcome_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Enter | KeyCode::Char(' ') => {
            s.step = Step::Models;
            Ev::Continue
        }
        KeyCode::Char('q') => Ev::Quit,
        _ => Ev::Continue,
    }
}

fn models_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Up => {
            s.model_cursor = s.model_cursor.saturating_sub(1);
            Ev::Continue
        }
        KeyCode::Down => {
            if s.model_cursor < 2 {
                s.model_cursor += 1;
            }
            Ev::Continue
        }
        KeyCode::Enter => {
            s.step = Step::Budget;
            Ev::Continue
        }
        KeyCode::Esc => {
            // Edit mode entered the wizard at Confirm and only walks
            // through config screens; Esc returns there. Create mode
            // came from Welcome.
            s.step = if s.existing_mode {
                Step::Confirm
            } else {
                Step::Welcome
            };
            Ev::Continue
        }
        _ => Ev::Continue,
    }
}

fn budget_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Enter => {
            s.step = Step::Sweep;
            Ev::Continue
        }
        KeyCode::Esc => {
            s.step = Step::Models;
            Ev::Continue
        }
        KeyCode::Tab | KeyCode::Down => {
            s.budget_field = match s.budget_field {
                BudgetField::Usd => BudgetField::Tokens,
                BudgetField::Tokens => BudgetField::Usd,
            };
            Ev::Continue
        }
        KeyCode::BackTab | KeyCode::Up => {
            s.budget_field = match s.budget_field {
                BudgetField::Usd => BudgetField::Tokens,
                BudgetField::Tokens => BudgetField::Usd,
            };
            Ev::Continue
        }
        KeyCode::Char(c) => {
            // Allow digits and a single '.' in USD; digits only in tokens.
            let target = match s.budget_field {
                BudgetField::Usd => &mut s.budget_usd_input,
                BudgetField::Tokens => &mut s.budget_tokens_input,
            };
            let ok = match s.budget_field {
                BudgetField::Usd => c.is_ascii_digit() || (c == '.' && !target.contains('.')),
                BudgetField::Tokens => c.is_ascii_digit(),
            };
            if ok {
                target.push(c);
            }
            Ev::Continue
        }
        KeyCode::Backspace => {
            let target = match s.budget_field {
                BudgetField::Usd => &mut s.budget_usd_input,
                BudgetField::Tokens => &mut s.budget_tokens_input,
            };
            target.pop();
            Ev::Continue
        }
        _ => Ev::Continue,
    }
}

fn sweep_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Enter => {
            s.step = Step::Audit;
            Ev::Continue
        }
        KeyCode::Esc => {
            s.step = Step::Budget;
            Ev::Continue
        }
        KeyCode::Tab | KeyCode::Down | KeyCode::Up | KeyCode::BackTab => {
            // Threshold field is only meaningful when sweeps are enabled;
            // skip past it when disabled.
            s.sweep_field = match (s.sweep_field, s.sweep_enabled) {
                (SweepField::Toggle, true) => SweepField::Threshold,
                _ => SweepField::Toggle,
            };
            Ev::Continue
        }
        KeyCode::Char(' ') if s.sweep_field == SweepField::Toggle => {
            s.sweep_enabled = !s.sweep_enabled;
            Ev::Continue
        }
        KeyCode::Char(c) if s.sweep_field == SweepField::Threshold && c.is_ascii_digit() => {
            s.sweep_threshold_input.push(c);
            Ev::Continue
        }
        KeyCode::Backspace if s.sweep_field == SweepField::Threshold => {
            s.sweep_threshold_input.pop();
            Ev::Continue
        }
        _ => Ev::Continue,
    }
}

fn audit_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Enter => {
            s.step = Step::Tests;
            Ev::Continue
        }
        KeyCode::Esc => {
            s.step = Step::Sweep;
            Ev::Continue
        }
        KeyCode::Char(' ') => {
            s.audit_enabled = !s.audit_enabled;
            Ev::Continue
        }
        _ => Ev::Continue,
    }
}

fn tests_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Enter => {
            s.step = Step::Confirm;
            Ev::Continue
        }
        KeyCode::Esc => {
            s.step = Step::Audit;
            Ev::Continue
        }
        KeyCode::Char(c) => {
            s.test_input.push(c);
            Ev::Continue
        }
        KeyCode::Backspace => {
            s.test_input.pop();
            Ev::Continue
        }
        _ => Ev::Continue,
    }
}

fn confirm_key(s: &mut WizState, code: KeyCode) -> Ev {
    match code {
        KeyCode::Up => {
            s.confirm_cursor = s.confirm_cursor.saturating_sub(1);
            Ev::Continue
        }
        KeyCode::Down => {
            if s.confirm_cursor < 1 {
                s.confirm_cursor += 1;
            }
            Ev::Continue
        }
        KeyCode::Enter => match s.confirm_cursor {
            0 => Ev::Done(s.build_result()),
            _ => {
                // "Back to edit settings" — re-walk the config screens
                // starting at Models. Same target in both modes since the
                // wizard no longer collects a goal.
                s.step = Step::Models;
                Ev::Continue
            }
        },
        KeyCode::Esc => {
            // Edit mode: Esc on the summary screen cancels the whole wizard
            // (so the user can back out without changes). Create mode keeps
            // its previous "go back to Tests" behavior.
            if s.existing_mode {
                return Ev::Quit;
            }
            s.step = Step::Tests;
            Ev::Continue
        }
        KeyCode::Char('q') => Ev::Quit,
        _ => Ev::Continue,
    }
}

// ── rendering ────────────────────────────────────────────────────────────────

fn render(f: &mut ratatui::Frame<'_>, state: &WizState) {
    let area = f.area();
    // Clear the whole screen first so scrollback behind the alternate-screen
    // buffer doesn't bleed through the gaps around the centered dialog.
    f.render_widget(Clear, area);

    let dialog = centered_rect(80, 32, area);
    match state.step {
        Step::Welcome => render_welcome(f, dialog, state),
        Step::Models => render_models(f, dialog, state),
        Step::Budget => render_budget(f, dialog, state),
        Step::Sweep => render_sweep(f, dialog, state),
        Step::Audit => render_audit(f, dialog, state),
        Step::Tests => render_tests(f, dialog, state),
        Step::Confirm => render_confirm(f, dialog, state),
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

fn text_input_widget(value: &str, focused: bool, area: Rect) -> Paragraph<'_> {
    let display = if focused {
        format!("{}█", value)
    } else {
        value.to_string()
    };
    let scroll = compute_input_scroll(&display, area);
    let border = if focused {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    Paragraph::new(display)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
}

/// Vertical scroll so the cursor stays visible. Simpler than
/// `tui::iteration::cursor_auto_scroll` because the wizard's inputs are
/// append-only — the cursor is always at the end of `display`, so scrolling
/// just needs to ensure the last line is in view. Kept local to avoid a
/// cross-module dependency for a three-line calculation.
fn compute_input_scroll(display: &str, area: Rect) -> u16 {
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2);
    let total_chars = display.chars().count() as u16;
    let needed = total_chars.max(1).div_ceil(inner_w);
    needed.saturating_sub(inner_h)
}

// Crumb shown at the top of every step so the user knows where they are.
// Edit mode has 5 config steps (Models→Tests) since Welcome and Goal are
// skipped; the summary screen sits outside the numbered flow.
fn step_crumb(state: &WizState) -> Line<'_> {
    let text = if state.existing_mode {
        match state.step {
            Step::Models => "step 1/5  •  models".to_string(),
            Step::Budget => "step 2/5  •  budget".to_string(),
            Step::Sweep => "step 3/5  •  sweeps".to_string(),
            Step::Audit => "step 4/5  •  auditor".to_string(),
            Step::Tests => "step 5/5  •  tests".to_string(),
            Step::Confirm => "current configuration".to_string(),
            // Welcome isn't reachable in edit mode, fall back gracefully.
            Step::Welcome => "current configuration".to_string(),
        }
    } else {
        match state.step {
            Step::Welcome => "step 0/6  •  welcome".to_string(),
            Step::Models => "step 1/6  •  models".to_string(),
            Step::Budget => "step 2/6  •  budget".to_string(),
            Step::Sweep => "step 3/6  •  sweeps".to_string(),
            Step::Audit => "step 4/6  •  auditor".to_string(),
            Step::Tests => "step 5/6  •  tests".to_string(),
            Step::Confirm => "step 6/6  •  ready".to_string(),
        }
    };
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
}

fn render_welcome(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let block = dialog_block("pitboss  setup");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(3), // tagline
            Constraint::Length(1), // spacer
            Constraint::Length(1), // "What we'll cover" label
            Constraint::Min(1),    // step list
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "pitboss runs AI agents through a phased plan you write.",
                Style::default().fg(Color::White),
            )),
            Line::from(Span::styled(
                "Setup is a quick walkthrough of how the runner should behave —",
                Style::default().fg(Color::White),
            )),
            Line::from(Span::styled(
                "models, budget caps, sweeps, auditor, tests.",
                Style::default().fg(Color::White),
            )),
        ])
        .wrap(Wrap { trim: false }),
        chunks[2],
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "What we'll cover:",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[4],
    );

    let steps = vec![
        "  1.  Models         — quality vs. cost tradeoff",
        "  2.  Budget         — USD + token caps (optional)",
        "  3.  Sweeps         — how pitboss drains deferred work",
        "  4.  Auditor        — reviews each phase's diff before commit",
        "  5.  Tests          — the suite pitboss runs after every phase",
        "  6.  Ready          — save your config and you're done",
        "",
        "  Every screen has a sensible default — press Enter to accept.",
        "  Press Esc to go back to the previous step.",
        "",
        "  (Describe what pitboss should build later via `pitboss start`,",
        "   or edit .pitboss/play/plan.md directly.)",
    ];
    let lines: Vec<Line> = steps
        .iter()
        .map(|s| {
            let style = if s.starts_with("  Every") || s.starts_with("  Press") {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };
            Line::from(Span::styled(*s, style))
        })
        .collect();
    f.render_widget(Paragraph::new(lines), chunks[5]);

    f.render_widget(hint("[Enter] start    [q] quit"), chunks[6]);
}

fn render_models(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let block = dialog_block("pitboss  setup");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(1), // label
            Constraint::Length(2), // explanation
            Constraint::Length(1), // spacer
            Constraint::Length(5), // model list
            Constraint::Length(1), // spacer
            Constraint::Length(2), // selected explainer
            Constraint::Min(1),
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Pick a model tier",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "pitboss uses different models for planning, implementing, auditing,",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "and fixing. The tier picks all four. You can edit individually later.",
                Style::default().fg(Color::Gray),
            )),
        ]),
        chunks[3],
    );

    let presets = [
        ModelPreset::Quality,
        ModelPreset::Balanced,
        ModelPreset::Fast,
    ];
    let items: Vec<ListItem> = presets
        .iter()
        .enumerate()
        .map(|(i, preset)| {
            let prefix = if i == state.model_cursor {
                "►  "
            } else {
                "   "
            };
            let style = if i == state.model_cursor {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(vec![Span::styled(
                format!("{}{}", prefix, preset.label()),
                style,
            )]))
        })
        .collect();
    f.render_widget(List::new(items), chunks[5]);

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            state.model_preset().explainer(),
            Style::default().fg(Color::DarkGray),
        )]))
        .wrap(Wrap { trim: false }),
        chunks[7],
    );

    f.render_widget(hint("[↑↓] select    [Enter] next    [Esc] back"), chunks[9]);
}

fn render_budget(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let block = dialog_block("pitboss  setup");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(1), // label
            Constraint::Length(3), // explanation
            Constraint::Length(1), // spacer
            Constraint::Length(1), // usd label
            Constraint::Length(3), // usd input
            Constraint::Length(1), // tokens label
            Constraint::Length(3), // tokens input
            Constraint::Min(1),
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Budget caps  (optional)",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "pitboss halts before the next dispatch that would exceed either",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "cap. Leave either blank for no limit. `pitboss rebuy` resumes",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "after you raise the cap.",
                Style::default().fg(Color::Gray),
            )),
        ]),
        chunks[3],
    );

    let usd_focused = state.budget_field == BudgetField::Usd;
    let tokens_focused = state.budget_field == BudgetField::Tokens;

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Max USD spend  (e.g. 5.00)",
            Style::default().fg(if usd_focused {
                Color::Yellow
            } else {
                Color::Gray
            }),
        )])),
        chunks[5],
    );
    f.render_widget(
        text_input_widget(&state.budget_usd_input, usd_focused, chunks[6]),
        chunks[6],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Max tokens  (e.g. 1000000)",
            Style::default().fg(if tokens_focused {
                Color::Yellow
            } else {
                Color::Gray
            }),
        )])),
        chunks[7],
    );
    f.render_widget(
        text_input_widget(&state.budget_tokens_input, tokens_focused, chunks[8]),
        chunks[8],
    );

    f.render_widget(
        hint("[Tab] switch field    [Enter] next    [Esc] back"),
        chunks[10],
    );
}

fn render_sweep(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let block = dialog_block("pitboss  setup");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(1), // label
            Constraint::Length(4), // explanation
            Constraint::Length(1), // spacer
            Constraint::Length(1), // toggle
            Constraint::Length(1), // spacer
            Constraint::Length(1), // threshold label
            Constraint::Length(3), // threshold input
            Constraint::Min(1),
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Deferred-item sweeps",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "When agents can't finish something in a phase, it lands in",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "deferred.md. A sweep is a side dispatch between phases that",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "drains the backlog. Triggered when the unchecked count crosses",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "the threshold. Recommended on.",
                Style::default().fg(Color::Gray),
            )),
        ]),
        chunks[3],
    );

    let toggle_focused = state.sweep_field == SweepField::Toggle;
    let mark = if state.sweep_enabled { "[x]" } else { "[ ]" };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("{} Enable sweeps  (Space to toggle)", mark),
            Style::default().fg(if toggle_focused {
                Color::Yellow
            } else {
                Color::White
            }),
        )])),
        chunks[5],
    );

    let threshold_focused = state.sweep_field == SweepField::Threshold;
    let threshold_color = if !state.sweep_enabled {
        Color::DarkGray
    } else if threshold_focused {
        Color::Yellow
    } else {
        Color::Gray
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Trigger threshold  (unchecked items before a sweep; default 5)",
            Style::default().fg(threshold_color),
        )])),
        chunks[7],
    );
    f.render_widget(
        text_input_widget(
            &state.sweep_threshold_input,
            threshold_focused && state.sweep_enabled,
            chunks[8],
        ),
        chunks[8],
    );

    f.render_widget(
        hint("[Tab] switch  [Space] toggle  [Enter] next  [Esc] back"),
        chunks[10],
    );
}

fn render_audit(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let block = dialog_block("pitboss  setup");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(1), // label
            Constraint::Length(4), // explanation
            Constraint::Length(1), // spacer
            Constraint::Length(1), // toggle
            Constraint::Min(1),
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Auditor pass",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "After tests pass, the auditor reviews the staged diff. Small",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "fixes are inlined; larger findings go to deferred.md for the",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "next sweep. Adds quality at the cost of an extra agent call",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "per phase. Recommended on.",
                Style::default().fg(Color::Gray),
            )),
        ]),
        chunks[3],
    );

    let mark = if state.audit_enabled { "[x]" } else { "[ ]" };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("{} Enable auditor  (Space to toggle)", mark),
            Style::default().fg(Color::Yellow),
        )])),
        chunks[5],
    );

    f.render_widget(
        hint("[Space] toggle    [Enter] next    [Esc] back"),
        chunks[7],
    );
}

fn render_tests(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let block = dialog_block("pitboss  setup");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(1), // label
            Constraint::Length(3), // explanation
            Constraint::Length(1), // detected line
            Constraint::Length(1), // spacer
            Constraint::Length(1), // override label
            Constraint::Length(3), // override input
            Constraint::Min(1),
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Test command",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[2],
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "pitboss runs your test suite after every phase. If tests fail,",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "the fixer agent is dispatched up to retries.fixer_max_attempts",
                Style::default().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                "times before halting.",
                Style::default().fg(Color::Gray),
            )),
        ]),
        chunks[3],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            format!("Auto-detected: {}", state.detected_test),
            Style::default().fg(Color::Cyan),
        )])),
        chunks[4],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Override  (blank = keep detected):",
            Style::default().fg(Color::Yellow),
        )])),
        chunks[6],
    );
    f.render_widget(
        text_input_widget(&state.test_input, true, chunks[7]),
        chunks[7],
    );
    f.render_widget(hint("[Enter] next    [Esc] back"), chunks[9]);
}

fn render_confirm(f: &mut ratatui::Frame<'_>, area: Rect, state: &WizState) {
    let title = if state.existing_mode {
        "pitboss  config"
    } else {
        "pitboss  config  ✓"
    };
    let block = dialog_block(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(1), // crumb
            Constraint::Length(1), // spacer
            Constraint::Length(1), // "Your settings:" label
            Constraint::Length(8), // summary
            Constraint::Length(1), // spacer
            Constraint::Length(1), // "What next?" label
            Constraint::Min(3),    // choices
            Constraint::Length(1), // footer
        ])
        .split(inner);

    f.render_widget(Paragraph::new(step_crumb(state)), chunks[0]);

    let summary_header = if state.existing_mode {
        "Current configuration:"
    } else {
        "Your settings:"
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            summary_header,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[2],
    );

    let usd = if state.budget_usd_input.trim().is_empty() {
        "no cap".to_string()
    } else {
        format!("${}", state.budget_usd_input.trim())
    };
    let tokens = if state.budget_tokens_input.trim().is_empty() {
        "no cap".to_string()
    } else {
        format!("{} tokens", state.budget_tokens_input.trim())
    };
    let sweep_label = if state.sweep_enabled {
        let threshold = if state.sweep_threshold_input.trim().is_empty() {
            "5 (default)".to_string()
        } else {
            state.sweep_threshold_input.trim().to_string()
        };
        format!("enabled (threshold: {})", threshold)
    } else {
        "disabled".to_string()
    };
    let audit_label = if state.audit_enabled {
        "enabled"
    } else {
        "disabled"
    };
    let test_label = if state.test_input.trim().is_empty() {
        format!("auto-detected ({})", state.detected_test)
    } else {
        state.test_input.trim().to_string()
    };

    // The wizard collects config only — the plan goal lives in plan.md and
    // is captured by `pitboss start` (iteration wizard) or hand-edited.
    let mut summary: Vec<Line> = Vec::new();
    summary.extend([
        Line::from(Span::styled(
            format!(
                "  Models     {} / {}",
                state.model_preset().planner(),
                state.model_preset().worker()
            ),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  Budget     {} USD  •  {}", usd, tokens),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  Sweeps     {}", sweep_label),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  Auditor    {}", audit_label),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  Tests      {}", test_label),
            Style::default().fg(Color::White),
        )),
    ]);
    f.render_widget(Paragraph::new(summary), chunks[3]);

    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "What next?",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )])),
        chunks[5],
    );

    let choices: [&str; 2] = if state.existing_mode {
        [
            "Save changes  (rewrites .pitboss/config.toml)",
            "Edit settings",
        ]
    } else {
        [
            "Save settings  (you'll write plan.md next)",
            "Back to edit settings",
        ]
    };
    let items: Vec<ListItem> = choices
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let prefix = if i == state.confirm_cursor {
                "►  "
            } else {
                "   "
            };
            let style = if i == state.confirm_cursor {
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
    f.render_widget(List::new(items), chunks[6]);
    f.render_widget(
        hint("[↑↓] select    [Enter] confirm    [Esc] back"),
        chunks[7],
    );
}
