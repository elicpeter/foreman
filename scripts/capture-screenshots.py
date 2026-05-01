#!/usr/bin/env python3
"""Capture the README screenshots: TUI dashboard, `pitboss status`, halted run, grind dashboard.

Each scene is captured with `vhs` (charm.sh), then framed with a cyan
gradient that anchors on the wordmark color (#06b6d4 cyan-500). Output
PNGs land in `assets/`.

Pipeline per scene:
  1. set up whatever the scene needs
     - tui / halt / grind: a temp Rust crate that depends on `pitboss`
       via path and renders a single ratatui frame from a deterministic
       App state, then sleeps long enough for vhs to screenshot it
     - status: a temp pitboss workspace (real `git init`, plan.md,
       deferred.md, pitboss.toml, .pitboss/state.json, one phase 01
       commit) so `pitboss status` produces realistic output
  2. write a vhs tape, run vhs to produce a PNG
  3. wrap the PNG in the cyan gradient frame and save into assets/

Re-run any time. Everything below /tmp/pitboss-screenshots-* is torn
down on exit (success or failure).

Prerequisites: vhs, python3+Pillow, rust toolchain, git.
Usage: python3 scripts/capture-screenshots.py
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
PITBOSS_BIN = ROOT / "target" / "release" / "pitboss"

# Frame geometry.
PAD_L, PAD_T, PAD_R, PAD_B = 80, 50, 80, 50
CORNER_RADIUS = 12

# Cyan gradient anchored on the wordmark cyan (#06b6d4). Lightest in TL,
# darkest in BR — the off-diagonal corners hold the wordmark color so
# the frame reads as a tonal range *of* the brand color, not a wash
# next to it.
GRAD_TL = (0xcf, 0xfa, 0xfe)  # cyan-100  — very light, airy
GRAD_TR = (0x38, 0xbd, 0xf8)  # sky-400   — vivid sky blue
GRAD_BL = (0x22, 0xd3, 0xee)  # cyan-400  — vivid cyan
GRAD_BR = (0x0e, 0x74, 0x90)  # cyan-700  — deep teal

# vhs visuals. One source of truth so every scene matches.
VHS_THEME = "Dracula"
VHS_FONT_SIZE = 14
VHS_PADDING = 24


def log(msg: str) -> None:
    print(f"[capture] {msg}", file=sys.stderr)


def run(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(cmd, check=True, **kwargs)


# ---------------------------------------------------------------------------
# Frame compositing
# ---------------------------------------------------------------------------


def make_gradient(w: int, h: int) -> Image.Image:
    """Bilinear gradient between the four GRAD_* corners.

    Built row-by-row by interpolating two edge gradients (top and bottom),
    then vertically blending — keeps a 1700x600 canvas under a few hundred
    ms vs the ten-second pure-Python pixel loop.
    """

    def edge(left, right):
        row = Image.new("RGB", (w, 1))
        px = row.load()
        for x in range(w):
            t = x / (w - 1) if w > 1 else 0.0
            px[x, 0] = (
                int(left[0] + (right[0] - left[0]) * t),
                int(left[1] + (right[1] - left[1]) * t),
                int(left[2] + (right[2] - left[2]) * t),
            )
        return row

    top, bot = edge(GRAD_TL, GRAD_TR), edge(GRAD_BL, GRAD_BR)
    out = Image.new("RGB", (w, h))
    for y in range(h):
        t = y / (h - 1) if h > 1 else 0.0
        out.paste(Image.blend(top, bot, t), (0, y))
    return out


def round_corners(img: Image.Image, radius: int) -> Image.Image:
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (0, 0, img.size[0], img.size[1]), radius=radius, fill=255
    )
    out = img.convert("RGBA")
    out.putalpha(mask)
    return out


def frame_png(src: Path, dst: Path) -> None:
    inner = Image.open(src).convert("RGB")
    iw, ih = inner.size
    ow, oh = iw + PAD_L + PAD_R, ih + PAD_T + PAD_B
    bg = make_gradient(ow, oh).convert("RGBA")
    rounded = round_corners(inner, CORNER_RADIUS)
    bg.paste(rounded, (PAD_L, PAD_T), rounded)
    bg.convert("RGB").save(dst, "PNG", optimize=True)
    log(f"framed → {dst.relative_to(ROOT)}")


# ---------------------------------------------------------------------------
# Demo binary (TUI + halt scenes)
# ---------------------------------------------------------------------------


DEMO_CARGO_TOML = f"""[package]
name = "pitboss-screenshots-demo"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
pitboss = {{ path = "{ROOT}" }}
ratatui = "0.30"
crossterm = "0.29"
chrono = "0.4"
"""

DEMO_MAIN_RS = r"""
use std::env;
use std::io;
use std::thread::sleep;
use std::time::Duration;

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{Duration as ChronoDuration, Utc};

