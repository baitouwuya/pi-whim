# Supervisor hooks

Pi-Whim hooks extend the Rust agent supervisor control plane. They do not mirror Pi's
internal model-loop extension events.

## Manifest

The global manifest is `${config_dir}/pi-whim/hooks.json` (`~/Library/Application Support`
on macOS). It is read when a session runtime starts. Invalid manifests are reported in
the application and no hooks from that manifest run.

```json
{
  "version": 1,
  "hooks": [
    {
      "id": "protect-main",
      "event": "tool_dispatching",
      "kind": "gate",
      "command": ["/absolute/path/to/protect-main"],
      "timeout_ms": 3000,
      "matcher": { "tools": ["bash"], "agent_levels": [0, 1] }
    }
  ]
}
```

Hook IDs must be unique stable identifiers containing only ASCII letters, digits, `.`,
`_`, and `-`. Command entrypoints must be absolute files, and timeouts must be between
1 and 30000 ms. Unknown fields are rejected at every Manifest level so a misspelled
security matcher cannot silently broaden a Hook. Hooks run in Manifest order.

Compile-time Rust hooks run before command hooks and cannot be disabled by a Manifest.
The initial `builtin.safety_floor` rejects malformed spawn and message events before
external code runs; typed Supervisor handlers validate every transformed value
again before carrying out an operation.

A project may add `.pi-whim/hooks.json`. Project hooks remain disabled until the user
approves the displayed SHA-256 fingerprint under Settings > Execution > Project hooks.
The fingerprint covers the Manifest plus every command entrypoint path and file content;
changing any of them invalidates approval. It does **not** cover helpers, interpreters, or
other dependencies loaded from the entrypoint's directory. Security-critical project hooks
must therefore live in a trusted, project-non-writable location (or use an immutable,
verified executable closure). Global hooks run first, followed
by approved project hooks, in their respective Manifest order. Duplicate IDs across the
merged configuration invalidate the merged set.

Approved project entrypoints are hashed again immediately before every invocation. A
post-launch replacement therefore follows the Hook kind's failure policy. The verified
bytes are copied into the invocation's private temporary directory and that snapshot is
executed, closing the replacement window between verification and process launch.

## Protocol

The command runs with the project root as its working directory and receives one JSON
document on stdin. `entrypoint` is the configured path even when an approved project
entrypoint is executed from a verified per-invocation snapshot:

```json
{"version":1,"hook_id":"protect-main","event":"tool_dispatching","entrypoint":"/absolute/path/to/protect-main","project_root":"/absolute/project","payload":{"tool":"bash","agent_id":"...","agent_level":0,"arguments":{}}}
```

Agent-scoped events carry a common authenticated context assembled by the Rust
supervisor, never from tool arguments: `agent_id`, `agent_level`, `team_id`, `session_id`,
nullable `parent_agent_id`, nullable `parent_session_id`, `agent_name`, `agent_role`, and
nullable `request_id`. A tool request uses its protocol request ID; lifecycle events that
do not originate from a request use `null`. Event-specific fields remain at the same
top level and take precedence when they intentionally describe a historical session,
such as `session_expired.session_id`.

A `gate` hook returns an empty response or one JSON document. `{"decision":"deny",
"message":"reason"}` rejects the operation. `{"arguments":{...}}` replaces the event's
arguments when returned by a `transform` hook; the normal typed handler validates the
replacement before use. `observe` hook output is ignored.

Transforms are event-specific and never grant a capability or bypass the Rust
reference monitor. `tool_dispatching` may replace the **entire** `arguments` object; the
typed handler reparses and validates it afterwards. This is functional rewriting, not a
trusted command or permission filter. `message_sending` may change only
`arguments.message`; every other argument field, including the target, must remain
byte-for-byte equivalent in JSON value terms. `agent_spawning` may change only an explicit
`permission_level` to a lower level and may shrink explicit, non-empty
`enabled_tools` or `trusted_extensions` allowlists to subsets. All other spawn fields
(including task, name, role, model, provider, and target) must be unchanged; transforms
cannot add fields or expand permissions. `permission_resolving` is a deny-only Gate and
runs only before an approval is granted; denials and cancellations can never be blocked by
a Hook.

Gate events fail closed on launch, timeout, oversized output, or invalid JSON. Transform
failures preserve the prior arguments. Observe events are best effort. Hook output is
limited to 64 KiB. Gate `decision` values must be `allow` or `deny`; malformed values and
invalid UTF-8 are failures rather than implicit approval.

