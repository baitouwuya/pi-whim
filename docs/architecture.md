# Architecture decisions

Pi-Whim is a capability-secured, reactive workbench around Pi. Its boundaries are typed:
views emit commands, the application commits domain state, projections update views, and the
agent supervisor owns subprocess identity and policy. JSON is serialized only at external
protocol boundaries; it is not the application's internal command or state model.

Pi-Whim has one agent runtime boundary: application orchestration talks only to
`AgentRuntime`. The current runtime launches Pi in `--mode rpc` and exchanges strict
LF-delimited JSONL. Process handling, Pi wire types, and future transport concerns therefore
do not leak into the view layer.

The SQLite database is an index, not the source of conversation truth. Pi owns JSONL sessions
under the Pi-Whim application support directory. The index stores enough metadata for the
project sidebar to load without parsing every conversation first.

## Crate layering

The workspace has 14 crates in a strict acyclic layering enforced by Cargo dependency edges,
not convention. Upper layers physically cannot reach lower internals:

- **Foundation**: `pi-whim-core`, `pi-whim-signal`, `pi-whim-tool-protocol`,
  `pi-whim-pi-rpc`, `pi-whim-theme`
- **Services and runtime components**: `pi-whim-hook-host`, `pi-whim-persistence`,
  `pi-whim-catalog`, `pi-whim-one-shot-ai`, `pi-whim-agent-team`
- **Upper orchestration and UI**: `pi-whim-runtime`, `pi-whim-engine`, `pi-whim-gpui`,
  `pi-whim-app`

`pi-whim-signal` supplies framework-independent typed `Signal`, replayable state, `Gate`, and
`Transform` primitives. `pi-whim-hook-host` depends on that signal layer and owns manifest
validation, approved scopes, sandboxed one-shot execution, persistent v2 processes, audit, and
health. Agent and UI callers consume its public typed phases rather than embedding another Hook
executor.

A `cargo build` failure about an unknown crate usually means a layering boundary was crossed;
fix the dependency rather than adding a workspace-wide `path` dependency.

## Compositional reactive architecture

### UI command plane

GPUI `Workspace` emits two independent typed signals:

- `Signal<AppCommand>` for framework-independent domain commands.
- `Signal<ShellCommand>` for window, platform, clipboard, paste, picker, provider credential,
  discovery, and provider-test operations.

Only `AppCommand` enters application orchestration or the command Hook pipeline. `ShellCommand`
remains Host-owned. Each signal preserves FIFO order within its lane; the architecture makes no
global ordering promise across the two lanes.

Every domain command is wrapped in `CommandEnvelope<AppCommand>`. The envelope generates a
stable UUID `command_id`, records `CommandSource` (`Ui`, `System`, or `HookReplay`), and carries
optional project and opaque session-key context separately from the typed payload. Its debug
representation is metadata-only and cannot print command payloads.

`AppCommand::control_policy()` classifies commands as `Bypass`, `ObserveOnly`, or
`GateTransform`. Commands eligible for control run on a background executor through one
Host-owned FIFO control slot, so Gate/Transform work never blocks the GPUI main thread and
controlled commands cannot be reordered. Safety operations such as stop, cancel, deny, revoke,
and queue clearing bypass external control and execute immediately, even while a controlled
command is in flight.

The application publishes metadata-only `CommandLifecycle` values through a reliable local
signal. Stages are `Submitted`, `Transforming`, `Accepted`, `Denied`, `Executing`, `Completed`,
and `Failed`. Local lifecycle publication precedes its external Observe notification.
`Completed` means the typed command was handed to application orchestration; it does not claim
that a later provider request, agent run, or other asynchronous operation ultimately succeeded.

### Reducer and state plane

Domain mutation has one commit path:

1. `PiWhimApplication` calls `EngineState::apply_batch` with typed actions and an explicit
   `CommitContext`.
2. A successful commit produces a `ChangeSet` and increments the revision.
3. The application publishes that `ChangeSet` on a reliable local signal.
4. Host-owned `ReplaySelection<T>` instances inspect `StateTopic`s and recompute only matching
   typed feature projections from the committed state.
5. Each replayable selection feeds a `StateSignalBridge` using latest-value delivery, and GPUI
   applies that projection to the corresponding feature views.

