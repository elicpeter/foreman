#!/bin/sh
# Emits a non-JSON stdout line followed by a normal result, exercising the
# parser fallback path in ClaudeCodeAgent.
set -eu

printf 'not-json output line\n'
cat <<'JSON'
{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"fake-3","usage":{"input_tokens":1,"output_tokens":1}}
JSON
exit 0
