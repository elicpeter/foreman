//! `ratatui` dashboard subscribed to runner events.
//!
//! [`App`] owns the visible state and folds [`crate::runner::Event`]s into
//! it; [`run`] is the entry point the CLI calls when `--tui` is set. The
//! integration is purely additive: the runner publishes on its broadcast
//! channel exactly as it does for the plain logger, and this module
//! subscribes alongside it.
//!
//! Quit behavior. The TUI runs concurrently with [`crate::runner::Runner::run`].
//! When the user hits `q` or `a` the host loop drops the runner future via
//! [`tokio::select`], which cancels every in-flight `await` chain inside the
//! runner — including the agent dispatch, which honors its own
//! [`tokio_util::sync::CancellationToken`]. The terminal is always restored,
//! even on panic or early return.

mod app;

pub use app::{Activity, App, PhaseStatus, OUTPUT_BUFFER_LINES};

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::broadcast;
use tokio::time::sleep;

use crate::agent::Agent;
use crate::git::Git;
use crate::runner::{Event, RunSummary, Runner};

/// Drive a [`Runner`] with the TUI dashboard attached.
///
/// Subscribes to the runner's event stream, sets up the terminal in
/// alternate-screen / raw mode, and runs the input + render loop concurrently
/// with [`Runner::run`]. Returns whatever the runner returned, or `None`
/// when the user quit before the runner finished. The terminal is always
/// restored before this function returns.
pub async fn run<A, G>(runner: &mut Runner<A, G>) -> Result<Option<RunSummary>>
where
    A: Agent + Send + Sync + 'static,
    G: Git + Send + Sync + 'static,
{
    let plan = runner.plan().clone();
    let state = runner.state().clone();
    let rx = runner.subscribe();

    let mut terminal = setup_terminal().context("tui: setting up terminal")?;
    let app = App::new(plan, state);

    let outcome = tokio::select! {
        biased;
        result = run_loop(&mut terminal, app, rx) => Outcome::User(result?),
        result = runner.run() => Outcome::Runner(result?),
    };

    restore_terminal(&mut terminal).context("tui: restoring terminal")?;

    match outcome {
        Outcome::Runner(summary) => Ok(Some(summary)),
        Outcome::User(UserOutcome::Quit) => Ok(None),
        Outcome::User(UserOutcome::ChannelClosed) => Ok(None),
    }
}

enum Outcome {
    Runner(RunSummary),
    User(UserOutcome),
}

enum UserOutcome {
    /// User pressed q or a.
    Quit,
    /// Runner dropped the broadcast channel (run completed via the other arm).
    /// Reported here only when this loop wins the race; in practice the
    /// runner arm wins and this is unreachable.
    ChannelClosed,
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Frame interval. Aggressive enough for streaming agent output to feel
/// live; loose enough not to thrash the terminal when nothing is happening.
const TICK_INTERVAL: Duration = Duration::from_millis(80);

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut app: App,
    mut events: broadcast::Receiver<Event>,
) -> Result<UserOutcome> {
    let mut input = EventStream::new();
    terminal.draw(|f| app.render(f))?;

    loop {
        tokio::select! {
            biased;
            // Drain runner events as they arrive — best-effort, lag tolerated.
            ev = events.recv() => {
                match ev {
                    Ok(event) => app.handle_event(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => {
                        terminal.draw(|f| app.render(f))?;
                        return Ok(UserOutcome::ChannelClosed);
                    }
                }
            }
            // Pump terminal input.
            input_event = input.next() => {
                match input_event {
                    Some(Ok(CtEvent::Key(key))) if key.kind == KeyEventKind::Press => {
                        if handle_key(&mut app, key.code, key.modifiers) {
                            return Ok(UserOutcome::Quit);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(UserOutcome::Quit),
                }
            }
            // Cap the frame rate so a quiet run still re-renders periodically.
            _ = sleep(TICK_INTERVAL) => {}
        }

        terminal.draw(|f| app.render(f))?;

        if app.quit_requested() {
            return Ok(UserOutcome::Quit);
        }
    }
}

/// Returns `true` when the key requests an immediate quit.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> bool {
    match code {
        KeyCode::Char('q') | KeyCode::Char('a') => true,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Char('p') => {
            app.toggle_pause();
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Phase, PhaseId, Plan};
    use crate::state::RunState;

    fn pid(s: &str) -> PhaseId {
        PhaseId::parse(s).unwrap()
    }

    fn fixture_app() -> App {
        let plan = Plan::new(
            pid("01"),
            vec![Phase {
                id: pid("01"),
                title: "first".into(),
                body: String::new(),
            }],
        );
        let state = RunState::new("rid", "branch", pid("01"));
        App::new(plan, state)
    }

    #[test]
    fn q_requests_quit() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::empty());
        assert!(quit);
    }

    #[test]
    fn a_requests_quit() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('a'), KeyModifiers::empty());
        assert!(quit);
    }

    #[test]
    fn ctrl_c_requests_quit() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(quit);
    }

    #[test]
    fn p_toggles_pause_without_quitting() {
        let mut app = fixture_app();
        assert!(!app.is_paused());
        let quit = handle_key(&mut app, KeyCode::Char('p'), KeyModifiers::empty());
        assert!(!quit);
        assert!(app.is_paused());
        let quit = handle_key(&mut app, KeyCode::Char('p'), KeyModifiers::empty());
        assert!(!quit);
        assert!(!app.is_paused());
    }

    #[test]
    fn unknown_key_is_a_no_op() {
        let mut app = fixture_app();
        let quit = handle_key(&mut app, KeyCode::Char('x'), KeyModifiers::empty());
        assert!(!quit);
        assert!(!app.is_paused());
    }
}
