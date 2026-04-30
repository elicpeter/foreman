# foreman

A Rust CLI that orchestrates coding agents (Claude Code first; pluggable) through a phased implementation plan, with deferred-work tracking, automatic test/commit/audit loops, and a ratatui dashboard.

Foreman keeps the *plan* in `plan.md` (read-only to agents), the *unfinished work* in `deferred.md` (the only file agents may write outside of their code changes), and the *runner state* in `.foreman/state.json` (managed by foreman itself). Each phase becomes its own commit on a per-run branch, optionally rolled into a pull request when the run finishes.

## Install

Foreman is a single Rust crate. With a recent stable toolchain installed:

```sh
git clone <this repo>
cd foreman
cargo install --path .
```

`foreman` will then be on your `$PATH`. To run the production agent you also need:

- The `claude` CLI from Anthropic, on `$PATH`.
- The `git` CLI, on `$PATH`.
- Optional: the `gh` CLI, only if you want `--pr` to open pull requests for you.

## Quickstart

```sh
mkdir my-project && cd my-project
git init
foreman init                            # scaffold plan.md, deferred.md, foreman.toml, .foreman/
$EDITOR plan.md                         # describe what to build, phase by phase
foreman run --dry-run                   # exercise the runner end-to-end without spending tokens
foreman run                             # let the agent loop drive the plan to completion
foreman status                          # check progress at any time
```

Other entry points worth knowing:

- `foreman plan "build a CLI todo app in Rust"` invokes the planner agent to write `plan.md` for you.
- `foreman run --tui` swaps the plain stderr logger for a live ratatui dashboard.
- `foreman run --pr` (or `git.create_pr = true`) opens a pull request via `gh pr create` after the run finishes.
- `foreman resume` picks up where a halted run left off.
- `foreman abort --checkout-original` marks the active run aborted and switches HEAD back to the branch you were on before `foreman run`.

## How the run loop works

For each phase in `plan.md`:

1. Snapshot `plan.md` and `deferred.md` (SHA-256).
2. Dispatch the **implementer** agent with the active phase, the unfinished deferred work, and the user prompt template.
3. If the agent modified `plan.md`, restore the snapshot and halt.
4. Re-parse `deferred.md`; on parse failure, restore the snapshot and halt.
5. Run the project test suite (auto-detected — see below). If the suite fails, dispatch the **fixer** agent up to `retries.fixer_max_attempts` times.
6. Stage the agent's diff and dispatch the **auditor** agent (when `audit.enabled = true`). The auditor inlines small fixes and records anything larger in `deferred.md`. Tests run again post-audit.
7. Commit the staged diff to the per-run branch as `[foreman] phase <id>: <title>`. `plan.md`, `deferred.md`, and `.foreman/` are excluded from the commit by design.
8. Sweep checked-off deferred items, advance `current_phase` in `plan.md`, persist `state.json`, move to the next phase.

Bounded retries everywhere — foreman never loops indefinitely. When a budget is exhausted the runner halts with a clear reason and `foreman resume` will pick up from the same phase.

## Configuration

Foreman reads `foreman.toml` from the workspace root. Every section is optional; missing keys fall back to defaults. Unknown keys are accepted with a warning so a config written by a newer foreman still loads.

```toml
# Per-role model selection. Strings are passed verbatim to the agent (e.g.,
# `claude --model <id>`), so they must match valid model identifiers.
[models]
planner     = "claude-opus-4-7"
implementer = "claude-opus-4-7"
auditor     = "claude-opus-4-7"
fixer       = "claude-opus-4-7"

# Bounded retries — no infinite loops.
[retries]
fixer_max_attempts = 2   # 0 disables the fixer entirely
max_phase_attempts = 3

# Auditor pass: ON by default. Disable to commit straight after tests pass.
[audit]
enabled              = true
small_fix_line_limit = 30   # line threshold separating "inline" from "defer"

# Per-run branch + optional PR.
[git]
branch_prefix = "foreman/run-"   # full branch is <prefix><utc_timestamp>
create_pr     = false            # true is equivalent to `foreman run --pr`

# Test runner override. Leave commented to auto-detect.
# [tests]
# command = "cargo test --workspace"

# Cost guard. Either limit being set activates budget enforcement: the
# runner halts before the next dispatch that would exceed the cap.
[budgets]
# max_total_tokens = 1_000_000
# max_total_usd    = 5.00

# Override or extend the default per-model price points. Defaults cover
# claude-opus-4-7, claude-sonnet-4-6, and claude-haiku-4-5.
# [budgets.pricing.claude-opus-4-7]
# input_per_million_usd  = 15.0
# output_per_million_usd = 75.0
```

### Per-role model recommendations

The defaults set every role to the latest Opus, which is the safe choice for getting started. If you want to spend less, the recommended split is:

| Role          | Recommendation        | Why                                                                                          |
| ------------- | --------------------- | -------------------------------------------------------------------------------------------- |
| `planner`     | `claude-opus-4-7`     | Plan quality compounds; one expensive call up front saves dozens of cheap-but-wrong phases.  |
| `implementer` | `claude-opus-4-7`     | The bulk of token spend, and the role most sensitive to capability.                          |
| `auditor`     | `claude-sonnet-4-6`   | Smaller, focused on diff review and short-form deferred items.                               |
| `fixer`       | `claude-sonnet-4-6`   | Test-failure fix-ups are short and specific; sonnet is usually enough.                       |

