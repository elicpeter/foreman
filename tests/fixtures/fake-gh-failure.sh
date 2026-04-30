#!/bin/sh
# Fake `gh` binary that simulates `gh pr create` failing — e.g., no remote
# configured. Used to verify ShellGit::open_pr surfaces the underlying stderr.
set -eu
echo "could not determine the base repository, please run with --repo" >&2
exit 1
