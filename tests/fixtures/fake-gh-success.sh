#!/bin/sh
# Fake `gh` binary used by ShellGit::open_pr unit tests.
# Writes a record of the invocation to `.gh-fake-log` *inside the cwd* so the
# call is naturally scoped to the per-test workspace tempdir. Then prints a
# representative `gh pr create` success line on stdout.
set -eu

{
    echo "argv:"
    for a in "$@"; do
        echo "  $a"
    done
    echo "cwd: $(pwd)"
} >".gh-fake-log"

# `gh pr create` prints any preamble first then the URL on its own line.
echo "Creating pull request for foreman/run-x into main"
echo "https://github.com/example/repo/pull/42"
exit 0
