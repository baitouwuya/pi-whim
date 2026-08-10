# Hooks

Pi-Whim Hooks extend the Rust supervisor and application command control planes. They do not
mirror Pi's internal model-loop extension events, do not receive view/render events, and never
replace the Rust reference monitor.

The implementation has three separate phases:

- **Gate** can add a denial. It cannot authorize an operation.
- **Transform** can rewrite only fields explicitly authorized by the event registry and manifest.
- **Observe** is bounded, best-effort external notification after an outcome.

There is no mixed `HookDecision` protocol. Typed Rust payloads are serialized only at the
`pi-whim-hook-host` boundary, and authenticated invocation context remains separate from mutable
payload data.

## Manifests

The global manifest is `${config_dir}/pi-whim/hooks.json` (`~/Library/Application Support` on
macOS). A project may add `.pi-whim/hooks.json`. Both are parsed by
`HookManifest::parse_json`, validated against `EventRegistry`, and preserve manifest order.
Global definitions run before approved project definitions in every external phase.

Versions 1 and 2 are supported:

- **v1** accepts the documented legacy event aliases and launches a sandboxed one-shot process
  for each invocation. It remains the compatibility format used by the legacy supervisor start
  path when a shared scope cannot be composed.
- **v2** requires canonical namespaced event names, an explicit `fields` allowlist, and persistent
  process `delivery` and `restart` policy. UI/state events have no v1 aliases. A v2 project
  manifest is shared-scope only and is never silently reduced to v1.

Unknown JSON fields and unknown event/kind/matcher/field combinations are rejected. Current
manifest bounds are:

| Item | Limit |
| --- | --- |
| hooks per manifest | 64 |
| hook ID | 1–128 bytes; ASCII letters, digits, `.`, `_`, `-` |
| command vector | 1–16 entries; first entry is an absolute path |
| command entry | at most 4 KiB |
| combined command entries | at most 16 KiB |
| authorized fields | at most 64 |
| matcher keys | at most 16 |
| timeout | 1–30000 ms; default 5000 ms |
| stdout JSONL line | at most 64 KiB |
| delivery capacity | 1–64 |
| restart budget | at most 3 restarts |
| restart backoff | 250–5000 ms, initial not greater than maximum |

Hook IDs must be unique within a manifest. A composed global/project scope also rejects duplicate
IDs. Hooks execute in manifest order.

### v1 example

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

For exact approval, a v1 definition is expanded to the registry's effective field set. The user
therefore approves what the compatibility definition can actually receive, not an empty or
implicit field description.

### v2 example

```json
{
  "version": 2,
  "hooks": [
    {
      "id": "review-ui-prompt",
      "event": "pi.ui.command.submitting",
      "kind": "transform",
      "command": ["/absolute/path/to/review-ui-prompt"],
      "timeout_ms": 3000,
      "fields": ["command_id", "command_name", "source", "project_id", "arguments"],
      "matcher": { "command_name": "submit_prompt", "source": "ui" },
      "delivery": { "mode": "request_response", "capacity": 1 },
      "restart": {
        "max_restarts": 3,
        "initial_backoff_ms": 250,
        "max_backoff_ms": 5000
      }
    }
  ]
}
```

`delivery.mode` is `request_response`, `state_latest`, or `telemetry`; `latest` is accepted as a
parser alias for `state_latest`. Gate and Transform definitions must use `request_response`.
Observe may use any mode. `state_latest` keeps only the newest pending state notification;
`telemetry` uses the configured bounded queue.

Field names or nested object keys that imply secrets, capabilities, environment, API keys,
approval tickets, endpoints, credentials, authorization data, or tokens are forbidden. The
registry also rejects forbidden data classes. A manifest cannot use its allowlist to request data
the event does not expose or data unavailable to project scopes.

## Project loading and exact approval

Global and project manifests are prepared separately. A project manifest is validated as a
project manifest and opened as the project half of a scope; it is never merged into the global
manifest to evade `project_visible` checks.

Preparation computes:

- an SHA-256 for each hook entrypoint,
- a combined fingerprint over the original manifest bytes and each entrypoint path and content,
- canonical exact grant descriptors in manifest order, and
- `grants_hash`, the SHA-256 of the canonical descriptor JSON.

Each descriptor contains the hook ID, canonical event, kind, effective fields, canonical matcher
metadata, delivery mode/capacity, restart budget/backoff, and entrypoint SHA-256. Settings shows
these descriptors for both pending and approved project Hooks. Persistence stores and compares
the fingerprint, `grants_hash`, and full descriptor JSON. Legacy fingerprint-only trust rows have
no exact grant identity and are treated as unapproved.

Approval rereads the manifest and entrypoints and must match both the displayed fingerprint and
`grants_hash` before writing trust. A change to manifest bytes, event, kind, fields, matcher,
delivery, restart policy, entrypoint path, or entrypoint content requires approval again.
Approved entrypoints are verified immediately before launch and executed from a private verified
snapshot for both one-shot and persistent processes. Helpers or interpreters loaded by the
entrypoint are not covered by the entrypoint hash and must be secured separately.

