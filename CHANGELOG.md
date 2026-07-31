# Changelog

All notable changes to pi-whim are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- `AGENTS.md` governing pi-driven contributions: Rust workspace layering, offline
  testing with `FakeRuntime`, build-time generation guard, agent-team `sandbox-exec`
  security review triggers, and multi-session git safety rules (explicit-path staging,
  no `git add -A`).
- `docs/tool-spec-redesign.md`: design proposal for collapsing agent-team tool dispatch's
  five touch points into a single `ToolSpec` registry. **Implemented** in `96ebdbd`:
  the 27-arm dispatch match, the `is_policy_tool` allowlist, and scattered internal
  `ensure_tool_enabled` calls are replaced by one `const TOOLS` registry; adding a tool
  is one entry instead of five touch points. Drift guards added in `88e7a46`
  (registry coverage) and `3b258e7` (Rust<->TS name parity).
- `resolve_session` tool + peer-message inbox: a parent agent can resolve an exact
  session and surface durable messages injected by other running tasks. Adds
  `RESOLVE_SESSION_TOOL`, a `CrossTaskMessage` gpui card, queue-clear, and `pollPeerInbox`
  in the agent-team extension (`609e789`).

### Changed

- `ProviderProtocol` now owns its discovery shape (`discover_endpoint`,
  `discovery_auth_headers`, `parse_models`) in
  `pi-whim-core/src/model_capabilities.rs`; `discover_models` is a generic ~15-line flow.
  Adding a protocol touches one `impl` block, not three branches in the call site.
  `pi-whim-core` gains a runtime `serde_json` dependency for `parse_models`. Discovery
  methods are now unit-tested (`81c5495`).
- Agent-team tool dispatch collapsed into a `ToolSpec` registry (`96ebdbd`).
  `ToolPermission::NeedsApproval` is the single source of truth for policy gating,
  fixing the prior asymmetry where `FETCH` was double-gated (`is_policy_tool` +
  arm-internal) while `WEB_SEARCH` was single-gated (arm-internal only). Both are now
  front-gated exactly once. `read_file`/`write_file`/`edit_file` drop their redundant
  internal `ensure_tool_enabled` calls.

### Fixed

- `pi-whim-core/build.rs` now fails the build (panicking with the offending catalog path)
  when a real model-catalog entry lacks the required `id`/`provider` fields, instead of
  silently skipping it. Dotfile manifests (`.manifest.json`) are filtered out as checksum
  metadata, not model records. Resolves the silent capability-table shrink risk on
  upstream schema drift.

### Documentation

- `docs/architecture.md` expanded with crate layering, threading (pump bridge), security
  model, model catalog generation, and tool-dispatch sections.
