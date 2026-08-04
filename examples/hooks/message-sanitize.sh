#!/bin/bash
# Transform hook for message_sending that redacts probable OpenAI-style keys.
#
# Configure as a Transform with event `message_sending`.  The response returns
# the complete arguments object, retaining target verbatim and changing only
# message.  validate_transform rejects target changes, but this script preserves
# it explicitly.  This is a narrow best-effort redaction, not a general secret
# detector and not a replacement for routing or message-size validation.
set -euo pipefail

fail() {
  # A Transform cannot reject: on command failure the dispatcher deliberately
  # preserves the prior arguments.  Exit nonzero rather than emit malformed or
  # Gate-shaped output; install a separate message_sending Gate when failure
  # must reject delivery.  This is the strongest fail-safe available to a
  # Transform under the documented protocol.
  exit 1
}

if ! command -v jq >/dev/null 2>&1; then
  fail
fi

request=$(cat) || fail
if ! jq -e . >/dev/null 2>&1 <<<"$request"; then
  fail
fi
event=$(jq -er '.event' <<<"$request") || fail
tool=$(jq -er '.payload.tool' <<<"$request") || fail
target=$(jq -er '.payload.arguments.target | strings' <<<"$request") || fail
message=$(jq -er '.payload.arguments.message | strings' <<<"$request") || fail
[ "$event" = 'message_sending' ] && [ "$tool" = 'send_message' ] || fail

# jq performs the replacement and JSON escaping.  The regex replaces tokens
# beginning sk- followed by at least 16 non-whitespace characters.
# Rebuild only the permitted argument fields; target remains byte-for-byte equal.
jq -n --arg target "$target" --arg message "$message" '
  {
    arguments: {
      target: $target,
      message: ($message | gsub("sk-[^[:space:]]{16,}"; "[REDACTED_API_KEY]"))
    }
  }
' || fail