The projections cover navigation/sidebar, conversation and queue/session runtime, runtime
controls/composer/chrome, and settings/preferences/providers/search/hooks. A projection may
contain limited fields needed by its feature, but none is a complete `AppState` mirror. A commit
publishes the local `ChangeSet` before the best-effort external `pi.state.committed` Observe.
The Host no longer publishes full-state snapshots as the primary UI synchronization path.

### Application effect plane

Application-to-Host effects use one reliable typed `Signal<ApplicationEffect>` so Notice,
Prompt, SessionClosed, AttachmentReady, ClipboardWrite, and OpenPicker preserve their total
order. A bounded startup buffer exists only until Host installs its retained signal bridge and
activates delivery. The Notice portion is bounded and deduplicates consecutive repeats;
non-Notice startup effects are never discarded. Active delivery emits directly rather than
staging values for a pull API. Debug output is metadata-only for sensitive effects.

### Hook phases and scope ownership

`Gate`, `Transform`, and `Observe` are distinct APIs. They are never collapsed into a mixed
JSON decision. For a dual-phase event the order is builtin typed safety precheck, Transform,
typed reparse and final safety validation, Gate on the final payload, then the handler. Observe
runs after the outcome. See [Hooks](hooks.md) for the event matrix and security invariants.

`ApplicationHookHost` owns each `HookHostManager` for the lifetime of its global manifest
revision. It opens project scopes keyed by project and a stable combined global/project
revision. The resulting cloneable `HookScopeHandle` is shared by the UI command controller and
all supervisors for that project; no caller creates a second manager or executor. The app owns
exactly one audit subscription and one health subscription per manager. Supervisors sharing a
scope emit builtin audit locally but do not duplicate the manager's external audit stream.

Render, layout, frame, focus, and other view-mechanics events never enter the Hook system.
Hooks may control only registered domain command submissions and may observe registered command
lifecycle or committed-state metadata.

## Threading

GPUI is single-threaded. Blocking runtime receivers, persistent Hook I/O, and command Hook
control run away from the main thread. `SignalBridge` posts typed events back onto the GPUI
async loop; `StateSignalBridge` coalesces replayable projection updates to the latest value.
Safety commands do not wait behind asynchronous Hook control.

## Agent teams and sessions

Agent-team tools follow the runtime boundary. A thin TypeScript extension registers stable
model-facing tool names and forwards authenticated requests to the Rust supervisor over loopback
JSONL. The supervisor owns caller identity, topology, quotas, message routing, permissions, and
child process lifetimes; tool arguments are never trusted for team or level identity.

Only level-0 Pi processes use JSONL sessions. Subagents run with `--no-session`, so the
non-recursive session index and sidebar contain only level-0 conversations. See
[Agent teams](agent-teams.md) for the routing and concurrency contract.

## Security model

Subagents run under macOS `sandbox-exec` with capability-based authentication. A child receives
an ephemeral capability token, never a shared secret, and per-process provider keys are injected
by environment variable name rather than copied into Hook payloads. Hook scope context (scope ID, revision, project root, and grants hash) is authenticated and
separate from mutable event payloads. Agent identity is assembled by the supervisor as a typed,
immutable event envelope rather than accepted from tool arguments.

Hooks are an additional denial and rewriting layer, not an authority source. They cannot grant a
capability, expand an `AgentPermissionPolicy`, approve an interaction, bypass tool-disabled,
topology, routing, policy, path, or sandbox checks, or block a safety operation. Hook execution
cannot recursively trigger itself. Project revocation immediately revokes the shared scope and
boundedly stops resident v2 processes.

## Model catalog

`pi-whim-core/build.rs` reads vendored provider JSON
(`vendor/pi-mono/packages/ai/src/providers/data`) and emits a `bundled_model_capabilities.rs`
table at build time. The build fails with the offending path if a real catalog entry lacks the
required `id`/`provider` fields, so upstream schema drift cannot silently shrink the table.
Dotfile manifests (`.manifest.json`) are filtered out as checksum metadata.

## Tool dispatch

Agent-team tool dispatch is currently a string-constant plus match and `is_policy_tool` list
spread across several files; see [ToolSpec redesign](tool-spec-redesign.md) for the planned
single-registry collapse.