use pitboss::config::ModelPricing;
use pitboss::git::CommitId;
use pitboss::grind::{GrindEvent, PlanBudgets, SessionRecord, SessionStatus};
use pitboss::plan::{Phase, PhaseId, Plan};
use pitboss::runner::{Event, HaltReason};
use pitboss::state::{RunState, TokenUsage};
use pitboss::tui::grind::GrindApp;
use pitboss::tui::{AgentDisplay, App, UsageView};

fn pid(s: &str) -> PhaseId {
    PhaseId::parse(s).unwrap()
}

fn three_phase_plan() -> Plan {
    Plan::new(
        pid("01"),
        vec![
            Phase {
                id: pid("01"),
                title: "Project foundation".into(),
                body: String::new(),
            },
            Phase {
                id: pid("02"),
                title: "Domain types".into(),
                body: String::new(),
            },
            Phase {
                id: pid("03"),
                title: "Plan parser".into(),
                body: String::new(),
            },
        ],
    )
}

fn fresh_state() -> RunState {
    RunState::new(
        "20260430T120000Z",
        "pitboss/run-20260430T120000Z",
        pid("01"),
    )
}

fn demo_agent() -> AgentDisplay {
    AgentDisplay {
        agent_name: "claude-code".into(),
        implementer_model: "claude-opus-4-7".into(),
        fixer_model: "claude-sonnet-4-6".into(),
        auditor_model: "claude-sonnet-4-6".into(),
    }
}

fn demo_usage_view() -> UsageView {
    let mut pricing = HashMap::new();
    pricing.insert(
        "claude-opus-4-7".to_string(),
        ModelPricing {
            input_per_million_usd: 15.0,
            output_per_million_usd: 75.0,
        },
    );
    pricing.insert(
        "claude-sonnet-4-6".to_string(),
        ModelPricing {
            input_per_million_usd: 3.0,
            output_per_million_usd: 15.0,
        },
    );
    UsageView {
        role_models: vec![
            ("planner".into(), "claude-opus-4-7".into()),
            ("implementer".into(), "claude-opus-4-7".into()),
            ("fixer".into(), "claude-sonnet-4-6".into()),
            ("auditor".into(), "claude-sonnet-4-6".into()),
        ],
        pricing,
    }
}

fn build_tui() -> App {
    let mut app = App::new(three_phase_plan(), fresh_state(), demo_agent(), demo_usage_view());
    app.handle_event(Event::PhaseStarted {
        phase_id: pid("01"),
        title: "Project foundation".into(),
        attempt: 1,
    });
    app.handle_event(Event::AgentStdout("Reading plan.md".into()));
    app.handle_event(Event::AgentStdout("Editing src/lib.rs".into()));
    app.handle_event(Event::TestStarted);
    app.handle_event(Event::TestFinished {
        passed: true,
        summary: "12 passed".into(),
    });
    app.handle_event(Event::PhaseCommitted {
        phase_id: pid("01"),
        commit: Some(CommitId::new("abc1234")),
    });
    app.handle_event(Event::PhaseStarted {
        phase_id: pid("02"),
        title: "Domain types".into(),
        attempt: 1,
    });
    app.handle_event(Event::AgentStdout("Defining PhaseId, Phase, Plan in src/plan/mod.rs".into()));
    app.handle_event(Event::AgentStdout("Adding parse() entry point with thiserror variants".into()));
    app.handle_event(Event::AgentStdout("Wiring serde derives onto Phase and Plan".into()));
    app.handle_event(Event::AgentStdout("Writing roundtrip test for plan::parse".into()));
    app.handle_event(Event::AgentStdout("cargo build --quiet (incremental)".into()));
    app
}

