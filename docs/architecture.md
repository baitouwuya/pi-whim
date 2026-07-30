# Architecture decisions

Pi-Whim has one runtime boundary: the UI talks only to `AgentRuntime`. The first
implementation launches Pi in `--mode rpc` and exchanges strict LF-delimited JSONL.
This prevents process handling, Pi wire types, and future tool transport concerns
from leaking into the view layer.

The SQLite database is an index, not the source of conversation truth. Pi owns JSONL
sessions under the Pi-Whim application support directory. The index stores enough
metadata for the project sidebar to load without parsing every conversation first.

Agent-team tools follow the same boundary. A thin TypeScript extension registers stable
model-facing tool names and forwards authenticated requests to the Rust supervisor over
loopback JSONL. The supervisor owns caller identity, topology, quotas, message routing,
and child process lifetimes; tool arguments are never trusted for team or level identity.

Only level-0 Pi processes use JSONL sessions. Subagents run with `--no-session`, so the
existing non-recursive session index and sidebar continue to contain level-0 conversations
only. See [Agent teams](agent-teams.md) for the routing and concurrency contract.

## Crate layering

The workspace is 12 crates in a strict acyclic layering enforced by Cargo dependency edges,
not convention. Upper layers physically cannot reach lower internals:

- **Foundation**: `pi-whim-core`, `pi-whim-tool-protocol`, `pi-whim-pi-rpc`, `pi-whim-theme`
- **Mid**: `pi-whim-persistence`, `pi-whim-catalog`, `pi-whim-one-shot-ai`, `pi-whim-agent-team`
- **Upper**: `pi-whim-runtime`, `pi-whim-engine`, `pi-whim-gpui`, `pi-whim-app`

A `cargo build` failure about an unknown crate usually means a layering boundary was
crossed; fix the dependency rather than adding a workspace-wide `path` hack.

## Threading

gpui is single-threaded; the engine produces results on a blocking crossbeam receiver.
`pi-whim-gpui/src/pump.rs` drains that receiver off the main thread and re-posts onto the
gpui async loop, so blocking work never stalls the view layer.

## Security model

Subagents run under macOS `sandbox-exec` with capability-based auth: a child receives an
ephemeral capability token, never a shared secret, and per-process API keys are injected by
environment variable name rather than value. Children run with `--no-session`, so the
non-recursive level-0 session index is unchanged. See [Agent teams](agent-teams.md).

## Model catalog

`pi-whim-core/build.rs` reads vendored provider JSON
(`vendor/pi-mono/packages/ai/src/providers/data`) and emits a `bundled_model_capabilities.rs`
table at build time. The build fails (panics with the offending path) if a real catalog
entry lacks the required `id`/`provider` fields, so upstream schema drift cannot silently
shrink the table. Dotfile manifests (`.manifest.json`) are filtered out as checksum metadata.

## Tool dispatch

Agent-team tool dispatch is currently a string-constant + match + `is_policy_tool` list
spread across five files; see [ToolSpec redesign](tool-spec-redesign.md) for the planned
single-registry collapse.
