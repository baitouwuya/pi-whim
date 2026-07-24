# Architecture decisions

Pi-Whim has one runtime boundary: the UI talks only to `AgentRuntime`. The first
implementation launches Pi in `--mode rpc` and exchanges strict LF-delimited JSONL.
This prevents process handling, Pi wire types, and future tool transport concerns
from leaking into the egui layer.

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