fn build_halt() -> App {
    let mut app = App::new(three_phase_plan(), fresh_state(), demo_agent(), demo_usage_view());
    app.handle_event(Event::PhaseStarted {
        phase_id: pid("02"),
        title: "Domain types".into(),
        attempt: 1,
    });
    app.handle_event(Event::AgentStdout("Defining PhaseId, Phase, Plan in src/plan/mod.rs".into()));
    app.handle_event(Event::AgentStdout("Adjusting parse() for trailing whitespace".into()));
    app.handle_event(Event::AgentStdout("Tightening edge case in dedent".into()));
    app.handle_event(Event::PhaseHalted {
        phase_id: pid("02"),
        reason: HaltReason::BudgetExceeded(
            "USD budget exhausted: $5.0123 spent, cap $5.0000".into(),
        ),
    });
    app
}

fn build_grind() -> GrindApp {
    // Anchor started_at near wall-clock now so the header's "elapsed" cell
    // reads as a few minutes, not as days against a hard-coded date. The
    // per-session offsets below stay relative to this anchor so the
    // SessionFinished records sit just in the past.
    let run_started = Utc::now() - ChronoDuration::seconds(285);
    let budgets = PlanBudgets {
        max_iterations: Some(50),
        until: None,
        max_cost_usd: Some(5.00),
        max_tokens: Some(1_000_000),
    };
    let mut app = GrindApp::new(
        "20260430T143022Z".into(),
        "pitboss/grind/20260430T143022Z".into(),
        "code-quality".into(),
        "claude-code".into(),
        run_started,
        budgets,
    );

    let session = |seq: u32,
                   prompt: &str,
                   status: SessionStatus,
                   start_offset_secs: i64,
                   duration_secs: i64,
                   input: u64,
                   output: u64,
                   cost: f64,
                   commit: Option<&str>|
     -> SessionRecord {
        let started = run_started + ChronoDuration::seconds(start_offset_secs);
        let ended = started + ChronoDuration::seconds(duration_secs);
        SessionRecord {
            seq,
            run_id: "20260430T143022Z".into(),
            prompt: prompt.into(),
            started_at: started,
            ended_at: ended,
            status,
            summary: Some(format!("ran {prompt}")),
            commit: commit.map(CommitId::new),
            tokens: TokenUsage {
                input,
                output,
                by_role: HashMap::new(),
            },
            cost_usd: cost,
            transcript_path: PathBuf::from(format!("transcripts/session-{seq:04}.log")),
        }
    };

    let finished = [
        session(1, "fix-lints",         SessionStatus::Ok,    0,   28, 4_120, 1_310, 0.038, Some("a1b2c3d")),
        session(2, "tighten-tests",     SessionStatus::Ok,    30,  41, 5_870, 1_980, 0.063, Some("b2c3d4e")),
        session(3, "doc-pass",          SessionStatus::Dirty, 75,  19, 2_410,   720, 0.018, None),
        session(4, "fix-lints",         SessionStatus::Ok,    98,  35, 4_960, 1_540, 0.052, Some("c3d4e5f")),
        session(5, "tighten-tests",     SessionStatus::Error, 138, 22, 3_120, 1_010, 0.029, None),
        session(6, "refresh-snapshots", SessionStatus::Ok,    164, 48, 7_240, 2_320, 0.084, Some("d4e5f6a")),
        session(7, "fix-lints",         SessionStatus::Ok,    218, 31, 4_540, 1_410, 0.046, Some("e5f6a7b")),
    ];

    // Skip SessionStarted for finished rows so the dashboard uses each
    // record's started_at/ended_at directly (the SessionStarted handler
    // would otherwise rebase started_at to wall-clock now and the duration
    // cell would render as 0s against the past-dated ended_at).
    for record in &finished {
        app.handle_event(GrindEvent::SessionFinished {
            record: record.clone(),
        });
    }

    // Active session: started but not finished, with output lines so the
    // right pane is populated and the left pane shows a `>` glyph.
    app.handle_event(GrindEvent::SessionStarted {
        seq: 8,
        prompt: "doc-pass".into(),
        parallel_safe: false,
    });
    let active_lines = [
        "Reading scratchpad.md for accumulated notes",
        "Skimming src/grind/run.rs for stale doc comments",
        "tool: Edit",
        "Trimming stale \"Phase 07 shipped...\" line; drift since merge",
        "tool: Edit",
        "Rewording rustdoc on GrindRunner::resume",
        "Writing summary to $PITBOSS_SUMMARY_FILE",
    ];
    for line in active_lines {
        if let Some(name) = line.strip_prefix("tool: ") {
            app.handle_event(GrindEvent::AgentToolUse {
                seq: 8,
                name: name.into(),
            });
        } else {
            app.handle_event(GrindEvent::AgentStdout {
                seq: 8,
                line: line.into(),
            });
        }
    }

    app.handle_event(GrindEvent::SchedulerPicked {
        rotation: 8,
        pick: Some("refresh-snapshots".into()),
    });

    app
}

