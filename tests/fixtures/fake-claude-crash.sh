#!/bin/sh
# Emits no `result` event but exits non-zero with a stderr message, simulating
# an early-failure case (e.g. authentication missing).
set -eu

echo "Error: authentication required" 1>&2
exit 1
