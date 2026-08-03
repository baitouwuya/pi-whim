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
      "command": ["/absolute/path/to/protect-main"],
      "timeout_ms": 3000,
      "matcher": { "tools": ["bash"], "agent_levels": [0, 1] }
    }
  ]
}
```

Hook IDs must be unique, command entrypoints must be absolute files, and timeouts must
be between 1 and 30000 ms. Hooks run in manifest order.

A project may add `.pi-whim/hooks.json`. Project hooks remain disabled until the user
approves the displayed SHA-256 fingerprint under Settings > Execution > Project hooks.
Changing any byte of the Manifest invalidates approval. Global hooks run first, followed
by approved project hooks, in their respective Manifest order. Duplicate IDs across the
merged configuration invalidate the merged set.

## Protocol

The command receives one JSON document on stdin:

```json
{"version":1,"event":"tool_dispatching","payload":{"tool":"bash","agent_id":"...","agent_level":0,"arguments":{}}}
```

A gate hook returns an empty response or one JSON document. `{"decision":"deny",
"message":"reason"}` rejects the operation. `{"arguments":{...}}` replaces the event's
arguments; the normal typed handler validates the replacement before use. Observe hooks'
output is ignored.

Gate events fail closed on launch, timeout, oversized output, or invalid JSON. Observe
events are best effort. Hook output is limited to 64 KiB.

Each invocation writes bounded audit metadata to SQLite: hook ID, event, outcome,
duration, output-truncation flag, configuration revision, and timestamp. Arguments,
message bodies, credentials, capabilities, and raw command output are never persisted.

## Events

- Gates: `tool_dispatching`, `agent_spawning`, `message_sending`,
  `permission_resolving`.
- Observe: `tool_completed`, `agent_started`, `agent_finished`, `message_delivered`,
  `interaction_created`, `interaction_resolved`, `team_reset`.

Internal supervisor tools and hook execution do not recursively invoke hooks.

## Security

Command hooks run through macOS `sandbox-exec` with an empty environment. They can read
the project and command directory, write only a per-invocation temporary directory, and
have no network rule, provider keys, supervisor endpoint, or agent capability. If
`sandbox-exec` is unavailable, gate hooks reject the operation and observe hooks are
skipped.
