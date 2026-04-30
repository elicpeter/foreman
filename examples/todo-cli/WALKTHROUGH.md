# Walkthrough — todo-cli

This is an end-to-end run of the `todo-cli` example. Follow it the first time you use foreman to get a feel for the loop without surprises.

## 1. Set up an empty workspace

```sh
mkdir scratch-todo && cd scratch-todo
git init
foreman init
```

`foreman init` writes `plan.md`, `deferred.md`, `foreman.toml`, the `.foreman/` directory, and updates `.gitignore`. Re-running `init` is a no-op.

## 2. Drop in the example plan

Replace the seed `plan.md` and `foreman.toml` with this example's:

```sh
cp ../foreman/examples/todo-cli/plan.md plan.md
cp ../foreman/examples/todo-cli/foreman.toml foreman.toml
```

(Adjust the paths to wherever you cloned foreman.)

## 3. Sanity check with `--dry-run`

```sh
foreman run --dry-run
```

What this exercises with no token spend:

- Parses `plan.md` and confirms `current_phase: "01"` resolves.
- Parses `foreman.toml`.
- Creates the per-run branch (`foreman/run-<utc>`).
- Walks each phase, dispatches the no-op agent, attempts a (no-op) commit,
  emits the same `Event` stream the real run will.
- Skips test execution because the no-op agent doesn't change anything.

If anything is wrong with the plan or config, you'll see it here. The dry run leaves a clean state on the per-run branch so the real run starts fresh.

## 4. Run for real

```sh
foreman run
```

Watch the streamed output. Each phase will:

1. Print `[foreman] phase 01 (Cargo skeleton & CLI parsing) — attempt 1`.
2. Stream the agent's stdout / tool-use lines as `[agent] ...`.
3. Print `[foreman] running tests` and the result.
4. (When `audit.enabled`) Print `[foreman] phase 01 auditor (total dispatch 2)`.
5. Print `[foreman] phase 01 committed: <short-sha>`.

If a phase halts, foreman prints a clear reason and exits non-zero. Run `foreman status` to see where it stopped, fix the underlying issue, then `foreman resume`.

## 5. Inspect the result

```sh
foreman status              # phase + token + cost summary
git log --oneline           # one commit per phase, all on the per-run branch
cat deferred.md             # anything the auditor marked as follow-up work
ls .foreman/logs/           # per-attempt agent + test logs for post-mortem
```

## 6. Open a PR (optional)

```sh
foreman run --pr            # or set git.create_pr = true in foreman.toml
```

Foreman shells out to `gh pr create` with a body listing the completed phases plus any unfinished deferred items.

## 7. Clean up

If you want to throw the run away:

```sh
foreman abort --checkout-original   # back to the branch you were on at run start
git branch -D foreman/run-<utc>     # delete the per-run branch
rm .foreman/state.json              # wipe the state breadcrumb
```

`plan.md` and `deferred.md` are preserved — they're never deleted by foreman.