If project Hooks are absent, unreadable, invalid, untrusted, changed, or cannot compose, the app
immediately removes and revokes any retained project scope before using the global-only path.
Explicit revoke deletes trust, removes the retained controller/scope, calls manager scope
revocation, and boundedly stops persistent processes. Reopening the same key after revocation
creates a fresh non-revoked scope even while old handles remain alive.

## Scope ownership and ordering

`ApplicationHookHost` owns a `HookHostManager` for each active global manifest revision and opens
project scopes using a stable combined global/project revision. Sessions for the same project and
revision reuse the same cloneable `HookScopeHandle`. The UI command controller and every agent
supervisor receive that handle; they do not create another manager or subprocess set.

The app subscribes exactly once to each manager's audit and health signals. Shared supervisors do
not subscribe to or duplicate external manager audit. The compatibility v1 supervisor path owns
its own manager and retains its local adapter. Revoking a project scope invalidates every clone.

Within each external phase the host preserves global-before-project ordering and each manifest's
source order. Hook execution does not recursively trigger Hooks.

## Execution protocols

### v1 one-shot

A v1 hook receives one JSON document on stdin and exits after returning at most one bounded JSON
response. Scope authentication is checked by the host before launch; the one-shot wire document
contains the registry-filtered `payload`, not the v2 `context` object. Gate returns
allow/deny, Transform returns a transformed payload, and Observe output has no control authority.

### v2 persistent JSONL

A v2 hook is a resident sandboxed process. The host starts with a nonce-bearing handshake:

```json
{"type":"hello","protocol":2,"hook_id":"review-ui-prompt","event":"pi.ui.command.submitting","kind":"gate","hello_id":"..."}
```

The hook must echo the exact hook ID, event, and nonce. The ready-frame `kind` is optional; if
present, it must match:

```json
{"type":"ready","hook_id":"review-ui-prompt","event":"pi.ui.command.submitting","kind":"gate","hello_id":"..."}
```

The host then sends LF-delimited requests:

```json
{"type":"request","request_id":"...","hook_id":"review-ui-prompt","event":"pi.ui.command.submitting","kind":"gate","context":{},"payload":{}}
```

The hook responds with matching request and definition identity:

```json
{"type":"response","request_id":"...","hook_id":"review-ui-prompt","event":"pi.ui.command.submitting","response":{"kind":"gate","decision":"allow"}}
```

A Transform response uses `{"kind":"transform","payload":{...}}`; an Observe response may use
`{"kind":"observe","accepted":true}`. Identity, kind, request ID, and handshake nonce are
validated. Protocol failure consumes the bounded restart budget and uses the configured bounded
backoff. Gate failure is fail-closed, Transform failure or invalid output preserves the preceding
typed value, and Observe failure does not change the completed operation.

## Event registry

v2 uses the canonical names below. The alias column is accepted only by v1 agent manifests. UI
and state events deliberately have no alias.

| Canonical event | v1 alias | Kinds |
| --- | --- | --- |
| `pi.supervisor.started` | `supervisor_started` | Observe |
| `pi.supervisor.stopping` | `supervisor_stopping` | Observe |
| `pi.session.published` | `session_published` | Observe |
| `pi.session.expired` | `session_expired` | Observe |
| `pi.tool.dispatching` | `tool_dispatching` | Gate, Transform |
| `pi.tool.completed` | `tool_completed` | Observe |
| `pi.tool.denied` | `tool_denied` | Observe |
| `pi.agent.spawning` | `agent_spawning` | Gate, Transform |
| `pi.agent.launching` | `agent_launching` | Gate |
| `pi.agent.started` | `agent_started` | Observe |
| `pi.agent.finished` | `agent_finished` | Observe |
| `pi.message.sending` | `message_sending` | Gate, Transform |
| `pi.message.delivered` | `message_delivered` | Observe |
| `pi.permission.resolving` | `permission_resolving` | Gate |
| `pi.interaction.created` | `interaction_created` | Observe |
| `pi.interaction.resolving` | `interaction_resolving` | Transform |
| `pi.interaction.resolved` | `interaction_resolved` | Observe |
| `pi.team.reset` | `team_reset` | Observe |
| `pi.ui.command.submitting` | none | Gate, Transform |
| `pi.ui.command.lifecycle` | none | Observe |
| `pi.state.committed` | none | Observe |

Agent definitions accept matcher keys `tools`, `agent_levels`, `source`, `agent_id`, `project_id`,
and `operation` where the event payload can satisfy them. `tools` and `agent_levels` compare exact
top-level values. Tool names are protocol constants such as `bash`, `read`, `write`, `edit`,
`spawn_agent`, and `send_message`.

The generic agent registry authorizes only its bounded non-secret field vocabulary, including
operation metadata and event-specific user content. Agent-team applies a stricter event-specific
typed boundary: supervisor-derived agent, team, session, parent, request, name, and role fields
form an immutable event envelope, while event-specific mutable fields are transformed separately.
The host's authenticated scope context (scope ID, revision, project root, and grants hash) remains
separate on the v2 wire. Capability data, provider keys, environment, approval tickets, supervisor
endpoints, credentials, and secrets are never payload.

### UI and state fields

