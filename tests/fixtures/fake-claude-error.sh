#!/bin/sh
# Emits a stream-json `result` with is_error=true and a non-zero exit so
# ClaudeCodeAgent can map it to StopReason::Error.
set -eu

cat <<'JSON'
{"type":"system","subtype":"init","cwd":"/tmp","session_id":"fake-2"}
{"type":"result","subtype":"error","is_error":true,"result":"rate limit exceeded — try again later","session_id":"fake-2","usage":{"input_tokens":0,"output_tokens":0}}
JSON
exit 2