Configure pricing for any model you reference in `[models]` so `foreman status` and the USD budget check produce accurate numbers.

### Test runner detection

The runner probes the workspace in this order and uses the first match:

1. `Cargo.toml` → `cargo test`
2. `package.json` (with a non-empty `scripts.test`) → `pnpm test` / `yarn test` / `npm test` (chosen by the lock file present)
3. `pyproject.toml` or `setup.py` → `pytest`
4. `go.mod` → `go test ./...`

Unrecognized layouts skip the test step and the runner advances on a passing implementer dispatch alone. Override detection by setting `[tests] command = "..."` — the value is whitespace-split into program + args, so shell features (pipes, env-var assignments) need an explicit `sh -c "..."` wrapper.

## Verbose output

`foreman -v <command>` lowers the log filter to `debug`; `-vv` lowers it to `trace`. `FOREMAN_LOG` and `RUST_LOG` still take precedence when set, so per-process tuning ("just this run, give me trace on a single module") works without touching the flag.

## `--dry-run`

`foreman run --dry-run` swaps the configured agent for a deterministic no-op and skips test execution. Use it to sanity-check that:

- `plan.md` parses and `current_phase` resolves to a real heading.
- `foreman.toml` parses cleanly with the keys you expect.
- The per-run branch is created and checked out without touching `main`.
- The event stream and TUI / logger render correctly.

The dry-run advances through every phase, attempts the per-phase commit (which no-ops because nothing was staged), and finishes — without any model spend. The post-run PR step is suppressed in dry-run mode regardless of `--pr` / `git.create_pr` so a no-op branch never accidentally opens a PR.

## Workspace layout

After `foreman init`:

```
your-project/
├── plan.md              # source of truth for the work
├── deferred.md          # agent-writable, swept between phases
├── foreman.toml         # config
├── .gitignore           # foreman appends `.foreman/` if missing
└── .foreman/
    ├── state.json       # runner-managed; ignored by git
    ├── snapshots/       # pre-agent snapshots of plan.md & deferred.md
    └── logs/            # per-phase, per-attempt agent + test logs
```

`init` is idempotent — re-running it on a populated workspace skips every existing file and prints a per-file summary.

## Troubleshooting

**`run halted at phase NN: plan.md was modified by the agent`**
The agent wrote to `plan.md`. Foreman restored the file from snapshot — your plan is intact. Re-read the phase prompt: it likely needs sharper guard rails about not editing planning artifacts. Re-running with `foreman resume` will retry the same phase.

**`run halted at phase NN: deferred.md is invalid: ...`**
The agent wrote a malformed `deferred.md`. Foreman restored from snapshot. The error message includes a 1-based line number; check the agent's log under `.foreman/logs/phase-<id>-implementer-<n>.log` to see what it tried to write.

**`run halted at phase NN: tests failed: ...`**
The implementer + fixer dispatches together couldn't get the suite green within the configured budget. The summary includes the trailing lines of the test log; the full transcript is at `.foreman/logs/phase-<id>-tests-<n>.log`. Either bump `retries.fixer_max_attempts`, fix the failing test by hand, or rework the phase.

**`run halted at phase NN: budget exceeded: ...`**
Either `max_total_tokens` or `max_total_usd` was met before the next dispatch. `foreman status` shows the running totals and per-role breakdown; raise the cap (or clear it) and `foreman resume`.

**`state.json marks run X as aborted; remove .foreman/state.json to start over`**
A previous run was aborted with `foreman abort`. Foreman keeps the state file as a breadcrumb. Delete `.foreman/state.json` to start fresh; everything else (plan, deferred, branch, commits) is preserved.

**`no run to resume: .foreman/state.json is empty; use foreman run to start a fresh run`**
You called `foreman resume` on a workspace where no run has started. Use `foreman run` instead.

**`creating per-run branch ... (workspace must already be a git repo)`**
The workspace isn't a git repo. `git init` it (foreman doesn't, deliberately).

**`foreman --version`**
Prints the foreman crate version. Useful when filing issues.

## Examples

The [`examples/`](examples) directory contains at least one walkthrough plan you can copy into a fresh workspace and run end-to-end.

## Module layout (for contributors)

```
src/
├── main.rs          — CLI entry, wires the tracing subscriber
├── cli/             — clap commands (init, plan, run, status, resume, abort)
├── plan/            — Plan/Phase types, parser, snapshot
├── deferred/        — DeferredDoc/items/phases, parser
├── state/           — RunState, atomic IO
├── config/          — foreman.toml schema + loader
├── agent/           — Agent trait, request/outcome, subprocess utils
│   ├── claude_code.rs
│   └── dry_run.rs
├── git/             — Git trait, ShellGit, MockGit, PR helpers
├── tests/           — project test runner detection (despite the name —
│                      this is NOT the integration test directory)
├── prompts/         — system prompt templates
├── runner/          — orchestration loop + events
└── tui/             — ratatui dashboard
tests/               — integration tests
```

See `plan.md` for the phase-by-phase design log.

## License

MIT OR Apache-2.0.
