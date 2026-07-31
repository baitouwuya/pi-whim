# ToolSpec 注册表重设计提案（#1）

> 状态：**已实施**（commits `96ebdbd` / `88e7a46` / `3b258e7`）。
> `resolve_session` 已落定并提交后，据此提案在干净基础上实施了 Rust 端单一注册（touch point #1/#3/#4）。#2 显示 match 按 option A 暂留；#5 TS 同步测试已加入。

## 一、现状与痛点

同一个工具名当前要在 **5 处**分别登记，加一个工具（如正在加的 `resolve_session`）要同时改这 5 处：

| # | touch point | 位置 | 形态 |
|---|---|---|---|
| 1 | 工具名常量 | `tool-protocol/src/lib.rs:8-36` | ~26 个 `pub const X_TOOL: &str = "x"` 字符串字面量 |
| 2 | 显示翻译 match | `engine/src/protocol.rs:100` `tool_call_report` + `:616` `agent_team_tool_summary` | 按 name 分支生成人可读的调用报告/结果摘要 |
| 3 | 核心 dispatch match | `agent-team/src/lib.rs:434` `dispatch_request_cancellable` | ~25 臂 `match tool_name { CONST => handler(...) }` |
| 4 | `is_policy_tool` 双重列举 | `agent-team/src/lib.rs:542` | `matches!(tool, CONST \| CONST \| ...)` ~22 个常量 |
| 5 | TS schema | `extensions/agent-team/index.ts` | 每工具 `pi.registerTool({name, label, description, parameters, execute})` |

### 三个具体痛点

1. **加工具 = 改 5 处，Rust↔TS 无编译期联系**。`tool-protocol` 的字符串常量与 `index.ts` 的 `registerTool` 名字靠人工保持一致，无任何编译器/测试约束。这正是 pi-mono 用 `ToolDefinition` 单一结构消除的重复。

2. **`is_policy_tool` 与 dispatch arm 是两份并列清单**。`is_policy_tool` 列了 ~22 个常量，dispatch match 有 ~25 臂，二者几乎一一对应却要分别维护——加工具漏改其一即静默 bug。

3. **policy gate 分散在两处，已出现不一致**（现状实测）：
   - `is_policy_tool` 含 `FETCH_TOOL`，但 FETCH arm 内又调 `ensure_tool_enabled` → **双重 gate**。
   - `WEB_SEARCH_TOOL` **不在** `is_policy_tool`，只在 arm 内 gate → 单 gate，但行为与 FETCH 不对称。
   - 根因：`is_policy_tool`（前置 gate 清单）与 arm 内 `ensure_tool_enabled`（就地 gate）是两套机制，各自维护必然漂移。

## 二、目标

单一注册：**加一个工具 = 一处声明**，dispatch 查表，policy gate 统一，`is_policy_tool` 消失。

## 三、设计

### 3.1 ToolSpec 结构

```rust
struct ToolSpec {
    name: &'static str,
    handler: fn(&HostContext, ActorId, &ToolRequest, Option<&AtomicBool>) -> Result<Content, HostError>,
    /// 取代 is_policy_tool + arm 内 ensure_tool_enabled 的双轨。
    permission: ToolPermission,
    /// 内部控制工具（_prompt_context / _take_peer_messages / _reset_team），
    /// 跳过 ensure_actor_active 前置。取代 dispatch 顶部的 matches! 特例。
    internal: bool,
}

enum ToolPermission {
    /// 无需审批（list_agents / read_messages / read_session 等只读）。
    None,
    /// 需 ensure_tool_enabled（spawn_agent / bash / edit / fetch / web_search 等）。
    NeedsApproval,
}
```

handler 签名统一为「原始 ToolRequest + cancelled」，每个工具在自己的 handler 内做 `parse_arguments::<T>()` 与业务，把现状 arm 内的 inline `.and_then` 链包成独立函数。这样 dispatch 只剩一次统一调用。

### 3.2 注册机制

**推荐：`const` 数组 + `iter::find`，零新依赖**：

```rust
const TOOLS: &[ToolSpec] = &[
    ToolSpec { name: SPAWN_AGENT_TOOL, handler: spawn_agent_handler, permission: NeedsApproval, internal: false },
    // ...
];
fn find_tool(name: &str) -> Option<&'static ToolSpec> {
    TOOLS.iter().find(|t| t.name == name)
}
```

- 工具数 ~26，线性扫 < 1µs，热路径可接受。
- `ToolSpec` 只含 `&'static str` / `fn` / `enum`，全部 `const` 可构造，无运行时构建开销。
- 零依赖：不引入 `phf` / `inventory` / `once_cell`。

备选：`std::sync::LazyLock<HashMap<&str, &ToolSpec>>`（edition 2024 的 std 已有 `LazyLock`），首次查表构建一次。仅当未来工具数显著增长时再换。

### 3.3 dispatch 查表化

`dispatch_request_cancellable` 从 ~110 行缩为：

```rust
let spec = match find_tool(&request.tool_name) {
    Some(spec) => spec,
    None => return ToolResponse::error(request.request_id, "unknown_tool", "unknown agent tool"),
};
// 统一前置：actor 活跃（内部工具豁免）
if !spec.internal {
    if let Err(error) = ensure_actor_active(host, actor_id) { return error_into(error); }
}
// 统一 policy gate：取代 is_policy_tool + arm 内 ensure_tool_enabled 双轨
if matches!(spec.permission, NeedsApproval) {
    if let Err(error) = ensure_tool_enabled(host, actor_id, spec.name) { return error_into(error); }
}
let result = (spec.handler)(host, actor_id, &request, cancelled);
```

