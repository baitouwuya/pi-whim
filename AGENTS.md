# Development Rules

pi-whim is a Rust workspace (cargo, edition 2024) implementing a multi-agent,
capability-secured workbench that runs pi as a sandboxed subprocess. This file
governs AI-assisted (pi-driven) contributions.

## Conversational Style

- Keep answers short and concise.
- No emojis in commits, issues, PR comments, or code.
- Technical prose only, be direct.
- When the user asks a question, answer it first before running commands or editing.
- When responding to feedback or an analysis, explicitly state agreement or disagreement before describing what changed.

## Repository Layout

- `crates/` — 14 crates in a strict acyclic layering (see `docs/architecture.md`). Cargo
  dependency edges physically prevent upper layers from reaching lower internals; never add a
  `path =` dependency that crosses the documented boundaries.
  - Foundation: `pi-whim-core`, `pi-whim-signal`, `pi-whim-tool-protocol`, `pi-whim-pi-rpc`, `pi-whim-theme`
  - Mid: `pi-whim-hook-host`, `pi-whim-persistence`, `pi-whim-catalog`, `pi-whim-one-shot-ai`, `pi-whim-agent-team`
  - Upper: `pi-whim-runtime`, `pi-whim-engine`, `pi-whim-gpui`, `pi-whim-app`
- `vendor/pi-mono/` — read-only upstream reference (TypeScript AI agent). Model catalog JSON
  under `packages/ai/src/providers/data` is consumed by `crates/pi-whim-core/build.rs`. Never
  edit vendored files; resync the subtree instead.
- `extensions/` — TypeScript extensions bundled into host via `include_str!` in
  `crates/pi-whim-agent-team/src/session.rs`.
- `docs/` — architecture and feature docs. Update alongside code changes that shift boundaries.

## Code Quality (Rust)

- edition 2024; the project uses let-chains and other current-era features. Keep the toolchain
  consistent; do not introduce features that require an unstable toolchain flag.
- Read files in full before wide-ranging changes; do not rely on grep snippets for broad edits.
- Prefer `?` and structured error types over `unwrap()` / `expect()` in library crates. `expect()`
  is acceptable only with a message explaining the invariant, and only where a violation is truly
  impossible. A `panic!` in library code reachable at runtime needs a justifying comment.
- Run `cargo fmt` before committing; run `cargo clippy -p <crate>` and resolve new warnings before
  committing.
- After code changes: `cargo build` (or `-p <crate>`). For the affected crate: `cargo test -p
  <crate>`.
- Do not remove functionality or code that appears intentional without asking.
- Do not preserve backward compatibility unless asked.

## Build-time Generated Code

- `crates/pi-whim-core/build.rs` emits `bundled_model_capabilities.rs` from `vendor/pi-mono` JSON
  into `OUT_DIR`. Never hand-edit the generated output. If the upstream schema changes, the build
  script fails (panics with the offending catalog path) — update `build.rs` to match the new shape.
  Do not loosen the schema checks to silence a build failure; that defeats the drift guard.
- `crates/pi-whim-core/src/model_capabilities.rs` consumes the generated table. Keep
  `BundledCapability` fields in sync with `build.rs`'s `render_catalog` when adjusting the schema.

## Testing

- Tests run offline; never call real provider APIs, use real keys, or spend paid tokens.
- `FakeRuntime` (engine) fakes the runtime boundary for unit tests of agent-team / engine logic.
  Prefer it over spinning up the real pi subprocess.
- No faux model provider exists yet (tracked improvement). Do not introduce network calls in tests.
- Run the specific crate: `cargo test -p pi-whim-engine`. Avoid whole-workspace `cargo test`
  unless asked.

## Security (agent-team)

- Subagents run under `sandbox-exec` with capability-based auth and per-process key injection.
  Changes to `crates/pi-whim-agent-team/src/{process,routing,lib}.rs` that alter the command line,
  sandbox profile, or key/env passthrough must be reviewed against the threat model. Never widen
  sandbox permissions or broaden environment passthrough without explicit approval.
- `AgentPermissionPolicy` (runtime) caps child permissions; `trusted_extensions` is an allowlist
  seam. Treat any change that lets a child escape the policy as a security regression.

## Git

Multiple pi sessions may run in this cwd at once, each modifying different files. Git operations
that touch unstaged, staged, or untracked files outside your own changes will stomp on other
sessions' work.

Committing:

- Only commit files YOU changed in THIS session.
- Stage explicit paths (`git add <path1> <path2>`); never `git add -A` / `git add .`.
- Before committing, run `git status` and verify you are only staging your files.
- Message format: `{feat,fix,docs,refactor,chore}(<scope>): <message>` (multiple lines OK). Common
  scopes: `ai`, `agent-team`, `engine`, `gpui`, `core`, `catalog`, `runtime`.

Never run (destroys other agents' work or bypasses checks):

- `git reset --hard`, `git checkout .`, `git clean -fd`, `git stash`, `git add -A`, `git add .`,
  `git commit --no-verify`.

Rebase conflicts:

- Resolve only in files you modified.
- If a conflict is in a file you did not modify, abort and ask the user.
- Never force push.

## Pi-driven Specifics

- As a project that itself runs under pi, follow this file when contributing to pi-whim.
- The layering is enforced by Cargo. A `cargo build` failure about an unknown crate or dependency
  usually means you tried to reach across a boundary — adjust the dependency, do not add a
  workspace-wide `path =` hack to force resolution.
- Keep `Cargo.lock` committed; review lockfile diffs before pushing.
- `gpui` is pulled from the zed git source (no rev pin yet). If a build breaks after a `cargo
  update -p gpui`, pin a known-good rev in `crates/pi-whim-gpui/Cargo.toml`.

## User Override

If the user's instructions conflict with any rule in this document, ask for explicit confirmation
before overriding. Only then execute their instructions.
