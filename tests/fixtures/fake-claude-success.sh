#!/bin/sh
# Fake `claude` binary used by ClaudeCodeAgent unit tests.
# Emits a representative subset of stream-json events for a successful run.
set -eu

cat <<'JSON'
{"type":"system","subtype":"init","cwd":"/tmp","session_id":"fake-1","model":"claude-haiku-test"}
{"type":"assistant","message":{"model":"claude-haiku-test","content":[{"type":"thinking","thinking":"reasoning..."},{"type":"text","text":"Hello from Claude"}],"usage":{"input_tokens":10,"output_tokens":5}},"session_id":"fake-1"}
{"type":"assistant","message":{"model":"claude-haiku-test","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"cmd":"ls"}}]},"session_id":"fake-1"}
{"type":"assistant","message":{"model":"claude-haiku-test","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"path":"foo"}}]},"session_id":"fake-1"}
{"type":"result","subtype":"success","is_error":false,"duration_ms":42,"num_turns":2,"result":"done","stop_reason":"end_turn","session_id":"fake-1","usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":5,"output_tokens":51}}
JSON
exit 0
