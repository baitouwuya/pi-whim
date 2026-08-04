#!/bin/bash
# Additional deny-in-depth policy for tool_dispatching hooks matching `bash`.
#
# Configure this as a Gate with event `tool_dispatching`, matcher.tools `bash`.
# The Rust supervisor still enforces its command policy, approvals, and sandbox;
# this raw-text check cannot replace those controls.  Root (agent level 0) is
# deliberately exempt so that this optional organizational policy does not
# constrain the interactive owner.
set -euo pipefail

# A Gate must fail closed.  Do not emit diagnostics on stdout: stdout is the
# hook protocol response, while stderr is intentionally not available to users.
deny() {
  printf '%s\n' "{\"decision\":\"deny\",\"message\":\"$1\"}"
  exit 0
}

if ! command -v jq >/dev/null 2>&1; then
  deny 'hook requires jq; refusing bash operation'
fi

request=$(cat) || deny 'could not read hook request'
if ! jq -e . >/dev/null 2>&1 <<<"$request"; then
  deny 'invalid hook JSON; refusing bash operation'
fi

# Validate the envelope even though the manifest matcher normally guarantees it.
event=$(jq -er '.event' <<<"$request") || deny 'missing hook event'
tool=$(jq -er '.payload.tool' <<<"$request") || deny 'missing tool name'
level=$(jq -er '.payload.agent_level | if type == "number" and floor == . then . else error("level") end' <<<"$request") || deny 'invalid agent level'
command=$(jq -er '.payload.arguments.command | strings' <<<"$request") || deny 'missing bash command'
[ "$event" = 'tool_dispatching' ] && [ "$tool" = 'bash' ] || deny 'unexpected hook payload'

# The authenticated level is injected by the supervisor, not read from arguments.
if [ "$level" -eq 0 ]; then
  printf '%s\n' '{"decision":"allow"}'
  exit 0
fi

# These are intentionally conservative substring checks.  They add denials but
# do not parse shell syntax and must not be used as an authorization mechanism.
case "$command" in
  *curl*) deny 'bash command contains curl' ;;
  *wget*) deny 'bash command contains wget' ;;
  *'rm -rf /'*) deny 'bash command contains rm -rf /' ;;
esac

printf '%s\n' '{"decision":"allow"}'
