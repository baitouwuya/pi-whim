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
  five touch points (constant, dispatch match, `is_policy_tool`, display translation,
  TS schema) into a single `ToolSpec` registry. Implementation is gated on the in-flight
  `resolve_session` tool landing.

### Changed

- `ProviderProtocol` now owns its discovery shape (`discover_endpoint`,
  `discovery_auth_headers`, `parse_models`) in
  `pi-whim-core/src/model_capabilities.rs`; `discover_models` is a generic ~15-line flow.
  Adding a protocol touches one `impl` block, not three branches in the call site.
  `pi-whim-core` gains a runtime `serde_json` dependency for `parse_models`.

### Fixed

- `pi-whim-core/build.rs` now fails the build (panicking with the offending catalog path)
  when a real model-catalog entry lacks the required `id`/`provider` fields, instead of
  silently skipping it. Dotfile manifests (`.manifest.json`) are filtered out as checksum
  metadata, not model records. Resolves the silent capability-table shrink risk on
  upstream schema drift.

### Documentation

- `docs/architecture.md` expanded with crate layering, threading (pump bridge), security
  model, model catalog generation, and tool-dispatch sections.
