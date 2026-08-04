#!/bin/bash
# Additional deny-only policy for approval resolution.
#
# Configure this as a Gate with event `permission_resolving`.  The dispatcher
# invokes this event only for an `approve` decision, so this hook cannot block a
# denial/cancellation or automatically approve an operation.  Hashes below are
# examples; replace them with reviewed operation hashes for your deployment.
set -euo pipefail

deny() {
  printf '%s\n' "{\"decision\":\"deny\",\"message\":\"$1\"}"
  exit 0
}

if ! command -v jq >/dev/null 2>&1; then
  deny 'hook requires jq; refusing approval'
fi

request=$(cat) || deny 'could not read hook request'
if ! jq -e . >/dev/null 2>&1 <<<"$request"; then
  deny 'invalid hook JSON; refusing approval'
fi
event=$(jq -er '.event' <<<"$request") || deny 'missing hook event'
decision=$(jq -er '.payload.decision | strings' <<<"$request") || deny 'missing approval decision'
# `operation_hash` is nullable; reject absent/null values to fail closed rather
# than accidentally approve an operation that cannot be matched to policy.
hash=$(jq -er '.payload.operation_hash | strings' <<<"$request") || deny 'missing operation hash'
[ "$event" = 'permission_resolving' ] || deny 'unexpected hook payload'
[ "$decision" = 'approve' ] || deny 'unexpected approval decision'

# The Rust host computes hashes; never derive a hash from untrusted arguments.
# Add audited values here, one exact operation hash per case.
case "$hash" in
  fnv1a:0000000000000000|fnv1a:deadbeefdeadbeef)
    deny 'operation hash is blocked by approval policy'
    ;;
esac

printf '%s\n' '{"decision":"allow"}'
