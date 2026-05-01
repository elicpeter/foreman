## Deferred items

- [ ] Wire grind `verify: true` into the `src/runner/` fixer cycle so a failing test run dispatches the fixer agent up to `retries.fixer_max_attempts` times before recording the session as `SessionStatus::Error`. Phase 07 ships a one-shot test invocation only (pass = `Ok`, fail = `Error`); the spec calls out reuse of the existing fixer cycle.
- [ ] Resume reconciliation between `state.json` and `sessions.jsonl` currently *refuses* on mismatch (`reconcile_state_with_log` → `ResumeError::StateOutOfSync`). Option (a) from the original deferred note. Option (c) — fully reconstruct scheduler / budget state from the JSONL log on resume so a single dropped `state.json` write doesn't take down the run — would be the more robust long-term fix, but it requires re-implementing the scheduler's per-prompt run accounting on top of the JSONL replay and verifying parity with the live tracker. Tracked here in case a future failure makes the refusal too aggressive.
- [ ] `pitboss grind --dry-run --resume` is currently rejected up front (phase 13 sweep). The proper fix is to load the persisted scheduler/budget snapshot when both flags are set and pass them through `DryRunInputs` so the preview reflects the resume start point. Until then the combination errors out.
- [ ] Phase 12's implementer left a `temp-plan.md` at the repo root sketching four future phases (14 sweep prompt/config/trigger, 15 runner integration, 16 CLI flags + status surface, 17 TUI audit and sweep rendering) for a periodic deferred-sweep step that fires between regular phases when `## Deferred items` piles up. The auditor deleted the file as scope creep before commit. If the user wants that work, the sketch needs to be adopted into `plan.md` deliberately rather than smuggled in as a side artifact of phase 12.

## Deferred phases

### From phase 04: TUI session-stats panel and `Event::UsageUpdated`

Phase 04 was scoped to "load and validate" plan files and the `[grind]` config block. The diff also adds a full session-stats panel that is independent of grind plans: a new `Event::UsageUpdated(TokenUsage)` variant emitted from `Runner::update_token_usage`, a `UsageView { role_models, pricing }` pricing snapshot built in `tui::run`, ~250 lines of new rendering in `tui::app` (panel layout, elapsed/cost/token/dispatch lines, per-role rows, `format_elapsed` / `format_tokens` / `format_usd` / `role_short` / `role_color` helpers), a height heuristic that drops the panel on terminals shorter than `STATS_HEIGHT + 4`, and three rebaselined snapshots under `src/tui/snapshots/`.

This is a coherent feature that wasn't requested and isn't covered by phase 04's acceptance criteria, so it slipped past phase-scoped review. Decisions worth a focused look before this lands: the role/model precedence (backend override beats `[models]` per-role) duplicates `build_agent_display`, the runner emits the event with `let _ = events_tx.send(...)` and has no test exercising the emission, the pricing math goes through `ModelPricing::cost_usd` for both per-role and total (potential drift from any future input/output split), and the elapsed line ticks on each render frame from `Utc::now()` rather than off a clock event.

User decision: accept in-place, move to its own phase before merging, or revert. Files touched outside phase scope: `src/runner/mod.rs` (Event enum + emission), `src/tui/app.rs`, `src/tui/mod.rs`, `src/tui/snapshots/pitboss__tui__app__tests__{initial,mid_run,halted}_*.snap`.

USER: PLEASE KEEP TUI session-stats panel DO NOT REVERT, THIS IS A CHANGE I MADE WHILE YOU WERE WORKING!

### From phase 08: TUI output-pane wrap-aware scrolling

Phase 08 was scoped to budgets and the exit-code policy. The diff also touches `src/tui/app.rs::render_output`: it now consults `Paragraph::line_count` to compute a vertical scroll offset so wrapped output lines no longer push the most recent rows off the bottom edge. The change requires adding the `unstable-rendered-line-info` ratatui feature in `Cargo.toml`, and lands a new `render_keeps_latest_line_visible_when_earlier_lines_wrap` test (~30 lines) under `src/tui/app.rs`'s test module.

This is a real bug fix but unrelated to phase 08's scope. Concerns worth a focused look: the ratatui feature is `unstable-` (semver-exempt), `line_count` is called every frame on the full output buffer, and the assumption that `Paragraph` reports `inner_height + 2` for the borders is implementation-defined and currently encoded as a `saturating_sub(2)` in the new code.

User decision: accept in-place, move to its own phase, or revert. Files touched outside phase scope: `Cargo.toml` (ratatui feature flag), `src/tui/app.rs` (render_output rewrite + new test).