Each invocation writes bounded audit metadata to SQLite: hook ID, event, outcome,
duration, output-truncation flag, configuration revision, and timestamp. Arguments,
message bodies, credentials, capabilities, and raw command output are never persisted.
At most 10000 entries are retained per project.

## Events

All UUID fields are JSON strings. Every agent-scoped payload includes this common
context (unless the referenced agent is no longer available): `agent_id`, `agent_level`,
`team_id`, `session_id`, nullable `parent_agent_id`, nullable `parent_session_id`, nullable
`request_id`, `agent_name`, and `agent_role`. The following table lists every
**event-specific** field in addition to that context; `arguments` is the complete JSON
object submitted to the corresponding tool. Event payloads are versioned with the outer
protocol.

| Event | Supported kind | Event-specific payload fields |
| --- | --- | --- |
| `supervisor_started` | observe | `root_agent_id` |
| `supervisor_stopping` | observe | `root_agent_id` |
| `session_published` | observe | none; common `agent_id`, `session_id`, and `agent_level` identify the published root session |
| `session_expired` | observe | `session_id` identifies the expired session and intentionally overrides the current common-context session ID |
| `tool_dispatching` | gate, transform | `tool` (protocol tool constant), `arguments` |
| `agent_spawning` | gate, transform | `tool` (`spawn_agent`), `arguments` (the spawn request) |
| `message_sending` | gate, transform | `tool` (`send_message`), `arguments` (the send request) |
| `permission_resolving` | gate only | `request_id`, `requester_id`, `owner_id`, `title`, nullable `operation_hash`, `decision` (`approve`); emitted only before an approval |
| `tool_completed` | observe | `tool` (protocol tool constant), `success` (boolean) |
| `agent_started` | observe | none; the common context describes the started child |
| `agent_finished` | observe | `interrupted` (boolean), nullable `exit_code` |
| `message_delivered` | observe | `sender_id`, `delivery` (the send operation's delivery-result object) |
| `interaction_created` | observe | `request_id`, `requester_id`, `owner_id` |
| `interaction_resolved` | observe | `request_id`, `requester_id`, `decision` |
| `team_reset` | observe | `team_id`, `session_id` (the newly reset root session) |

This is a strict matrix, not a general `15 × 3` event/kind system:
`tool_dispatching`, `agent_spawning`, and `message_sending` support Gate and Transform;
`permission_resolving` supports only Gate; the other eleven events support only Observe.
Invalid event/kind combinations are rejected when the manifest is validated.

`matcher.tools` and `matcher.agent_levels` compare exact top-level payload values. Tool
matcher values are **protocol constant names**, not UI or documentation aliases; examples
include `bash`, `read`, `write`, `edit`, `spawn_agent`, and `send_message`. An empty matcher
list means no restriction for that dimension. Events without the relevant top-level field
do not match a non-empty matcher for that dimension.

`supervisor_stopping` and the final `session_expired` run synchronously within a five-second
phase budget. Individual timeouts are capped by the remaining budget; failure never
prevents process and capability cleanup.

Internal supervisor tools and hook execution do not recursively invoke hooks.

## Security

Hooks are defense in depth, not a replacement for typed handlers, capability validation,
`AgentPermissionPolicy`, approval tickets, routing checks, path canonicalization, or the
agent sandbox. A Gate can add a denial but cannot authorize an operation, grant a tool,
or make later Rust checks succeed. A Transform must be monotonic where its event contract
requires it: it can only make the permitted spawn policy smaller; it cannot enlarge a
permission level or allowlist. The unconstrained `tool_dispatching` argument replacement
is specifically not a security sanitizer and must not be used to turn a dangerous command
into an approved one.

Observe is best-effort telemetry, not reliable audit storage: delivery is asynchronous,
the bounded queue can discard work when full, and the hook's private temporary directory
is deleted after invocation. The built-in SQLite audit stores bounded metadata (hook ID,
event, outcome, duration, truncation, configuration revision, and timestamp), not
arguments, message bodies, credentials, capabilities, or raw hook output. Use a
controlled, durable audit sink when retention is required.

Command hooks run through macOS `sandbox-exec` with an empty environment. They can read
the project and command directory, write only a per-invocation temporary directory, and
have no network rule, provider keys, supervisor endpoint, or agent capability. If
`sandbox-exec` is unavailable, gate hooks reject the operation, transform hooks preserve
the original value, and observe failures are audited without affecting the operation.