| Event | Matcher keys | Authorized fields | Transformable fields |
| --- | --- | --- | --- |
| `pi.ui.command.submitting` | `command_name`, `source`, `project_id` | `command_id`, `command_name`, `source`, `project_id`, `arguments` | `arguments` only |
| `pi.ui.command.lifecycle` | `command_name`, `source`, `project_id`, `stage` | `command_id`, `command_name`, `source`, `project_id`, `stage`, `diagnostic` | none |
| `pi.state.committed` | `commit_source`, `project_id`, `scope` | `revision`, `topics`, `action_count`, `coalesced`, `scope`, `commit_source`, `project_id` | none |

The command adapter exposes only the bounded arguments authorized for a supported command. For
prompt submission, attachment paths and contents are withheld; only `attachment_count` is
visible, and transforms may change permitted prompt content/mode without changing attachments.
Envelope `command_id`, source, project/session context, command variant, and authority-bearing
identity cannot be changed.

There are no render, layout, frame, focus, clipboard, paste, API-key, or provider-credential Hook
events. Those operations remain in the shell/platform lane.

## Typed command and supervisor pipelines

A dual Gate+Transform event runs in this order:

1. builtin typed validation and safety-floor precheck,
2. Rust typed Transform hooks, then global and project external Transform definitions,
3. typed reparse, final validation, and safety-floor recheck,
4. Rust typed Gate hooks, then global and project external Gate definitions on the final payload,
5. the typed handler.

Gate-only events skip Transform. Transform-only events still perform final typed reparse and
safety validation. Observe runs after the outcome. A Gate is never run only against the
pre-Transform value.

UI `GateTransform` commands run through one Host-owned background FIFO, with at most one control
operation active, so GPUI is never blocked and controlled commands preserve submission order.
Commands classified `Bypass` or `ObserveOnly` are not gated or transformed. Safety commands bypass
the FIFO and execute immediately. The UI lifecycle Observe mirrors the reliable local lifecycle
signal but remains best-effort externally. `Completed` means dispatch to app orchestration, not
provider-level completion.

For committed state, the application first commits, then publishes the reliable local
`ChangeSet`, then sends the best-effort `pi.state.committed` Observe. Hook failure cannot roll back
the already committed state.

## Security invariants

Hooks are defense in depth, not a replacement for typed handlers, capabilities,
`AgentPermissionPolicy`, approval tickets, topology and routing checks, path canonicalization,
tool policy, or sandboxing.

- A Hook may add denial but cannot authorize, grant a tool, or make a failed Rust check succeed.
- Tool-disabled, capability, topology, policy, path, and sandbox checks remain the builtin floor.
- Spawn transforms can only tighten permission, tool, extension, and model policy. They cannot
  expand allowlists or change task/name/role/provider/model identity semantics.
- Message transforms can change only the body; target and actor identity are immutable.
- Interaction questions may receive a non-empty answer. An approval can only be auto-denied,
  never auto-approved.
- Permission resolving and agent launching are deny-only.
- Deny, cancel, stop, reset, revoke, queue-clear, and equivalent safety operations cannot be
  blocked by a Hook.
- Gate errors fail closed. Transform errors preserve the previous typed value and are followed by
  final parsing and safety validation. Observe errors are best-effort telemetry failures.
- Hook invocation and internal supervisor operations cannot recursively invoke the same Hook
  pipeline.
- Render, layout, and frame work never enters Hook control.

Hooks run under macOS `sandbox-exec` with environment passthrough cleared; only a fixed `PATH` and
the private `TMPDIR` are supplied. They can read only the configured project/command inputs
allowed by the profile, write only their private temporary area, and have no network rule,
provider key, supervisor endpoint, or agent capability. If sandbox execution is
unavailable, Gate rejects, Transform preserves, and Observe records failure without affecting the
completed operation.

## Audit and health

External audit is persisted as bounded metadata only:

- hook ID and scope ID,
- canonical event and kind,
- outcome and duration,
- manifest revision and optional exact `grants_hash`,
- dropped flag, restart count, and drop count,
- timestamp.

Persistence may retain `project_path` as indexing metadata. It never stores payload, transformed
output, raw stdout/stderr, command arguments, message bodies, session/attachment/entrypoint paths,
environment, credentials, provider keys, capabilities, endpoints, tokens, or secrets.
Builtin supervisor audit uses explicit safe defaults instead of fabricating external process
metadata.

Each manager also publishes replayable health for every resident definition: hook ID, scope ID,
event, `Starting`/`Ready`/`Unhealthy`/`Stopped` status, revision, restart count, drop count, and a
bounded last-error diagnostic. The app's single manager subscription wakes Host, which commits a
stably sorted health snapshot through `SetHookHealth`; Settings receives it through the normal
state projection path. Initial, unhealthy, restarted, dropped, revoked, and stopped states remain
observable without one subscription per supervisor.

The distinction is intentional: local `ChangeSet`, `CommandLifecycle`, and `ApplicationEffect`
signals are reliable application channels. External Observe delivery is bounded and best-effort;
its drop/restart state is exposed through audit and health rather than being presented as a
reliable transport.