三个前置 `if` 块、`is_policy_tool` 函数、~25 臂 match 全部消失。

### 3.4 显示翻译 match 的处理（touch point #2/#4）

`engine/protocol.rs` 的 `tool_call_report` / `agent_team_tool_summary` 是**面向用户的显示翻译**，与 dispatch（面向执行的 handler）职责不同。两个选项：

- **A（保守，推荐先做）**：保留这两个 match 不动。它们是纯显示逻辑，集中在一处、无双重列举问题，可接受。本提案先消除 #1/#3/#4 的执行层重复，显示层 match 留待后续。
- **B（彻底）**：给 `ToolSpec` 加 `call_report: fn(&str, Option<&Value>) -> String` 和 `summary: fn(&str, &Value) -> Option<String>` 两个字段，把显示翻译也并入注册表。代价：显示逻辑从集中 match 搬到分散字段，可读性略降；收益：加工具的显示也单点声明。

建议**先 A 后 B**：A 解决核心痛点且风险低；B 作为后续 polish。

### 3.5 TS schema 对齐（touch point #5）

Rust 是真相源，TS 是给 Pi 的描述。无编译期同步路径。三个层次：

1. **最小（立即做）**：加一个测试，断言「Rust `TOOLS` 的 name 集合 == `index.ts` `registerTool` 的 name 集合」。解析 TS 用简单正则或轻量 parser。漂移即测试失败。
2. **中等**：写 build 脚本从 Rust `ToolSpec` 生成 TS schema 片段（如 `tools.generated.ts`），`index.ts` import。代价高（要 TS codegen + 类型映射）。
3. **pi-mono 式**：pi-mono 是 TS 单语言，`ToolDefinition` 天然单一。pi-whim 跨 Rust/TS，无法完全照搬，但 #1 的核心是 Rust 端单一注册，TS 同步靠测试/CodeGen 渽补。

建议先做 #1（最小同步测试），#2 留待工具稳定。

## 四、迁移步骤（等 resolve_session 提交后）

1. 在 `agent-team/src/lib.rs`（或新 `tools.rs`）定义 `ToolSpec` / `ToolPermission` / `TOOLS` / `find_tool`。
2. 把第一个工具（如 `spawn_agent`）的 arm body 包成 `fn spawn_agent_handler(host, actor, req, cancelled) -> Result<_,_>`，登记进 `TOOLS`，从 dispatch match 删该 arm，从 `is_policy_tool` 删该常量。
3. `cargo build && cargo test -p pi-whim-agent-team` 验证单工具迁移不破行为。
4. 逐工具重复 2-3，每个工具一次提交（小步可回退）。
5. 全部迁移后，删 dispatch 的 match 骨架、`is_policy_tool` 函数、顶部 `matches!` 特例。
6. 把 `FETCH`/`WEB_SEARCH` 的 arm 内 `ensure_tool_enabled` 删掉（由 `permission: NeedsApproval` 统一 gate）——顺带修现状的双 gate 不一致。
7. 加 TS 同步测试（3.5 #1）。

## 五、与 resolve_session 的衔接

等对方 `resolve_session` 提交后：
- `RESOLVE_SESSION_TOOL` 常量已在 `tool-protocol/lib.rs:11`。
- dispatch 已有 `RESOLVE_SESSION_TOOL => resolve_session(...)` arm。
- `is_policy_tool` 已含 `RESOLVE_SESSION_TOOL`。
- `index.ts` 已 `registerTool("resolve_session", ...)`。
- `engine/protocol.rs` 已加两处显示翻译分支。

迁移时把 `resolve_session` 一并纳入 `TOOLS` 一处，删其 arm + is_policy 条目 + 显示 match 分支。届时它的 5 处登记收进 1 处，正好验证注册表的收益。

## 六、收益

| 指标 | 现状 | 重构后 |
|---|---|---|
| 加一个工具要改的文件 | 5 处（2 语言） | 1 处（Rust）+ TS 同步测试守护 |
| `is_policy_tool` 双重列举 | 是 | 消除（`permission` 字段） |
| policy gate 不一致（FETCH 双 gate / WEB_SEARCH 单 gate） | 存在 | 统一（`permission` 字段单点） |
| dispatch match 臂数 | ~25 | 0（查表） |
| `internal` 工具特例 | 顶部 `matches!` 硬编码 | `internal` 字段 |

## 七、风险

1. **失去 match 的穷尽性检查**：const 数组查表不像 `match` 有编译期穷尽保证。用测试补：`find_tool` 对每个常量都返回 Some。
2. **fn 指针表无内联**：dispatch 是热路径，`match` 可被编译器内联各 handler。`fn` 指针间接调用理论上慢一点，但 dispatch 非每帧热路径（每个工具调用一次），实测可忽略。
3. **TS schema 仍需人工或 CodeGen**：Rust 单一注册只解决 Rust 端，TS 端靠测试/CodeGen 守护，无法像 pi-mono 那样天然单一。
4. **迁移期间的双轨**：逐工具迁移时，`TOOLS` 表和残留 match 并存，需保证同一工具不同时出现在两处（测试守）。
