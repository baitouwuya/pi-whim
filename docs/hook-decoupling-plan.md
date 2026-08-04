# Hook 解耦/优化方案

> 基于对 `pi-whim` Hook 系统（15 事件 × 3 种类）及现有硬编码策略的全面分析，制定将内部逻辑迁移到可外部配置 Hook 体系的分阶段计划。

---

## 目录

1. [现状概览](#1-现状概览)
2. [Hook 系统能力边界](#2-hook-系统能力边界)
3. [可解耦的功能点清单](#3-可解耦的功能点清单)
4. [分阶段实施计划](#4-分阶段实施计划)
5. [各方案详细设计](#5-各方案详细设计)
6. [新增事件/扩展建议](#6-新增事件扩展建议)
7. [风险与注意事项](#7-风险与注意事项)

---

## 1. 现状概览

### 当前 Hook 架构

```
事件（15种）──┬── Gate（允许/拒绝）
              ├── Transform（修改参数）
              └── Observe（异步观测）
```

- **Rust 内置钩子**: `builtin.safety_floor`（编译时注册，不可禁用）
- **外部命令钩子**: 通过 `hooks.json` 配置，经 `sandbox-exec` 沙箱执行
- **执行顺序**: Rust 内置 → 全局 hooks.json → 项目 hooks.json（需审批）
- **安全约束**: 无网络、只读项目目录、空环境变量、输出 ≤64KiB

### 当前硬编码策略分布

| 模块 | 策略 | 所在文件 | 行数参考 |
|------|------|----------|---------|
| `bash_dispatch.rs` | bash 策略（deny/allow/blocked patterns） | `bash_dispatch.rs:147-187` | ~40 |
| `bash_dispatch.rs` | 命令白名单匹配、高危命令检测 | `bash_dispatch.rs:200-265` | ~65 |
| `bash_dispatch.rs` | Controlled 级别 sandbox 配置 | `bash_dispatch.rs:363-375` | ~12 |
| `process.rs` | 子代理沙箱 profile | `process.rs` | ~80 |
| `process.rs` | 环境变量过滤 | `process.rs` | ~50 |
| `lib.rs` (agent-team) | `ensure_tool_enabled` 工具权限检查 | `lib.rs` | ~80 |
| `lib.rs` (agent-team) | 审批流程（`request_bash_approval`） | `lib.rs` | ~100 |
| `lib.rs` (agent-team) | 消息路由与广播 | `lib.rs` | ~150 |
| `lib.rs` (agent-team) | Session 生命周期管理 | `lib.rs` | ~200 |
| `lib.rs` (agent-team) | Agent 团队拓扑限制（`max_depth`/`max_spawn`） | `lib.rs` | ~50 |
| `core/src/lib.rs` | `AgentTeamConfig` 默认值 | `lib.rs` | ~30 |
| `file_dispatch.rs` | 文件操作权限级别 | `file_dispatch.rs` | ~60 |

---

## 2. Hook 系统能力边界

### 支持的变更类型

| Hook 种类 | 可修改内容 | 不可修改内容 |
|-----------|-----------|-------------|
| **Gate** | 返回 `deny { message }` 或 `allow` | 无 |
| **Transform** (tool_dispatching) | `arguments` 对象整体替换 | 事件类型、上下文 |
| **Transform** (message_sending) | 仅 `message` 字段 | agent_id, level, 目标等 |
| **Transform** (agent_spawning) | 仅降低 `permission_level`、缩小 `enabled_tools`/`trusted_extensions` | task, identity, model, target |
| **Observe** | 无（fire-and-forget） | 无 |

### 当前限制

1. **Transform 不能扩大权限**——只能缩小
2. **无法添加新的事件字段**——payload 由 Rust 端定义
3. **Observe 无法影响流程**——纯观测
4. **命令钩子无网络、无环境变量**——不能调用外部 API
5. **输出 ≤64KiB**——Gate 响应必须精简
6. **超时 1-30000ms**——Gate 须快速响应

---

## 3. 可解耦的功能点清单

按优先级排序（P0=零代码变更，纯配置；P1=少量 Rust 变更+配置；P2=中等 Rust 变更；P3=架构级变更）：

| 优先级 | ID | 功能点 | 当前实现 | 目标 Hook | 变更量 |
|--------|----|--------|---------|-----------|--------|
| **P0** | F1 | Bash 命令过滤/审计 | `bash_dispatch.rs` 中 `command_allowed` + `PI_WHIM_BASH_BLOCKED_PATTERNS` | `tool_dispatching` Gate | 零 Rust 变更 |
| **P0** | F2 | 工具调用审计日志 | 仅 SQLite 元数据审计 | `tool_completed` Observe | 零 Rust 变更 |
| **P1** | F3 | 文件操作策略 | `file_dispatch.rs` 中权限级别检查 | `tool_dispatching` Gate | 零 Rust 变更 |
| **P1** | F4 | 可控命令白名单 | `bash_dispatch.rs` 中 `command_matches_allowlist` | `tool_dispatching` Transform | 零 Rust 变更 |
| **P1** | F5 | 子代理审批策略 | `lib.rs` 中 `ensure_tool_enabled` | `agent_spawning` Gate | 零 Rust 变更 |
| **P1** | F6 | 消息内容过滤 | `lib.rs` 中消息路由 | `message_sending` Gate/Transform | 零 Rust 变更 |
| **P2** | F7 | 沙箱 profile 扩展 | `process.rs` 中 `sandbox_profile` | `agent_spawning` Transform（需扩展 payload） | 少量 Rust + 配置 |
| **P2** | F8 | 环境变量策略 | `process.rs` 中 `filtered_environment` | `agent_spawning` Transform（需扩展 payload） | 少量 Rust + 配置 |
| **P2** | F9 | 跨会话/跨团队限制 | `lib.rs` 中消息路由 + 会话管理 | 新增 `cross_session_*` 事件或扩展 payload | 中等 Rust |
| **P3** | F10 | 模型选择过滤 | `lib.rs` 中模型检查 | `agent_spawning` Gate（需扩展 payload） | 中等 Rust |
| **P3** | F11 | 审批流程外部化 | `lib.rs` 中 `request_bash_approval` | `permission_resolving` Gate | 中等 Rust |

---

## 4. 分阶段实施计划

### 第一阶段：零 Rust 变更（P0-P1，纯配置）

**目标**: 不修改一行 Rust 代码，通过 `hooks.json` 配置实现安全策略增强。

#### 1.1 Bash 审计/限制（F1）

```json
{
  "version": 1,
  "hooks": [
    {
      "id": "bash-audit",
      "event": "tool_dispatching",
      "kind": "gate",
      "command": ["/absolute/path/to/bash-audit.sh"],
      "timeout_ms": 3000,
      "matcher": { "tools": ["bash"] }
    }
  ]
}
```

`bash-audit.sh` 示例：
```bash
#!/bin/bash
# 从 stdin 读取事件 payload
read -r INPUT
TOOL=$(echo "$INPUT" | jq -r '.payload.tool')
COMMAND=$(echo "$INPUT" | jq -r '.payload.arguments.command')

# 审计日志（写入临时目录）
echo "[$(date -u +%Y-%m-%dT%H:%M:%SZ)] tool=$TOOL agent=$(echo "$INPUT" | jq -r '.payload.agent_id') command=$COMMAND" >> "$TMPDIR/bash-audit.log"

# 策略：拒绝所有 curl/wget
if echo "$COMMAND" | grep -qE '^\s*(curl|wget)\s'; then
  echo '{"decision":"deny","message":"External downloads are prohibited by policy"}'
  exit 0
fi

# 允许
echo '{"decision":"allow"}'
```

#### 1.2 工具调用审计日志（F2）

```json
{
  "id": "tool-audit-log",
  "event": "tool_completed",
  "kind": "observe",
  "command": ["/absolute/path/to/tool-audit.sh"],
  "timeout_ms": 5000,
  "matcher": { "tools": ["bash", "write_file", "edit_file", "read_file"] }
}
```

#### 1.3 文件操作策略（F3）

```json
{
  "id": "file-protect",
  "event": "tool_dispatching",
  "kind": "gate",
  "command": ["/absolute/path/to/file-protect.sh"],
  "timeout_ms": 3000,
  "matcher": { "tools": ["read_file", "write_file", "edit_file"] }
}
```

`file-protect.sh` 示例：
```bash
#!/bin/bash
INPUT=$(cat)
PATH_FIELD=$(echo "$INPUT" | jq -r '.payload.arguments.path // .payload.arguments.target // ""')
AGENT_LEVEL=$(echo "$INPUT" | jq -r '.payload.agent_level')

# 保护关键路径
case "$PATH_FIELD" in
  /etc/*|/usr/*|/System/*|*/hooks.json|*/.git/*)
    echo '{"decision":"deny","message":"Protected path"}'
    exit 0
    ;;
esac

# 允许
echo '{"decision":"allow"}'
```

#### 1.4 可控命令白名单（F4）

通过 `tool_dispatching` Transform 修改 bash 命令参数：

```json
{
  "id": "command-allowlist",
  "event": "tool_dispatching",
  "kind": "transform",
  "command": ["/absolute/path/to/command-filter.sh"],
  "timeout_ms": 3000,
  "matcher": { "tools": ["bash"], "agent_levels": [1, 2, 3] }
}
```

#### 1.5 子代理审批策略（F5）

```json
{
  "id": "spawn-policy",
  "event": "agent_spawning",
  "kind": "gate",
  "command": ["/absolute/path/to/spawn-policy.sh"],
  "timeout_ms": 3000,
  "matcher": { "tools": ["spawn_agent"] }
}
```

### 第二阶段：少量 Rust 变更（P2）

**目标**: 扩展 Hook payload，使外部策略能控制沙箱、环境变量等。

#### 2.1 沙箱 Profile 扩展（F7）

**当前问题**:
- `process.rs` 中 `sandbox_profile()` 硬编码沙箱规则
- `bash_dispatch.rs` 中 `sandbox_profile()` 另有独立实现
- 无法通过 Hook 增加额外的文件读写路径或网络规则

**变更方案**:

1. 在 `agent_spawning` payload 中添加 `sandbox_extra_read_paths`、`sandbox_extra_write_paths`、`sandbox_deny_network` 字段
2. Transform Hook 可以修改这些字段
3. Rust 端在 `sandbox_profile()` 中应用这些扩展

```rust
// 在 core/src/lib.rs 中扩展
pub struct AgentSpawningPayload {
    // ... 现有字段
    pub sandbox_extra_read_paths: Vec<String>,
    pub sandbox_extra_write_paths: Vec<String>,
    pub sandbox_deny_network: bool,
}
```

**Rust 变更量**: ~50 行（payload 定义 + profile 合并）

#### 2.2 环境变量策略（F8）

**当前问题**:
- `filtered_environment()` 在 `process.rs` 中硬编码过滤规则
- 某些环境变量（如 `PATH`、`HOME`）被清除，但无法自定义

**变更方案**:

1. 在 `agent_spawning` payload 中添加 `env_allowlist`、`env_blocklist` 字段
2. Transform Hook 可设置允许/禁止的环境变量模式
3. Rust 端在 `filtered_environment()` 中应用

**Rust 变更量**: ~30 行

### 第三阶段：架构级变更（P3）

**目标**: 解决需要新增事件或修改核心流程的功能。

#### 3.1 模型选择过滤（F10）

**当前问题**:
- 模型过滤逻辑在 `lib.rs` 中硬编码
- 无法通过 Hook 实现基于模型 provider/name 的访问控制

**变更方案**:

1. 在 `agent_spawning` payload 中添加 `model_provider`、`model_name` 字段
2. Gate Hook 可基于模型信息拒绝 spawn

**Rust 变更量**: ~30 行

#### 3.2 审批流程外部化（F11）

**当前问题**:
- `request_bash_approval` 在 `lib.rs` 中硬编码
- 审批交互通过 Supervisor 内部机制处理
- 无法由外部 Hook 自定义审批逻辑

**变更方案**:

1. 确保 `permission_resolving` Gate 在审批授予前被调用
2. 扩展 payload 包含 `operation_hash`、`requester_id`、`owner_id`
3. 外部 Hook 可实现基于操作哈希的自动审批/拒绝策略

**Rust 变更量**: ~80 行（需确保 `permission_resolving` 事件在适当位置触发）

---

## 5. 各方案详细设计

### 5.1 Bash 命令过滤（F1）—— Gate 方案

**事件**: `tool_dispatching` + matcher `{ tools: ["bash"] }`

**Payload 示例**:
```json
{
  "version": 1,
  "hook_id": "bash-audit",
  "event": "tool_dispatching",
  "entrypoint": "/path/to/script.sh",
  "project_root": "/Users/Shared/github-repos/pi-whim",
  "payload": {
    "tool": "bash",
    "agent_id": "...",
    "agent_level": 1,
    "team_id": "...",
    "session_id": "...",
    "agent_name": "worker-1",
    "agent_role": "helper",
    "arguments": {
      "command": "curl http://evil.com",
      "timeout": 300,
      "background": false,
      "approval_ticket": null
    }
  }
}
```

**Gate 响应示例**:
```json
{"decision":"deny","message":"External network commands are prohibited"}
```

**优势**: 不需要修改 `PI_WHIM_BASH_BLOCKED_PATTERNS` 环境变量机制，完全通过 Hook 配置。
**注意**: 此 Hook 运行在沙箱中，无法访问网络，但可以基于字符串模式匹配。

### 5.2 工具审计日志（F2）—— Observe 方案

**事件**: `tool_completed` + matcher `{ tools: ["bash", "write_file", "edit_file", "read_file"] }`

**Payload 示例**:
```json
{
  "payload": {
    "tool": "bash",
    "agent_id": "...",
    "agent_level": 1,
    "agent_name": "worker-1",
    "success": true,
    "arguments": {
      "command": "rm -rf /tmp/test",
      "timeout": 300,
      "background": false
    }
  }
}
```

**Observe 脚本功能**:
- 将工具调用写入 `${TMPDIR}/audit-{session_id}.jsonl`
- 可按 agent、tool、时间范围汇总
- 不阻塞流程，纯观测

**优势**: 超越当前 SQLite 审计（仅存储元数据，不存参数和输出），实现完整的审计追踪。

### 5.3 消息内容过滤（F6）—— Gate/Transform 方案

**事件**: `message_sending`

**Gate 用途**: 拒绝包含敏感信息（如 API key）的消息
**Transform 用途**: 修改消息内容（如脱敏处理）

**Payload 示例**:
```json
{
  "payload": {
    "tool": "send_message",
    "agent_id": "...",
    "agent_level": 1,
    "arguments": {
      "target": "sibling-1",
      "message": "The API key is sk-xxxx"
    }
  }
}
```

**Gate 响应**:
```json
{"decision":"deny","message":"Message contains sensitive information"}
```

**Transform 响应**:
```json
{"arguments": {"target": "sibling-1", "message": "The API key is [REDACTED]"}}
```

### 5.4 沙箱 Profile 扩展（F7）—— Transform 方案

**当前硬编码 profile**（bash_dispatch.rs）:
```
(version 1) (deny default) (allow process*) 
(allow file-read* (subpath "/project") (subpath "/usr") (subpath "/bin") (subpath "/System")) 
(allow file-write* (subpath "/project")) 
(deny network*)
```

**目标**: 通过 Transform Hook 添加/修改沙箱规则

**需要 Rust 变更**:
1. 在 `agent_spawning` payload 中添加 `sandbox_extra_read_paths: Vec<String>` 和 `sandbox_extra_write_paths: Vec<String>`
2. 在 `process.rs` 的 `sandbox_profile()` 中合并这些路径
3. 在 `bash_dispatch.rs` 的 `sandbox_profile()` 中也合并

**Transform Hook 示例**:
```json
{
  "arguments": {
    "name": "worker",
    "task": "default task",
    "permission_level": "controlled",
    "sandbox_extra_read_paths": ["/var/log", "/opt/shared-data"],
    "sandbox_extra_write_paths": ["/tmp/worker-output"],
    "sandbox_deny_network": false
  }
}
```

### 5.5 审批流程外部化（F11）—— Gate 方案

**当前流程**:
1. Controlled 级别 agent 执行高风险命令
2. `request_bash_approval` 创建审批请求
3. 父 agent 收到 `InteractionCreated` 事件
4. 父 agent 调用 `resolve_interaction`
5. Rust 端检查 ticket 有效性

**目标**:
1. 在 `request_bash_approval` 创建审批请求前，调用 `permission_resolving` Gate
2. 外部 Hook 可自动批准/拒绝（基于命令哈希、agent 级别等）
3. 如果 Hook 拒绝，则不创建审批请求，直接返回错误

**Rust 变更**: 在 `request_bash_approval` 函数开始时调用 `permission_resolving` Gate

---

## 6. 新增事件/扩展建议

### 6.1 建议新增的事件

| 事件 | 种类 | 用途 | 优先级 |
|------|------|------|--------|
| `tool_enabling` | Gate | 在 `ensure_tool_enabled` 之前调用，决定是否允许启用某个工具 | P2 |
| `agent_heartbeat` | Observe | 定期报告 agent 状态（CPU、内存、运行时间） | P3 |
| `session_creating` | Gate | 在创建新会话前调用，可用于限制并发会话数 | P3 |
| `file_accessing` | Gate | 在文件操作前调用，提供更细粒度的路径匹配 | P1（替代 F3 的通用方案） |

### 6.2 建议扩展的 payload 字段

#### `agent_spawning` 扩展字段

```json
{
  "arguments": {
    "name": "worker",
    "task": "...",
    "role": "...",
    "model": "gpt-4",
    "provider": "openai",
    "permission_level": "controlled",
    "enabled_tools": ["read", "bash"],
    "trusted_extensions": [],
    // 新增字段
    "sandbox_extra_read_paths": [],
    "sandbox_extra_write_paths": [],
    "sandbox_deny_network": true,
    "env_allowlist": ["PATH", "HOME"],
    "env_blocklist": ["API_KEY", "SECRET"]
  }
}
```

#### `tool_dispatching` 扩展字段

```json
{
  "payload": {
    "tool": "bash",
    // ... 现有字段
    "arguments": {
      "command": "...",
      "timeout": 300,
      // 新增
      "request_timestamp": 1234567890,
      "agent_depth": 2,
      "team_size": 5
    }
  }
}
```

---

## 7. 风险与注意事项

### 7.1 安全风险

1. **Gate 拒绝导致的 DoS**: 配置错误的 Gate Hook 可能拒绝所有操作。需设置合理的 `timeout_ms` 和 fallback 策略。
2. **Transform 验证绕过**: Rust 端必须严格验证 Transform 返回值，确保不能越权扩大权限。
3. **Observe 数据泄露**: Observe 脚本可以写入临时目录，但临时目录对其他进程不可见。审计日志包含命令参数，需注意敏感信息。
4. **项目 Hook 审批绕过**: 项目级 Hook 需用户审批 SHA-256 指纹，变更后需重新审批。

### 7.2 性能风险

1. **Gate 延迟**: 每个 Gate 调用增加 1-3000ms 延迟。建议将 `timeout_ms` 设置为最小值 1000ms。
2. **Observe 队列满**: 当 `OBSERVE_QUEUE_CAPACITY=64` 满时，Observe 事件被丢弃。高并发场景需注意。
3. **SQLite 审计记录上限**: 10000 条记录上限，超出后丢弃旧记录。

### 7.3 兼容性风险

1. **Payload 版本**: 所有事件 payload 使用 `version: 1`。如果未来升级，旧 Hook 脚本需兼容。
2. **matcher 变化**: 当前 `matcher.tools` 和 `matcher.agent_levels` 为空表示"不限制"。如果未来增加新维度，需注意配置兼容性。
3. **sandbox-exec 依赖性**: 非 macOS 平台缺少 `sandbox-exec`，Gate 拒绝、Transform 保留原值、Observe 失败。

### 7.4 实施建议

1. **先 P0，后 P1，再 P2/P3**: 零 Rust 变更的配置优先实施，快速获得安全收益。
2. **每个 Hook 脚本都经过测试**: 使用 `invoke()` 函数的单元测试模式（参考 `hooks.rs` 测试）。
3. **审计和监控**: 第一个 Hook 应该是审计 Hook，记录所有其他 Hook 的决策。
4. **渐进式部署**: 先用 Observe 模式观察现有行为，确认后再切换到 Gate 模式。
5. **文档化**: 每个 Hook 脚本顶部需有完整的注释说明目的、事件、期望行为。

---

## 附录 A：完整配置示例

### 安全增强配置包

```json
{
  "version": 1,
  "hooks": [
    {
      "id": "audit-all-tools",
      "event": "tool_completed",
      "kind": "observe",
      "command": ["/etc/pi-whim/hooks/audit-tool.sh"],
      "timeout_ms": 5000,
      "matcher": {}
    },
    {
      "id": "block-dangerous-bash",
      "event": "tool_dispatching",
      "kind": "gate",
      "command": ["/etc/pi-whim/hooks/block-dangerous-bash.sh"],
      "timeout_ms": 3000,
      "matcher": { "tools": ["bash"] }
    },
    {
      "id": "protect-project-files",
      "event": "tool_dispatching",
      "kind": "gate",
      "command": ["/etc/pi-whim/hooks/protect-files.sh"],
      "timeout_ms": 3000,
      "matcher": { "tools": ["read_file", "write_file", "edit_file"] }
    },
    {
      "id": "limit-spawn-depth",
      "event": "agent_spawning",
      "kind": "gate",
      "command": ["/etc/pi-whim/hooks/limit-spawn.sh"],
      "timeout_ms": 1000,
      "matcher": { "tools": ["spawn_agent"] }
    },
    {
      "id": "sanitize-messages",
      "event": "message_sending",
      "kind": "transform",
      "command": ["/etc/pi-whim/hooks/sanitize-message.sh"],
      "timeout_ms": 2000,
      "matcher": {}
    }
  ]
}
```

### 脚本：`block-dangerous-bash.sh`

```bash
#!/bin/bash
set -euo pipefail

INPUT=$(cat)
COMMAND=$(echo "$INPUT" | jq -r '.payload.arguments.command // ""')
AGENT_LEVEL=$(echo "$INPUT" | jq -r '.payload.agent_level // 0')

# Level 0 (root) 不做限制
if [ "$AGENT_LEVEL" = "0" ]; then
  echo '{"decision":"allow"}'
  exit 0
fi

# 高危命令列表
DANGEROUS_PATTERNS=(
  "rm -rf /"
  "rm -rf ~"
  "> /dev/"
  "mkfs"
  "dd if="
  "chmod 777 /"
  "sudo"
  "curl.*|.*bash"
  "wget.*|.*bash"
  ":(){ :|:& };:"
)

for pattern in "${DANGEROUS_PATTERNS[@]}"; do
  if echo "$COMMAND" | grep -qiE "$pattern"; then
    echo "{\"decision\":\"deny\",\"message\":\"Command matches blocked pattern: $pattern\"}"
    exit 0
  fi
done

# 允许
echo '{"decision":"allow"}'
```

### 脚本：`sanitize-message.sh`

```bash
#!/bin/bash
set -euo pipefail

INPUT=$(cat)

# 提取 message 字段
MESSAGE=$(echo "$INPUT" | jq -r '.payload.arguments.message // ""')

# 脱敏处理：替换 API key 模式
SANITIZED=$(echo "$MESSAGE" | sed -E 's/(sk-[a-zA-Z0-9]{20,})/[API_KEY_REDACTED]/g')
SANITIZED=$(echo "$SANITIZED" | sed -E 's/([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})/[EMAIL_REDACTED]/g')

# 返回 Transform 结果（只修改 message）
echo "$INPUT" | jq --arg msg "$SANITIZED" '.payload.arguments.message = $msg' | jq '{arguments: .payload.arguments}'
```

---

## 附录 B：实施路线图

| 阶段 | 时间估计 | 交付物 | 验收标准 |
|------|---------|--------|---------|
| **Phase 0**: 准备 | 1 天 | 3 个示例 Hook 脚本 + 文档 | 可通过 `hooks.json` 加载并执行 |
| **Phase 1**: P0 配置 | 0.5 天 | Bash 审计 + 文件保护配置 | 无需 Rust 变更，安全策略生效 |
| **Phase 2**: P1 配置 | 0.5 天 | 子代理审批 + 命令白名单 | 外部化策略覆盖所有 Controlled agent |
| **Phase 3**: P2 Rust 变更 | 2 天 | 沙箱/环境变量 payload 扩展 | Transform Hook 可定制沙箱 |
| **Phase 4**: P3 架构变更 | 3 天 | 审批流程外部化 + 模型过滤 | 完整外部策略系统 |
| **Phase 5**: 测试 | 1 天 | 集成测试覆盖所有 Hook 场景 | `cargo test -p pi-whim-agent-team` 通过 |

---

*本文档基于 pi-whim Hook 系统 v1 设计，对应代码提交 `crates/pi-whim-agent-team/src/hooks.rs` 和 `crates/pi-whim-core/src/lib.rs`。*