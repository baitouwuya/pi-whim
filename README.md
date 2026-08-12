# Pi-Whim

> A native Rust desktop workbench for the [Pi coding agent](https://pi.dev).

[English](#english) | [中文](#chinese)

---

## English

Pi-Whim is a macOS desktop application built with Rust and [gpui](https://github.com/zed-industries/zed).
It wraps the Pi coding agent in a native UI while extending it with a multi-agent team system,
a coordinated file access layer, and a supervisor-level hook engine.

### Key Features

#### Multi-Agent Team Architecture

Pi-Whim lets you spawn sub-agents that form a hierarchical team. Each conversation is a
**level-0 agent** that owns one runtime team; children can spawn further agents up to a
configurable depth (max 8).

- **Sandboxed isolation** — Subagents run under macOS `sandbox-exec` with an empty environment.
  They have no network access, no provider keys, and no supervisor endpoint unless explicitly
  granted.
- **Capability-based auth** — Each child receives an ephemeral capability token (never a shared
  secret). API keys are injected by environment variable name at process launch, never by value.
- **Hierarchical routing** — Agents can message siblings, direct parent, or direct children.
  Cross-team and cross-level messages are denied by the supervisor. Level-0 sessions can
  exchange peer messages across different teams.
- **Quota & lifecycle** — Configurable max depth and max agents per level. A parent finishing
  normally does not interrupt running children. `interrupt_agent` cascades termination through
  the entire subtree.
- **Ephemeral children** — Subagents run `--no-session`; their transcripts stay in the
  supervisor catalog and never appear in the sidebar. Final outputs remain in the level-0 tool
  result.

#### Coordinated File Access

The Rust file coordinator (`read` / `write` / `edit`) replaces Pi's built-in file tools with
automatic ordering and conflict detection:

- **File lanes** — Different files execute in parallel; same-file reads execute together;
  queued writers run FIFO before later reads. No manual locking needed.
- **Snapshot-based conflict detection** — Every write carries a `snapshot_id` from the
  preceding read. A stale agent cannot silently overwrite a newer change; the coordinator
  returns a `file_conflict` with the diff summary.
- **Adaptive reading** — Small files return exact source; large files return a structured
  adaptive view with location-preserving compresssion. Truncated results include a
  `<read_metadata>` JSON block with `next_cursor` for continuation.
- **Path safety** — All file paths are resolved and canonicalized against the project root,
  including symlink targets. `bash` remains intentionally available as an escape hatch.

#### Supervisor Hooks & Signals

A Rust-level hook system extends the supervisor control plane, running before or after every
agent tool dispatch:

- **Three hook kinds** — `gate` (deny or allow), `transform` (rewrite arguments monotonically),
  and `observe` (best-effort telemetry).
- **Eleven+ events** — `tool_dispatching`, `agent_spawning`, `message_sending`,
  `permission_resolving`, `interaction_created`, `interaction_resolved`, `agent_started`,
  `agent_finished`, `tool_completed`, `message_delivered`, `session_expired`, and more.
- **Declared in JSON** — Global `~/Library/Application Support/pi-whim/hooks.json` and
  per-project `.pi-whim/hooks.json` (SHA-256 fingerprint approved in Settings).
- **Sandboxed execution** — Command hooks run under `sandbox-exec` with empty environment,
  project read access, and a per-invocation temporary directory. Gate hooks fail closed on
  launch errors.
- **Signal bridge** — Sanitized hook events and application signals are exported to the
  session-owned wait hub, so agents can `wait` for hook outcomes, signal matchers, and
  background completions.
- **Built-in audit** — Each invocation records hook ID, event, outcome, duration, and
  timestamp to SQLite. Arguments, message bodies, and credentials are never persisted.

### Prerequisites

- Rust stable (edition 2024)
- Node.js 22.19+ and Bun
- macOS (the UI uses gpui, which is macOS-native)

### Quick Start

```bash
git submodule update --init --recursive
cargo run -p xtask -- pi-build
cargo run -p pi-whim-app
```

Set `PI_WHIM_PI_BIN` to a standalone Pi binary for development. Otherwise the
application looks for the Pi build under `vendor/pi-mono`.

### Crate Architecture

The workspace contains 15 crates in a strict acyclic layering enforced by Cargo
dependency edges:

| Layer | Crates |
|-------|--------|
| Foundation | `pi-whim-core`, `pi-whim-tool-protocol`, `pi-whim-pi-rpc`, `pi-whim-theme`, `pi-whim-signal`, `pi-whim-wait` |
| Hook foundation | `pi-whim-hook-host` |
| Mid | `pi-whim-persistence`, `pi-whim-catalog`, `pi-whim-one-shot-ai`, `pi-whim-agent-team` |
| Upper | `pi-whim-runtime`, `pi-whim-engine`, `pi-whim-gpui`, `pi-whim-app` |

The UI talks only to `AgentRuntime`; the engine produces results on a background
thread and re-posts onto the gpui async loop.

See [docs/architecture.md](docs/architecture.md) for the full design rationale,
[docs/agent-teams.md](docs/agent-teams.md) for team topology and routing,
[docs/hooks.md](docs/hooks.md) for the hook system, and
[docs/wait.md](docs/wait.md) for the unified wait tool.

### License

MIT

---

## 中文

Pi-Whim 是一个用 Rust 和 [gpui](https://github.com/zed-industries/zed) 构建的 macOS
桌面应用，为 [Pi 编程助手](https://pi.dev) 提供原生界面，同时扩展了多 agent 团队系统、
协调文件访问层和监督者级钩子引擎。

### 核心特性

#### 多 Agent 团队架构

Pi-Whim 允许你创建子 agent，形成层次化团队。每个对话是一个 **level-0 agent**，
拥有一个运行时团队；子 agent 可以继续创建下一级，最多可达 8 层。

- **沙箱隔离** — 子 agent 在 `sandbox-exec` 下运行，环境为空。无网络、无提供商密钥、
  无监督者端点，除非显式授权。
- **基于能力的认证** — 每个子 agent 收到一次性能力令牌（非共享密钥）。API 密钥通过
  环境变量名注入，绝不传递值。
- **层次化路由** — Agent 可向兄弟、父或子发送消息。跨团队、跨层级消息被监督者拒绝。
  Level-0 会话可在不同团队间交换消息。
- **配额与生命周期** — 可配置最大深度和每层最大 agent 数。父 agent 正常结束不会中断
  正在运行的子 agent。`interrupt_agent` 会级联终止整个子树。
- **临时子 agent** — 子 agent 以 `--no-session` 运行，其对话记录留在监督者目录中，
  不会出现在侧边栏。最终输出保留在 level-0 的工具结果中。

#### 协调文件读写

Rust 文件协调器（`read` / `write` / `edit`）取代 Pi 内置的文件工具，提供自动排序
和冲突检测：

- **文件通道** — 不同文件并行执行；同一文件的读操作合并执行；写操作 FIFO 排队，
  之后才处理后续读请求。无需手动加锁。
- **快照冲突检测** — 每次 `write` 携带前一次 `read` 返回的 `snapshot_id`。过时的
  agent 无法静默覆盖更新的更改；协调器返回 `file_conflict` 及差异摘要。
- **自适应读取** — 小文件返回精确源码；大文件返回结构化的自适应视图，保留位置信息
  并压缩内容。截断结果包含 `<read_metadata>` JSON 块和 `next_cursor` 用于续读。
- **路径安全** — 所有文件路径经规范化处理，限制在项目根目录内（含符号链接目标）。
  `bash` 仍保留作为有意为之的逃生门。

#### 监督者钩子与信号

Rust 层面的钩子系统扩展了监督者控制平面，在每个 agent 工具分发前后运行：

- **三种钩子** — `gate`（拒绝或允许）、`transform`（单调改写参数）、`observe`（尽
  力而为的遥测）。
- **十多种事件** — `tool_dispatching`、`agent_spawning`、`message_sending`、
  `permission_resolving`、`interaction_created`、`interaction_resolved`、`agent_started`、
  `agent_finished`、`tool_completed`、`message_delivered`、`session_expired` 等。
- **JSON 声明** — 全局 `~/Library/Application Support/pi-whim/hooks.json` 和项目内
  `.pi-whim/hooks.json`（SHA-256 指纹需在设置中审批）。
- **沙箱执行** — 命令钩子在 `sandbox-exec` 下运行，空环境、项目只读、每次调用独立
  临时目录。Gate 钩子启动失败时默认拒绝操作。
- **信号桥** — 经过净化的钩子事件和应用信号被导出到会话的 wait hub，agent 可以
  `wait` 等待钩子结果、信号匹配和后台完成事件。
- **内置审计** — 每次调用记录钩子 ID、事件、结果、耗时和时间戳到 SQLite。参数、
  消息体和凭据永不持久化。

### 环境要求

- Rust stable（edition 2024）
- Node.js 22.19+ 和 Bun
- macOS（UI 基于 gpui，仅支持 macOS）

### 快速开始

```bash
git submodule update --init --recursive
cargo run -p xtask -- pi-build
cargo run -p pi-whim-app
```

开发时可设置 `PI_WHIM_PI_BIN` 指向独立的 Pi 二进制文件。否则应用会在 `vendor/pi-mono`
下查找 Pi 构建产物。

### Crate 架构

工作区包含 15 个 crate，通过 Cargo 依赖边强制实现严格的无环分层：

| 层 | Crate |
|-----|-------|
| 基础层 | `pi-whim-core`、`pi-whim-tool-protocol`、`pi-whim-pi-rpc`、`pi-whim-theme`、`pi-whim-signal`、`pi-whim-wait` |
| 钩子基础层 | `pi-whim-hook-host` |
| 中间层 | `pi-whim-persistence`、`pi-whim-catalog`、`pi-whim-one-shot-ai`、`pi-whim-agent-team` |
| 上层 | `pi-whim-runtime`、`pi-whim-engine`、`pi-whim-gpui`、`pi-whim-app` |

UI 仅与 `AgentRuntime` 通信；引擎在后台线程生成结果，然后重新投递到 gpui 异步循环。

详见 [docs/architecture.md](docs/architecture.md)（架构设计）、
[docs/agent-teams.md](docs/agent-teams.md)（团队拓扑与路由）、
[docs/hooks.md](docs/hooks.md)（钩子系统）和
[docs/wait.md](docs/wait.md)（统一等待工具）。

### 许可证

MIT