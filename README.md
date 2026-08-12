# Pi-Whim

> A native Rust desktop workbench for the [Pi coding agent](https://pi.dev).

[English](#english) | [中文](#chinese)

---

## English

Pi-Whim is a macOS desktop application that wraps the Pi coding agent in a native
Rust UI built with [gpui](https://github.com/zed-industries/zed). It owns project
navigation, session history, credential storage, and presentation, while Pi
continues to own the agent loop and its JSONL session format.

### Features

- **Multi-agent teams** — subagents run under `sandbox-exec` with capability-based
  auth, per-process API keys, and a hierarchical permission model.
- **Provider agnostic** — add OpenAI, Anthropic, Google, or any OpenAI-compatible
  proxy. Discovery is optional; model IDs can be added manually.
- **Web search & fetch** — pluggable SearXNG engines and a bounded `fetch` tool
  (HTTP, TCP, UDP, WebSocket).
- **Supervisor hooks** — sandboxed external gate, transform, and observe commands
  for policy, telemetry, and audit.
- **Session persistence** — SQLite metadata index with JSONL session files owned
  by Pi under the macOS application support directory.
- **Keychain integration** — API keys are stored only in macOS Keychain and
  injected by environment variable name at process launch.

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

### Architecture

The workspace contains 15 crates in a strict acyclic layering enforced by Cargo
dependency edges:

| Layer | Crates |
|-------|--------|
| Foundation | `pi-whim-core`, `pi-whim-tool-protocol`, `pi-whim-pi-rpc`, `pi-whim-theme`, `pi-whim-signal`, `pi-whim-wait` |
| Hook foundation | `pi-whim-hook-host` |
| Mid | `pi-whim-persistence`, `pi-whim-catalog`, `pi-whim-one-shot-ai`, `pi-whim-agent-team` |
| Upper | `pi-whim-runtime`, `pi-whim-engine`, `pi-whim-gpui`, `pi-whim-app` |

The UI talks only to `AgentRuntime`; the engine produces results on a background
thread and re-posts onto the gpui async loop. Subagents run Pi with `--no-session`
so the level-0 session index remains non-recursive.

See [docs/architecture.md](docs/architecture.md) for the full design rationale.

### License

MIT

---

## 中文

Pi-Whim 是一个 macOS 桌面应用，使用 Rust 和 [gpui](https://github.com/zed-industries/zed)
构建，为 [Pi 编程助手](https://pi.dev) 提供原生桌面体验。它管理项目导航、会话历史、
凭据存储和界面展示，而 Pi 继续负责代理循环和 JSONL 会话格式。

### 功能特性

- **多代理团队** — 子代理在 `sandbox-exec` 下运行，使用基于能力的认证、进程级 API
  密钥注入和分层权限模型。
- **多提供商支持** — 可添加 OpenAI、Anthropic、Google 或任意 OpenAI 兼容代理。
  模型发现是可选的，支持手动添加模型 ID。
- **网络搜索与抓取** — 可插拔的 SearXNG 搜索引擎，以及受限的 `fetch` 工具
  （HTTP、TCP、UDP、WebSocket）。
- **监督者钩子** — 沙盒化的外部网关、转换和观察命令，用于策略、遥测和审计。
- **会话持久化** — SQLite 元数据索引 + Pi 管理的 JSONL 会话文件，
  存储在 macOS 应用支持目录下。
- **钥匙串集成** — API 密钥仅存储在 macOS 钥匙串中，在进程启动时以环境变量名注入。

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

### 架构

工作区包含 15 个 crate，通过 Cargo 依赖边强制实现严格的无环分层：

| 层 | Crate |
|-----|-------|
| 基础层 | `pi-whim-core`、`pi-whim-tool-protocol`、`pi-whim-pi-rpc`、`pi-whim-theme`、`pi-whim-signal`、`pi-whim-wait` |
| 钩子基础层 | `pi-whim-hook-host` |
| 中间层 | `pi-whim-persistence`、`pi-whim-catalog`、`pi-whim-one-shot-ai`、`pi-whim-agent-team` |
| 上层 | `pi-whim-runtime`、`pi-whim-engine`、`pi-whim-gpui`、`pi-whim-app` |

UI 仅与 `AgentRuntime` 通信；引擎在后台线程生成结果，然后重新投递到 gpui 异步循环。
子代理以 `--no-session` 运行 Pi，因此 level-0 会话索引保持非递归。

详见 [docs/architecture.md](docs/architecture.md)。

### 许可证

MIT