enum DemoApp {
    Play(App),
    Grind(GrindApp),
}

impl DemoApp {
    fn render(&self, frame: &mut ratatui::Frame) {
        match self {
            DemoApp::Play(a) => a.render(frame),
            DemoApp::Grind(a) => a.render(frame),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scene = env::args().nth(1).unwrap_or_else(|| "tui".to_string());
    let app = match scene.as_str() {
        "tui" => DemoApp::Play(build_tui()),
        "halt" => DemoApp::Play(build_halt()),
        "grind" => DemoApp::Grind(build_grind()),
        other => return Err(format!("unknown scene: {other}").into()),
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result: Result<(), Box<dyn std::error::Error>> = (|| {
        // Initial draw renders the layout; the second draw (after a short
        // pause) lets time-sensitive cells like the header's "elapsed"
        // counter and the in-flight session's duration tick forward before
        // vhs takes its screenshot.
        terminal.draw(|f| app.render(f))?;
        sleep(Duration::from_millis(2500));
        terminal.draw(|f| app.render(f))?;
        sleep(Duration::from_millis(3500));
        Ok(())
    })();

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}
"""


def build_demo_binary(workdir: Path) -> Path:
    """Write Cargo.toml + src/main.rs into `workdir`, build release, return
    path to the compiled binary."""
    src = workdir / "src"
    src.mkdir(parents=True, exist_ok=True)
    (workdir / "Cargo.toml").write_text(DEMO_CARGO_TOML)
    (src / "main.rs").write_text(DEMO_MAIN_RS)
    log("building demo binary")
    run(["cargo", "build", "--release", "--quiet"], cwd=workdir)
    return workdir / "target" / "release" / "pitboss-screenshots-demo"


# ---------------------------------------------------------------------------
# Pitboss status workspace
# ---------------------------------------------------------------------------


STATUS_PLAN_MD = """---
current_phase: "02"
---

# Phase 01: Project foundation

Scaffold the crate, set up CI, drop in a CLAUDE.md.

# Phase 02: Domain types

Define PhaseId, Phase, Plan and their parser.

# Phase 03: Plan parser

Wire the parser into `pitboss init` and `pitboss run`.
"""

STATUS_DEFERRED_MD = """## Deferred items

- [ ] Stricter PhaseId validation (reject leading zeros)
- [x] Document the phase id grammar in plan.md

## Deferred phases

### From phase 02: rework parser entry point
"""

STATUS_PITBOSS_TOML = """[models]
implementer = "claude-opus-4-7"
auditor     = "claude-sonnet-4-6"
fixer       = "claude-sonnet-4-6"

[budgets]
max_total_tokens = 1000000
max_total_usd    = 5.00
"""


def setup_status_workspace(workdir: Path) -> None:
    """Build a workspace that produces a realistic `pitboss status` output:
    one completed phase committed on the run branch, deferred items, real
    token usage so the budget block has numbers in it."""
    workdir.mkdir(parents=True, exist_ok=True)
    play_dir = workdir / ".pitboss" / "play"
    play_dir.mkdir(parents=True, exist_ok=True)
    (play_dir / "plan.md").write_text(STATUS_PLAN_MD)
    (play_dir / "deferred.md").write_text(STATUS_DEFERRED_MD)
    (workdir / ".pitboss" / "config.toml").write_text(STATUS_PITBOSS_TOML)

    # Real git history so `last commit:` resolves to a sensible value.
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "pitboss",
        "GIT_AUTHOR_EMAIL": "pitboss@example.com",
        "GIT_COMMITTER_NAME": "pitboss",
        "GIT_COMMITTER_EMAIL": "pitboss@example.com",
    }
    run(["git", "init", "--quiet", "--initial-branch=main"], cwd=workdir)
    run(["git", "checkout", "--quiet", "-b", "pitboss/run-20260429T143022Z"], cwd=workdir, env=env)
    (workdir / "src").mkdir()
    (workdir / "src" / "lib.rs").write_text("// scaffold\n")
    run(["git", "add", "src/lib.rs"], cwd=workdir, env=env)
    run(
        ["git", "commit", "--quiet", "-m", "[pitboss] phase 01: Project foundation"],
        cwd=workdir,
        env=env,
    )

    state_dir = play_dir
    state = {
        "run_id": "20260429T143022Z",
        "branch": "pitboss/run-20260429T143022Z",
        "original_branch": "main",
        "started_at": "2026-04-29T14:30:22Z",
        "started_phase": "01",
        "completed": ["01"],
        "attempts": {"02": 2},
        "token_usage": {
            "input": 12850,
            "output": 4210,
            "by_role": {
                "implementer": {"input": 9000, "output": 2910},
                "auditor": {"input": 2100, "output": 480},
                "fixer": {"input": 1750, "output": 820},
            },
        },
        "aborted": False,
    }
    (state_dir / "state.json").write_text(json.dumps(state, indent=2) + "\n")


# ---------------------------------------------------------------------------
# vhs orchestration
# ---------------------------------------------------------------------------


def write_tape(
    tape_path: Path,
    output_png: Path,
    command: str,
    *,
    width: int,
    height: int,
    pre_screenshot_sleep_ms: int = 2500,
    post_command_sleep_ms: int = 1500,
    cwd: str | None = None,
) -> None:
    """Render a vhs tape that types `command`, waits for it to settle, and
    screenshots once. Setup steps (cd, PATH export, clear) run inside a
    Hide block so the captured frame is just `> {command}` and its
    output."""
    setup = ""
    if cwd:
        setup += f'  Type "cd {cwd}"\n  Enter\n  Sleep 200ms\n'
    pitboss_bin_dir = str(PITBOSS_BIN.parent)
    setup += (
        f'  Type "export PATH={pitboss_bin_dir}:$PATH"\n  Enter\n  Sleep 200ms\n'
    )
    setup += '  Type "clear"\n  Enter\n  Sleep 200ms\n'
    tape = f"""Output "{output_png}"

Set Theme "{VHS_THEME}"
Set FontSize {VHS_FONT_SIZE}
Set Width {width}
Set Height {height}
Set Padding {VHS_PADDING}
Set Shell "bash"

Hide
{setup}  Type "{command}"
  Enter
  Sleep {post_command_sleep_ms}ms
Show
Sleep {pre_screenshot_sleep_ms}ms
Screenshot "{output_png}"
Sleep 300ms
"""
    tape_path.write_text(tape)


def run_vhs(tape: Path) -> None:
    log(f"vhs ← {tape.name}")
    run(["vhs", str(tape)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


# ---------------------------------------------------------------------------
# Interview scene
# ---------------------------------------------------------------------------

# A self-contained bash script that simulates `pitboss plan --interview`.
# Echoes pre-canned questions and answers with the correct ANSI palette so
# the screenshot looks like a real session. Called by the vhs tape in place
# of the real binary.
INTERVIEW_WRAPPER_TEMPLATE = """\
#!/usr/bin/env bash
set -e
PITBOSS_REAL="{pitboss_bin}"
if [[ "$1" == "plan" && "$2" == "--interview" ]]; then
  R="\\033[0m"; BC="\\033[1;36m"; M="\\033[35m"; BG="\\033[1;32m"; D="\\033[2m"; C="\\033[36m"
  printf "${{BC}}[pitboss]${{R}} ${{M}}generating design questions...${{R}}\\n" >&2
  sleep 0.3
  printf "${{BC}}[pitboss]${{R}} ${{BG}}interview:${{R}} 8 ${{D}}questions ready — press Enter to skip any question${{R}}\\n" >&2
  questions=(
    "What file changes should trigger a rebuild (all files, or specific extensions like .rs)?"
    "Should the watcher debounce rapid saves, and if so what delay?"
    "Does watch mode need its own per-run branch, or reuse the current branch?"
    "Should watch runs commit after each phase, or only after the full plan finishes?"
    "Which directories to exclude from watching (target/, .pitboss/, node_modules/)?"
    "Should the TUI be on by default in watch mode?"
    "How should watch mode handle a run that halts mid-plan?"
    "Is there a cap on watch-triggered runs, or does Ctrl-C stop it?"
  )
  answers=(
    ".rs and .toml files"
    "yes, 500ms debounce"
    "own branch with a pitboss/watch- prefix"
    "after each phase, same as a normal run"
    "target, .pitboss, node_modules"
    "yes — makes sense for a long-running session"
    "pause and wait, resume on the next file change"
    "Ctrl-C only, no cap"
  )
  total=${{#questions[@]}}
  for i in "${{!questions[@]}}"; do
    n=$((i + 1))
    printf "\\n[%s/%s] %s\\n" "$n" "$total" "${{questions[$i]}}"
    printf "> "
    sleep 0.05
    printf "%s\\n" "${{answers[$i]}}"
  done
  printf "\\n" >&2
  printf "${{BC}}[pitboss]${{R}} ${{BG}}interview complete${{R}} (8 answered)\\n" >&2
  printf "${{BC}}[pitboss]${{R}} ${{M}}dispatching planner${{R}} ${{C}}claude-opus-4-7${{R}} (attempt 1/2)\\n" >&2
  printf "${{BC}}[pitboss]${{R}} ${{D}}live log: .pitboss/logs/planner-attempt-1.log${{R}}\\n" >&2
  sleep 0.3
  printf "${{BG}}wrote${{R}} plan.md (1 attempt)\\n"
else
  exec "$PITBOSS_REAL" "$@"
fi
"""


def setup_interview_scene(workdir: Path) -> Path:
    """Write the interview wrapper script and return its directory."""
    wrapper_dir = workdir / "interview-bin"
    wrapper_dir.mkdir(parents=True, exist_ok=True)
    wrapper = wrapper_dir / "pitboss"
    wrapper.write_text(
        INTERVIEW_WRAPPER_TEMPLATE.format(pitboss_bin=str(PITBOSS_BIN))
    )
    wrapper.chmod(0o755)
    return wrapper_dir


def write_interview_tape(tape_path: Path, output_png: Path, wrapper_dir: Path) -> None:
    """Write a vhs tape for the interview scene.

    The command contains double quotes so we need single-quoted Type args;
    the wrapper intercepts `pitboss plan --interview ...` and simulates output.
    """
    pitboss_bin_dir = str(PITBOSS_BIN.parent)
    setup = (
        f'  Type "export PATH={wrapper_dir}:{pitboss_bin_dir}:$PATH"\n  Enter\n  Sleep 200ms\n'
        '  Type "clear"\n  Enter\n  Sleep 200ms\n'
    )
    tape = f"""Output "{output_png}"

Set Theme "{VHS_THEME}"
Set FontSize {VHS_FONT_SIZE}
Set Width 1200
Set Height 680
Set Padding {VHS_PADDING}
Set Shell "bash"

Hide
{setup}  Type 'pitboss plan --interview "add a --watch mode to the build CLI"'
  Enter
  Sleep 4500ms
Show
Sleep 2000ms
Screenshot "{output_png}"
Sleep 300ms
"""
    tape_path.write_text(tape)


# ---------------------------------------------------------------------------
# Scene driver
# ---------------------------------------------------------------------------


def ensure_pitboss_built() -> None:
    if PITBOSS_BIN.exists():
        return
    log("building pitboss release binary")
    run(["cargo", "build", "--release", "--quiet"], cwd=ROOT)


def capture_all() -> None:
    ensure_pitboss_built()
    ASSETS.mkdir(exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="pitboss-screenshots-") as tmp:
        tmp = Path(tmp)
        demo_dir = tmp / "demo"
        status_dir = tmp / "status-workspace"
        interview_dir = tmp / "interview"
        captures = tmp / "captures"
        captures.mkdir()

        demo_bin = build_demo_binary(demo_dir)
        setup_status_workspace(status_dir)
        interview_wrapper_dir = setup_interview_scene(interview_dir)

        scenes: list[tuple[str, str, int, int]] = [
            # name           command                w     h
            ("pitboss-tui",    f"{demo_bin} tui",   1500, 480),
            ("pitboss-halt",   f"{demo_bin} halt",  1500, 460),
            ("pitboss-grind",  f"{demo_bin} grind", 1500, 520),
            ("pitboss-status", "pitboss status",     880, 360),
        ]

        for name, command, w, h in scenes:
            tape = captures / f"{name}.tape"
            png = captures / f"{name}.png"
            cwd = str(status_dir) if name == "pitboss-status" else None
            write_tape(tape, png, command, width=w, height=h, cwd=cwd)
            run_vhs(tape)
            frame_png(png, ASSETS / f"{name}.png")

        # Interview scene uses a custom tape (quoted command, different setup).
        interview_tape = captures / "pitboss-interview.tape"
        interview_png = captures / "pitboss-interview.png"
        write_interview_tape(interview_tape, interview_png, interview_wrapper_dir)
        run_vhs(interview_tape)
        frame_png(interview_png, ASSETS / "pitboss-interview.png")

    log("done")


if __name__ == "__main__":
    capture_all()
