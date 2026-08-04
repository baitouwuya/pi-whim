#!/bin/bash
# Additional admission policy for agent_spawning hooks matching `spawn_agent`.
#
# Configure as a Gate.  The supervisor's max depth, max spawn count, permission
# ceiling, tool policy, and unique-name checks remain the security boundary.
# This script only adds organizational denials before Rust validates the request.
set -euo pipefail

deny() {
  printf '%s\n' "{\"decision\":\"deny\",\"message\":\"$1\"}"
  exit 0
}

if ! command -v jq >/dev/null 2>&1; then
  deny 'hook requires jq; refusing agent spawn'
fi

request=$(cat) || deny 'could not read hook request'
if ! jq -e . >/dev/null 2>&1 <<<"$request"; then
  deny 'invalid hook JSON; refusing agent spawn'
fi
event=$(jq -er '.event' <<<"$request") || deny 'missing hook event'
tool=$(jq -er '.payload.tool' <<<"$request") || deny 'missing tool name'
level=$(jq -er '.payload.agent_level | if type == "number" and floor == . then . else error("level") end' <<<"$request") || deny 'invalid agent level'
name=$(jq -er '.payload.arguments.name | strings' <<<"$request") || deny 'missing child agent name'
[ "$event" = 'agent_spawning' ] && [ "$tool" = 'spawn_agent' ] || deny 'unexpected hook payload'

# The actor at level 3 or deeper cannot spawn.  The field is authenticated
# context supplied by the supervisor, rather than a child-provided argument.
if [ "$level" -ge 3 ]; then
  deny 'agents at level 3 or deeper may not spawn children'
fi

# Keep names portable and predictable: ASCII letters/digits, dot, underscore,
# and hyphen only.  Empty names and whitespace are rejected as special cases.
case "$name" in
  ''|*[!A-Za-z0-9._-]*) deny 'agent name contains forbidden special characters' ;;
esac

printf '%s\n' '{"decision":"allow"}'
