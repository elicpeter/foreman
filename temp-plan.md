---
current_phase: "01"
---

This plan adds a periodic deferred-sweep step that fires between regular phases when `## Deferred items` in `deferred.md` has piled up. Cross-cutting items (test flakes, doc fixes, refactors that don't unblock any current phase) currently sit forever because the implementer prompt only sweeps items relevant to the active phase. The sweep step reuses the existing per-phase pipeline (implementer → tests → fixer-on-fail → auditor → commit) with a different prompt focused on `deferred.md`. `## Deferred phases` (H3 blocks) are out of scope — they behave more like design notes and need a separate promotion path. Each phase ends with `cargo test` and `cargo clippy --all-targets -- -D warnings` green; clippy and rustfmt match the existing crate.

# Phase 01: Sweep prompt, config, and trigger

**Scope.** Stand up the typed pieces the runner will consume in phase 02 — a new prompt template, a `[sweep]` config block, and a pure trigger function — without changing any runtime behavior. A fresh checkout still builds and `pitboss play` still runs the existing pipeline unmodified. The trigger range (5..=8 items) is configurable but defaulted; the agent is *not* told a numeric cap on how many items to resolve per sweep, because clamping a 7-easy-fixes sweep to 5 just defers the rest to another dispatch.

**Deliverables.**
- New template `src/prompts/templates/sweep.txt` modeled on `implementer.txt`. Tells the agent: its job is `deferred.md`, not a `plan.md` phase; `## Deferred items` only (ignore `## Deferred phases` entirely); fix as many items as it can reasonably finish in one session and mark each `- [x]` only if actually done; do not check items it didn't finish; `plan.md` and `.pitboss/` remain off-limits under the same byte-snapshot enforcement as a normal phase. The template includes the same "Order of operations" framing as `implementer.txt` so the existing implementer instruction to sweep phase-relevant items first is unchanged for the regular path.
- New `prompts::sweep(plan: &Plan, deferred: &DeferredDoc, after_phase: &PhaseId) -> String` renderer in `src/prompts/mod.rs`, wired with placeholders for `phase_id` (the phase the sweep follows), `deferred`, and the static guidance text.
- `src/config/mod.rs`: new `SweepConfig` struct serde-defaulted with `enabled: bool` (default `true`), `trigger_min_items: u32` (default `5`), `trigger_max_items: u32` (default `8`), `max_consecutive: u32` (default `1`). Add `sweep: SweepConfig` to `Config`. Validation: `trigger_min_items >= 1`, `trigger_min_items <= trigger_max_items`, `max_consecutive >= 1`.
- New module `src/runner/sweep.rs` exposing `should_run_deferred_sweep(deferred: &DeferredDoc, sweep_cfg: &SweepConfig, consecutive_sweeps: u32) -> bool`. The fairness rule: trigger when `enabled && unchecked_items >= trigger_min_items && consecutive_sweeps < max_consecutive`. The `trigger_max_items` value is *advisory* — it documents an expected upper bound rather than gating behavior, since the sweep agent picks how many items to address. Tests cover the on/off threshold boundary, the `enabled = false` short-circuit, and the `max_consecutive` clamp.
- `insta` snapshot test for the rendered sweep prompt with a representative `DeferredDoc` fixture (5 unchecked items, 1 already-checked item that should be filtered out before the agent sees it, no `## Deferred phases`).
- Unit tests on `prompts::sweep` for: placeholder substitution; that already-checked items are stripped from the rendered prompt (the agent only sees pending work); that the rendered text is below `TEMPLATE_STATIC_BUDGET` for an empty deferred doc.

**Acceptance.**
- `cargo build` succeeds on a fresh `cargo clean`.
- `cargo test prompts::sweep`, `cargo test config::sweep`, and `cargo test runner::sweep` all pass.
- `cargo clippy --all-targets -- -D warnings` is clean.
- `cargo insta test` snapshot is created and committed.
- `pitboss play --help` is byte-identical to before this phase (no new flags yet).
- A `pitboss.toml` with no `[sweep]` section parses and applies the documented defaults.

# Phase 02: Runner integration

**Scope.** Wire phase 01's pieces into `Runner::run_phase` so a sweep step fires between regular phases when the trigger is satisfied. The sweep step reuses `dispatch_and_validate`, `run_tests`, `run_fixer_loop`, and `run_auditor_pass` unchanged — those already enforce the `plan.md` snapshot and `deferred.md` parsability invariants. State that survives a halt+resume goes on `RunState`. No CLI flags yet (those land in phase 03); behavior is governed entirely by `pitboss.toml`. The sweep step does *not* introduce a synthetic phase id — `state.attempts` keys remain real `PhaseId` values, and sweep log files are distinguished by a `sweep-after-<phase-id>-` filename prefix rather than by impersonating a phase.

**Deliverables.**
- New fields on `RunState` (`src/state/mod.rs`): `pending_sweep: bool` (default `false`) and `consecutive_sweeps: u32` (default `0`), both serde-defaulted so existing `state.json` files load.
- `Runner::run_phase_inner` flow change: at the end of a regular phase, after `self.deferred.sweep()`, the `state.completed.push`, and the existing `current_phase` advancement (`src/runner/mod.rs:517-530`), call `should_run_deferred_sweep` on the post-sweep deferred doc. If it returns `true`, set `state.pending_sweep = true`. `current_phase` still advances normally; the next call to `run_phase` notices `pending_sweep` and dispatches the sweep step before the new `current_phase` runs.
- `Runner::run_phase` dispatch fork: at the top of the function, if `state.pending_sweep` is `true`, call `run_sweep_step(after)` where `after = state.completed.last().clone()` and return its `PhaseResult` directly. Otherwise dispatch the regular `run_phase_inner` path as today. The sweep path persists `state.json` on exit the same way the regular path does.
- New private method `Runner::run_sweep_step(after: PhaseId) -> Result<PhaseResult>`. Mirrors `run_phase_inner` but with: the sweep prompt instead of the implementer prompt; the same test → fixer-loop → auditor → commit chain; log filenames built via a new `sweep_log_path(after, role, attempt)` helper that produces `.pitboss/logs/sweep-after-{after}-{role}-{attempt}.log` so they don't collide with the next regular phase's logs; commit message `git::commit_message_sweep(&after, resolved_count)` producing e.g. `[pitboss] sweep after phase 01: 3 deferred items resolved` (or `... no items resolved`). On successful commit: clear `state.pending_sweep`, increment `state.consecutive_sweeps`. The sweep step does not push anything onto `state.completed` (that field tracks plan progress).
- A regular phase commit resets `state.consecutive_sweeps = 0` so the `max_consecutive` clamp re-arms after every forward step.
- A local `sweep_attempt_counter: u32` inside `run_sweep_step` drives the fixer-loop attempt numbering and the log filenames; sweep dispatches do not bump `state.attempts` (which keys real `PhaseId`s only).
- Empty-sweep handling: if the sweep agent doesn't check any items off (resolved_count == 0) and the diff is empty, the runner skips the commit (mirrors the existing "phase produced no code changes" branch at `src/runner/mod.rs:512-514`) and logs a warning. `state.consecutive_sweeps` still increments either way so an unproductive sweep can't loop into another sweep next iteration.
- New events on `runner::Event`: `SweepStarted { after: PhaseId, items_pending: usize, attempt: u32 }`, `SweepCompleted { after: PhaseId, resolved: usize, commit: Option<CommitId> }`, `SweepHalted { after: PhaseId, reason: HaltReason }`. Emit `SweepStarted` on entry to `run_sweep_step`, `SweepCompleted` on successful exit, and `SweepHalted` (in addition to whatever the inner halt produced) on failure. The TUI in phase 04 consumes these.
- Halt behavior inside the sweep: if `dispatch_and_validate` halts (plan tampered, deferred invalid, agent failure), or the fixer loop halts (tests failed), or the auditor halts, the run halts and `state.pending_sweep` stays `true` so a `pitboss rebuy` (or next `pitboss play`) retries the sweep rather than skipping past it. `current_phase` continues to point at the next regular phase, so once the sweep clears the run resumes in the right place.
- Integration test (new `tests/sweep_smoke.rs`) using the existing `MockAgent` + `MockGit` test doubles: a plan with two phases; the first phase commits and leaves 6 unchecked deferred items; the runner fires a sweep step before phase 02; the sweep agent checks 4 items off; assert that the sequence of `Event`s contains exactly one `SweepStarted` between `Event::PhaseCommitted { phase 01 }` and `Event::PhaseStarted { phase 02 }`, that `state.pending_sweep` is `false` post-sweep, that `state.completed` contains only the real phase ids, and that the post-sweep `deferred.md` has 2 unchecked items.
- A second integration test asserts that with `[sweep] enabled = false`, no sweep fires even when 8 items pile up.
- A third asserts `max_consecutive = 1` (the default) prevents back-to-back sweeps even if the first sweep leaves the item count above the threshold.

**Acceptance.**
- `cargo test --test sweep_smoke` passes.
- All existing tests still pass: `cargo test --workspace`.
- `cargo clippy --all-targets -- -D warnings` is clean.
- A test asserts a halt during the sweep leaves `state.pending_sweep = true` and that a follow-up `Runner::run_phase` call retries the sweep (does not advance `current_phase`).
- `pitboss play` end-to-end with the live agent disabled (skip-tests + mock dispatch) produces a sweep commit with the documented message format when a phase leaves ≥5 items.

# Phase 03: CLI flags and `pitboss status` surface

**Scope.** Surface sweep state on the CLI so an operator can see what's pending and override the default trigger. Adds two flags on `pitboss play` and extends `pitboss status` to report the sweep counters from `RunState`. No behavior changes beyond what the flags request.

**Deliverables.**
- `pitboss play --no-sweep`: clears `state.pending_sweep` at the top of the run and forces `should_run_deferred_sweep` to return `false` for the duration of the invocation (does not write to `pitboss.toml`). Implemented as a runner-level override flag passed through `Runner::new`.
- `pitboss play --sweep`: forces `state.pending_sweep = true` before the next phase even if the trigger threshold isn't met; useful for "I just edited deferred.md by hand, run a sweep now."
- The two flags are mutually exclusive at the clap level.
- `pitboss status` (`src/cli/status.rs`) gains a "Sweep" line in the existing layout: `Sweep: pending=<bool> consecutive=<n> deferred_items=<n>` where `deferred_items` is the count of unchecked items in the on-disk `deferred.md`. Add this near the existing budget/attempts surface; reuse the formatting helpers already present.
- Update the `pitboss play --help` text to document the new flags and reference the `[sweep]` config block.
- Tests: `assert_cmd` integration test for `pitboss play --no-sweep --sweep` exiting non-zero with a clap error; a `pitboss status` snapshot test that includes the new line in three states (no pending sweep, pending sweep, just-finished sweep with `consecutive=1`).

**Acceptance.**
- `pitboss play --help` shows `--sweep` and `--no-sweep`.
- `pitboss status` in a fresh repo prints `Sweep: pending=false consecutive=0 deferred_items=0`.
- `cargo test --test cli_status` snapshot covers the three sweep states.
- `pitboss play --no-sweep` against a repo with 10 deferred items skips the sweep that would otherwise fire (verified by an integration test that asserts no `SweepStarted` event is emitted).
- `pitboss play --sweep` against a repo with 2 deferred items still fires a sweep before the next phase.

# Phase 04: TUI audit and sweep rendering

**Scope.** Make sure the sweep step renders cleanly in the existing ratatui dashboard (`src/tui/app.rs`) and that the new events thread through every code path the TUI cares about. This phase is half audit, half implementation — the audit catches anywhere a `SweepStarted` / `SweepCompleted` / `SweepHalted` event lands without a render path, and the implementation adds the missing pieces. No changes to runner or CLI behavior.

**Deliverables.**
- Audit pass on `src/tui/app.rs::handle_event` (`src/tui/app.rs:241-323`): every other `Event::*` variant has a matching arm; add `SweepStarted`, `SweepCompleted`, and `SweepHalted` arms and confirm no variant is silently dropped. The match must remain exhaustive — drop the `_ =>` fallback if one was added during phase 02. A test exercises the dispatch by sending one of every event variant in sequence and asserting the resulting `App` state for each.
- Header rendering: while a sweep is in flight, the phase header row reads `Sweep after phase 01 — attempt 1` (mirroring the existing `Phase 01: <title> — attempt 1` format from `Event::PhaseStarted`). On `SweepCompleted`, the header transitions back to whatever the next regular `Event::PhaseStarted` reports. The session-stats panel from the deferred phase-04 entry continues to tick during a sweep — verify by hand that elapsed time and dispatch counts behave naturally across the sweep boundary.
- Output pane: agent output during a sweep flows into the same scrollable pane as a regular phase. The wrap-aware scrolling fix from the deferred phase-08 entry must not regress; the existing `render_keeps_latest_line_visible_when_earlier_lines_wrap` test stays green.
- Sweep summary line on completion: when `SweepCompleted` fires, append a one-line entry to the same activity log the TUI uses for `PhaseCommitted` — `sweep after 01: 3 items resolved` (or `0 items resolved`). Match the visual style and color of the existing commit row.
- New `insta` snapshot fixture `pitboss__tui__app__tests__sweep_in_flight.snap` capturing the dashboard mid-sweep (after `SweepStarted`, before `SweepCompleted`) and a second `pitboss__tui__app__tests__sweep_completed.snap` capturing the post-sweep frame with the resolved-items line in the activity log.
- Defensive check: if a `SweepStarted` arrives without a preceding `PhaseCommitted`, or a `SweepCompleted` arrives without a `SweepStarted`, log a debug-level message and render the dashboard from whatever state we have — do not panic. Tests cover both out-of-order cases.
- Manual checklist appended to the TUI module rustdoc: how to drive a sweep frame by hand for visual verification (the existing user stance is that TUI behavior is verified manually, not in CI).

**Acceptance.**
- `cargo test tui` passes including the two new snapshot tests.
- `cargo insta test` snapshots are reviewed and committed.
- An exhaustiveness test (a `match` over `Event` in test code) compiles, proving no variant was added without a TUI handler.
- Manual smoke: `pitboss play --tui` against a repo set up to trigger a sweep produces a frame matching the snapshot (verified by hand, recorded in the PR description).
- `cargo clippy --all-targets -- -D warnings` is clean.
- `cargo test --workspace` passes — all prior phases' tests still green.
