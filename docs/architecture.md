# Architecture decisions

Pi-Whim has one runtime boundary: the UI talks only to `AgentRuntime`. The first
implementation launches Pi in `--mode rpc` and exchanges strict LF-delimited JSONL.
This prevents process handling, Pi wire types, and future tool transport concerns
from leaking into the egui layer.

The SQLite database is an index, not the source of conversation truth. Pi owns JSONL
sessions under the Pi-Whim application support directory. The index stores enough
metadata for the project sidebar to load without parsing every conversation first.

`pi-whim-tool-protocol` intentionally has no active host. When a Pi fork needs a
Rust implementation for a built-in tool, a thin TypeScript adapter can preserve the
existing Pi tool name and schema while forwarding requests to this versioned protocol.
No GUI, session, or persistence API needs to change.
