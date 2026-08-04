#!/bin/bash
# Additional raw-path deny policy for read, write, and edit dispatches.
#
# Configure this as a Gate with event `tool_dispatching` and matcher.tools set
# to read, write, and edit.  It blocks only write/edit requests because reads do
# not modify a file.  This is NOT a substitute for Rust-side path
# canonicalization, symlink protection, scopes, or approval-ticket validation.
set -euo pipefail

deny() {
  printf '%s\n' "{\"decision\":\"deny\",\"message\":\"$1\"}"
  exit 0
}

if ! command -v jq >/dev/null 2>&1; then
  deny 'hook requires jq; refusing file operation'
fi

request=$(cat) || deny 'could not read hook request'
if ! jq -e . >/dev/null 2>&1 <<<"$request"; then
  deny 'invalid hook JSON; refusing file operation'
fi

event=$(jq -er '.event' <<<"$request") || deny 'missing hook event'
tool=$(jq -er '.payload.tool' <<<"$request") || deny 'missing tool name'
path=$(jq -er '.payload.arguments.path | strings' <<<"$request") || deny 'missing file path'
[ "$event" = 'tool_dispatching' ] || deny 'unexpected hook payload'
case "$tool" in read|write|edit) ;; *) deny 'unexpected file tool' ;; esac

# A read is non-mutating.  Canonicalization and scope checks remain mandatory.
if [ "$tool" = 'read' ]; then
  printf '%s\n' '{"decision":"allow"}'
  exit 0
fi

# Raw matching catches obvious protected paths, including a .git directory in
# either absolute or relative spelling.  It cannot detect symlink traversal.
case "$path" in
  /etc/*|/usr/*|/System/*|*.git|*.git/*|*/.git|*/.git/*)
    deny 'write or edit targets a protected raw path'
    ;;
esac

printf '%s\n' '{"decision":"allow"}'
