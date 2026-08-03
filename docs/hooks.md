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
changing any of them invalidates approval. Global hooks run first, followed
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

A `gate` hook returns an empty response or one JSON document. `{"decision":"deny",
"message":"reason"}` rejects the operation. `{"arguments":{...}}` replaces the event's
arguments when returned by a `transform` hook; the normal typed handler validates the
replacement before use. `observe` hook output is ignored.

Transforms are event-specific. `tool_dispatching` may replace its argument object;
`message_sending` may replace only `message`; `agent_spawning` may only lower an explicit
permission level or shrink explicit non-empty tool/extension allowlists. Task, identity,
model, target, and unrelated fields cannot be changed. `permission_resolving` is a
deny-only Gate and runs only before an approval is granted; denials and cancellations can
never be blocked by a Hook.

Gate events fail closed on launch, timeout, oversized output, or invalid JSON. Transform
failures preserve the prior arguments. Observe events are best effort. Hook output is
limited to 64 KiB. Gate `decision` values must be `allow` or `deny`; malformed values and
invalid UTF-8 are failures rather than implicit approval.

Each invocation writes bounded audit metadata to SQLite: hook ID, event, outcome,
duration, output-truncation flag, configuration revision, and timestamp. Arguments,
message bodies, credentials, capabilities, and raw command output are never persisted.
At most 10000 entries are retained per project.

## Events

All UUID fields are JSON strings. Event payloads are versioned with the outer protocol:

| Event | Kind | Payload fields |
| --- | --- | --- |
| `supervisor_started` | observe | `root_agent_id` |
| `supervisor_stopping` | observe | `root_agent_id` |
| `session_published` | observe | `agent_id`, `session_id`, `agent_level` |
| `session_expired` | observe | `agent_id`, nullable `session_id`, `agent_level` |
| `tool_dispatching` | gate/transform | `tool`, `agent_id`, `agent_level`, `arguments` |
| `agent_spawning` | gate/transform | `tool`, `agent_id`, `agent_level`, `arguments` |
| `message_sending` | gate/transform | `tool`, `agent_id`, `agent_level`, `arguments` |
| `permission_resolving` | gate | `request_id`, `requester_id`, `owner_id`, `title`, nullable `operation_hash`, `decision` |
| `tool_completed` | observe | `tool`, `agent_id`, `success` |
| `agent_started` | observe | `agent_id`, `agent_level` |
| `agent_finished` | observe | `agent_id`, `interrupted`, nullable `exit_code` |
| `message_delivered` | observe | `sender_id`, `delivery` object |
| `interaction_created` | observe | `request_id`, `requester_id`, `owner_id` |
| `interaction_resolved` | observe | `request_id`, `requester_id`, `decision` |
| `team_reset` | observe | `team_id`, `session_id` |

`matcher.tools` and `matcher.agent_levels` compare exact top-level payload values. An
empty matcher list means no restriction for that dimension. Events without the relevant
top-level field do not match a non-empty matcher for that dimension.

`supervisor_stopping` and the final `session_expired` run synchronously within a five-second
phase budget. Individual timeouts are capped by the remaining budget; failure never
prevents process and capability cleanup.

Internal supervisor tools and hook execution do not recursively invoke hooks.

## Security

Command hooks run through macOS `sandbox-exec` with an empty environment. They can read
the project and command directory, write only a per-invocation temporary directory, and
have no network rule, provider keys, supervisor endpoint, or agent capability. If
`sandbox-exec` is unavailable, gate hooks reject the operation, transform hooks preserve
the original value, and observe failures are audited without affecting the operation.
