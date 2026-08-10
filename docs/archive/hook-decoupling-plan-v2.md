> **Archived:** This implementation plan has been completed and is retained for historical context. See [Architecture decisions](../architecture.md) and [Hooks](../hooks.md) for the current design.

# Hook 解耦/优化方案 v2（架构审查版）

> 本文档审查并替代 `hook-decoupling-plan.md`。目标不是将所有硬编码都变成 Hook，而是在不削弱 capability、沙箱和拓扑安全不变量的前提下，把**可由管理员配置的附加策略**外置。

## 1. 审查结论

原方案的方向（以 Hook 承载附加审计和组织策略）合理，但把 Hook 误认为“15 个事件均支持 Gate / Transform / Observe”的通用策略引擎，并据此高估了零代码可解耦范围。当前实现是**按事件阶段严格限定 kind**：

| 事件类别 | 实际支持的 kind |
| --- | --- |
| `tool_dispatching`、`agent_spawning`、`message_sending` | Gate；并可使用 Transform |
| `permission_resolving` | Gate，且只在批准操作前执行 |
| 其余 11 个生命周期/完成事件 | Observe |

`HookConfig::validate` 会拒绝不符合上述阶段的组合。因此不存在“15 × 3”个可用组合。

此外，Hook 的正确安全定位为：

1. **不可替代底线控制。** typed handler、`AgentPermissionPolicy`、能力令牌验证、文件 canonicalization、审批 ticket 绑定、路由可见性、资源上限和 sandbox profile 必须继续由 Rust 强制执行。
2. **Gate 可附加拒绝，不能授权。** 它适合组织的 deny 规则、审计前置检查和配额限制；不能成为授予文件/网络/模型/工具权限的依据。
3. **Transform 只适合有单调性证明的收缩。** 当前仅对 spawn 做了权限/allowlist 收缩验证。`tool_dispatching` Transform 可以替换整个参数对象，不能被当成可信的安全过滤器。
4. **Observe 不是可靠审计存储。** 它异步、队列满会丢失，且 hook 的私有临时目录会在调用后删除。

结论：原方案中的 F1、F3、F5、F6 可作为附加 Gate 配置推进；F10 和 F11 已部分或完全存在；F2、F4、F7、F8 的设计需重做。原 P0–P3 与 Phase 0–5 的排序和工期均需调整。

## 2. 当前架构的关键事实与边界

### 2.1 调度顺序

对外部工具请求，监督器先验证能力与工具名、确认 agent 活跃，随后执行：

1. `tool_dispatching` / `agent_spawning` / `message_sending` Hook；
2. `ensure_tool_enabled`（按 `AgentPermissionPolicy.enabled_tools`）；
3. 具体 typed handler 和更细的安全检查。

因此 Hook 能看到原始 JSON arguments，但一般发生在参数反序列化、路径 canonicalization、文件审批 hash 和 bash 命令解析之前。Hook 的 allow 绝不跳过后续检查；拒绝才具有安全效果。

### 2.2 各类 Transform 的真实限制

- `message_sending`：只能改 `message`；target 等字段必须逐字不变。
- `agent_spawning`：只能降低显式 `permission_level`，并把显式、非空的 `enabled_tools` / `trusted_extensions` 缩为子集；不能修改 task、name、role、model、provider、target 或添加新字段。
- `tool_dispatching`：目前可以替换完整 arguments，之后由 handler 重新解析。这是功能性变换能力，不是权限收缩机制；例如不应依赖它把危险命令“改安全”。

内置 `builtin.safety_floor` 在命令 Hook 前执行，随后 handler 会再做类型与业务校验。项目级 Hook 还须经过用户指纹批准，并在每次执行前验证 entrypoint 内容；这不适用于全局管理员 Hook 的信任模型。

### 2.3 Hook 运行环境与审计边界

Hook 通过 `sandbox-exec` 运行，`env_clear` 后仍注入受控 `PATH` 和私有 `TMPDIR`；无 agent capability、provider key、监督器 endpoint 或网络权限。Hook 可读项目、entrypoint 所在目录及系统运行时目录，并可执行受 profile 约束的子进程，只可写私有临时目录，目录在调用结束后删除。SQLite 内置审计仅保存 hook 元数据（结果、耗时、截断、revision），**不保存 arguments、消息、凭据或 hook 原始输出**。项目 Hook 虽会复验入口文件并执行快照，但入口可加载的同目录 helper/解释器依赖不在入口内容指纹覆盖范围；安全关键 Hook 应部署在项目不可写的可信目录，或对整个可执行闭包做不可变校验。另注意：受限子 agent 为模型推理实际允许 outbound TCP；不能把 child sandbox 误建模为“完全无网络”。

## 3. 原功能点逐项复核

| ID | 原结论 | 复核结论 | v2 处置 |
| --- | --- | --- | --- |
| F1 bash 过滤 | P0、可完全通过 Gate 外置 | Gate 可作**附加拒绝**，但无法取代 `PI_WHIM_BASH_POLICY`、blocked patterns、受控 argv allowlist、shell-composition 禁止、审批和 sandbox。现有 bash 规则仍必须保留。 | P0：提供示例 Gate；明确为 deny-in-depth。 |
| F2 工具审计 | P0 Observe、可记录命令参数 | 不可行。`tool_completed` payload 只有公共上下文、`tool`、`success`，没有 arguments/输出；私有 TMPDIR 也不会留下审计日志。Observe 可能丢失。 | P1：新增受控审计 sink 或受限的脱敏 completed payload；不能用 shell 脚本替代。 |
| F3 文件策略 | P1（实际零代码） | 对 `read` / `write` / `edit` 的 `tool_dispatching` Gate 可做原始路径 deny。原文的 `read_file` 等名称不匹配协议。不能替代 canonicalization、symlink 防护、ReadOnly/Controlled scope、审批 ticket。 | P0：仅附加路径 deny，脚本必须 fail closed。 |
| F4 命令白名单 | P1 Transform | 不可作为白名单实现。Transform 无法让 Rust 将“allow”变成跳过审批，且完整 arguments 替换可能改 timeout/background/ticket，扩大可用操作面或造成 DoS。 | 删除。命令 allowlist 仍是 `AgentPermissionPolicy.command_allowlist`；组织黑名单使用 F1 Gate。 |
| F5 子代理审批/工具策略 | P1 Gate | `agent_spawning` Gate 确实可拒绝请求，但 `ensure_tool_enabled` 是调用者工具访问控制，不是子代理审批策略；Gate 也不改变 `max_depth` / `max_spawn` 等硬边界。 | P0：改名为“spawn 附加准入”；保留 Rust policy。 |
| F6 消息内容过滤 | P1 Gate/Transform | 技术上可行。Gate/Transform 在路由前执行，Transform 受 target 不可变校验。仍不可取代长度上限、sender 身份和拓扑路由。 | P0：脱敏 Transform 或敏感内容 Gate；需测试 Unicode 与误杀。 |
| F7 沙箱 profile 扩展 | P2、允许增加路径/关闭网络 | 原设计不可接受。让外部 Hook 添加写路径或将 `sandbox_deny_network=false` 会直接把不可信/项目 Hook 变成权限扩大通道；并且当前 spawn Transform 会拒绝这些新增字段。bash 与 child 的 sandbox profile 亦不是同一条执行路径。 | P2：仅设计**单调收紧**的 supervisor 配置（例如 deny-network、额外 deny path），不提供 extra allow path。 |
| F8 环境变量策略 | P2、allow/block list | 原设计不可接受。`filtered_environment` 刻意只传 provider 所需单一 key、`HOME`、`PATH`、隔离的配置目录。allowlist 可能恢复敏感父环境，blocklist 还会破坏模型认证。 | P2：仅允许可信全局配置按名称移除非关键变量；不得注入、枚举或回传秘密。 |
| F9 跨会话/团队限制 | P2、新事件 | 现有 `message_sending` 已携带 team/session/parent context，可加拒绝规则；真正跨会话可见性由 routing/capability 强制，不能交给 Hook。 | P1：先定义具体缺口；只在已有 Gate 无法表达时新增窄事件。 |
| F10 模型选择过滤 | P3、扩展 payload | 请求 arguments 已含 `provider`/`model`，所以可用 `agent_spawning` Gate 拒绝显式选择。缺省模型须在 `effective_child_policy` / `delegated_models` 解析后才能准确判断。 | P1：若需要对最终模型决策审计/拒绝，新增解析后的 `agent_launching` Gate，payload 只读且不可 Transform。 |
| F11 审批流程外部化 | P3、需新增调用 | 已实现：`resolve_interaction_for_owner` 在 approval 的 `approve` 前调用 `permission_resolving`，payload 已含 request/requester/owner、title、operation_hash、decision；拒绝/取消不可被阻止。它是 deny-only，不能自动批准。 | P0：文档和示例即可；不改为自动批准。 |

## 4. 安全设计原则与必须保留的硬编码

下列项目属于安全参考监控器（reference monitor），不纳入“可外部化替换”范围：

- capability 与请求认证、agent 活跃状态、root 不可自审批；
- `AgentPermissionPolicy` 的上限合并、`enabled_tools`、`trusted_extensions`、委派模型的子集关系；
- `max_depth`、`max_spawn`、名字唯一性及可见性/直系消息路由；
- 文件路径解析、canonicalization、symlink 逃逸防护、文件锁与 revision 冲突控制；
- controlled bash 的 argv 解析、系统可执行文件解析、组合语法禁止、审批 ticket/hash/TTL、后台进程限额；
- 子 agent 的最小环境、provider 凭据选择、sandbox-exec 可用性（不可用时受限 agent fail closed）；
- Hook manifest schema、项目 Hook 批准/复验、输出限制和 Gate 失败关闭。

还应在 `AgentSupervisor::start` 的嵌入式构造边界重复执行 `HookConfig::validate()`，不能只依赖应用层 loader；并为 Transform 响应定义严格 schema（非 object/无 `arguments` 当前会被记作成功 no-op），避免配置错误被静默掩盖。

尤其禁止以下设计：

1. 项目 Hook 或任意 command Hook 扩展文件读写白名单、trusted extension、环境变量、网络或模型权限；
2. Hook 的 `allow` 结果绕过 Rust 审批、沙箱或 capability 校验；
3. 使用 Transform 给 bash 注入/替换 `approval_ticket`，或以 Transform 作为命令净化器；
4. 将完整命令、文件内容、消息或凭据写入无访问控制的审计文件；
5. 将 Observe 作为合规不可丢失审计链。

## 5. 遗漏点与建议的可解耦边界

### 5.1 可立即利用的已有事件

- **网络工具准入**：对 `fetch`、`web_search` 的 `tool_dispatching` Gate 按 agent level、团队或请求形状附加拒绝。不能取代 fetch URL/协议验证和 search credential 隔离。
- **高代价/破坏性工具准入**：对 `write`、`edit`、`bash`、`spawn_agent`、`interrupt_agent`、`reset` 进行组织级 deny。工具名必须取自协议常量，不能使用文档中的旧别名。
- **消息数据治理**：`message_sending` Gate/Transform 可阻止或脱敏疑似密钥、个人信息；建议默认拒绝高置信度秘密而非试图对所有秘密做正则替换。
- **生命周期遥测**：`agent_started`、`agent_finished`、`interaction_created`、`interaction_resolved` 等 Observe 可提供 best-effort 指标。应由受控 collector 接收，而非期待 Hook 自己保留文件。
- **审批约束**：`permission_resolving` 可实施操作 hash deny-list、变更窗口和分级批准限制；仅能否决已经由合法 owner 发出的 approve。

### 5.2 真正需要新增的窄接口

按需新增，而非先增加泛化 payload：

1. **可靠审计 sink（P1）**：由 supervisor 向 SQLite/应用审计服务写结构化、字段白名单且可脱敏的记录；明确 retention、访问控制、失败策略。若需命令审计，记录 hash、分类和经策略批准的摘要，而非默认明文。`tool_completed` 只覆盖已进入 handler 的成功/失败结果，不能作为所有尝试的总账；若合规需要，新增 `tool_attempted` / `tool_denied`（含 policy、validation、Hook deny 分类）或明确重定义审计语义。
2. **`agent_launching` Gate（P1/P2）**：在模型、最终有效 policy、委派模型已解析但尚未启动进程时触发。只读 payload；只允许 deny。这样才能正确按最终模型或最终权限拒绝，而不暴露 provider key。
3. **单调收紧的 sandbox restriction（P2）**：若有业务需求，只接受 supervisor 可信的静态配置，表达“禁用网络”“附加 deny 路径/能力”；配置与最终 profile 均需 canonicalize，并取与原规则的交集。不要把 allow path、write path 或网络开启暴露给 Hook。
4. **资源配额事件（P2）**：只有当现有 agent context 不足时，增加只读的 `resource_reserving` Gate，用于 spawn 或后台进程的组织级配额。Rust 的全局硬上限永远优先。

不建议新增 `file_accessing`：通用 `tool_dispatching` 已能执行附加 deny；若需要 canonical path，应在 Rust 解析后提供只读 `file_authorizing` Gate，且 Gate 不能改变 scope/ticket。`agent_heartbeat` 先以 supervisor 原生遥测实现；高频 command Hook 会造成额外进程开销和可丢失队列压力。`session_creating` 只有明确存在独立会话创建路径后再设计。

## 6. 改进后的优先级与实施路线

### P0：现有能力的安全使用（约 2–3 人日）

交付：

- 修正文档的事件/kind 矩阵、工具名、payload 与失败模型；
- 提供经过测试的全局 Hook 示例：bash 附加 deny、文件原始路径 deny、spawn 附加准入、消息敏感信息 Gate/Transform、`permission_resolving` deny；
- 建立“先 Observe 指标、再 Gate”上线清单，但不可把 Observe 当审计保证；
- 对每个 Gate 做 timeout、sandbox 不可用、非法 JSON、hook 自身失败的 fail-closed 演练。

验收：不改 Rust 也不宣称替代底线控制；`cargo test -p pi-whim-agent-team` 的现有 Hook 测试通过，并为示例协议增加 fixture 测试。

### P1：补齐正确的审计和最终决策点（约 4–6 人日）

交付：可靠审计 sink 的数据分类设计与实现；按需求增加只读 `agent_launching` Gate（或确认现有 spawn Gate 已足够）；为 F9 的具体跨 session 需求设计最小事件。

验收：审计记录有明确脱敏、retention、访问控制和丢失语义；最终 provider/model/policy 的 Gate 测试覆盖显式与默认选择；不增加 secret payload。

### P2：单调收紧的运行时限制（约 5–8 人日）

交付：经 threat-model 评审的 sandbox restriction / environment drop-only 配置；分别覆盖 child process（其基线允许模型所需 outbound TCP）和 controlled bash（基线 deny network）两个 profile，绝不假设二者共用实现。路径进入 sandbox DSL 前必须 canonicalize、验证位于预先授权根内、拒绝控制字符与危险转义，并采用可靠的 DSL quoting。

验收：形式化或表格证明每个配置只能减少原权限；测试证明无法增加读写目录、网络、trusted extension、工具、模型或环境秘密；macOS sandbox 不可用保持 fail closed。

### P3：仅在明确产品需求后（约 3–6 人日，按事件另计）

包括资源配额、session 生命周期策略或高频遥测。每个事件先写 payload 数据分类、失败策略、性能预算、调用位置和威胁模型，批准后实现。没有具体需求不做泛化扩展。

## 7. 分阶段计划（替代原 Phase 0–5）

| Phase | 工作 | 估计 | 前置/验收 |
| --- | --- | --- | --- |
| 0 | 事件映射、威胁模型、数据分类、基准测试 | 1–2 人日 | 确认每个 Hook 的调用点、payload 和失败语义；冻结安全不变量。 |
| 1 | P0 示例、文档、fixture 与故障演练 | 1–2 人日 | 示例只做附加 deny/脱敏；所有 Hook 失败关闭行为可复现。 |
| 2 | 可靠审计 sink 与/或最终 launch Gate | 3–4 人日 | schema、迁移、访问控制、脱敏与集成测试完成。 |
| 3 | sandbox/env 单调收紧实现 | 4–6 人日 | 需安全评审；分别测试 bash/child、符号链接、缺失 sandbox 和 provider key。 |
| 4 | 可选的新事件/资源策略 | 3–6 人日 | 每项独立设计评审，不能以“通用扩展”为理由合并。 |
| 5 | 回归、fuzz/负向测试、性能与发布演练 | 2–3 人日 | `cargo fmt`、`cargo clippy -p pi-whim-agent-team`、`cargo test -p pi-whim-agent-team`；Gate 延迟与 Observe 丢弃率满足预算。 |

总计：只完成 P0 为 **2–4 人日**；完成 P0–P2 为 **11–17 人日**，不含 P3 的需求不确定性。原计划的“P2 两天、P3 三天、全量测试一天”未包含 payload/version 兼容、两套沙箱、机密处理、负向安全测试与 UI/持久化审计，估计偏低。

## 8. 配置与实现检查表

1. 全局 Hook 与项目 Hook 分开部署；项目 Hook 必须走指纹批准，不将项目 Hook 视为管理员策略。
2. Gate 的 matcher 使用真实工具名：`bash`、`read`、`write`、`edit`、`spawn_agent`、`send_message` 等；matcher 是精确匹配。
3. 脚本从 stdin 读取完整 JSON（不要用只读取一行的 `read`），处理不存在字段和 JSON 转义；绝不信任 arguments 中的身份字段，身份以 supervisor 注入的公共上下文为准。
4. Gate 返回仅 `{"decision":"deny","message":"..."}` 或 `{"decision":"allow"}`；超时、错误、过大输出和非法 JSON 均会拒绝操作。
5. Transform 只在明确允许的事件使用，返回最小 `{"arguments": ...}`；不修改 approval ticket、target、provider/model 或未获授权字段。
6. 审计策略默认记录 hook outcome 与脱敏摘要，避免命令参数和消息内容落盘；如合规必须留存敏感数据，使用专用加密存储和独立访问控制。
7. 每个新增字段均应判断：是否包含秘密、是否来自未可信 agent、是否可被 Transform 改写、是否会扩大执行权限；任一项为是时默认不暴露。

## 9. 最终建议

立即推进 P0，但将其定位为“可配置的纵深防御”。随后优先实现可靠、脱敏的 supervisor 审计和最终模型/策略决策点，而不是开放 sandbox 或环境变量的 allowlist。任何涉及文件路径、网络、环境变量、provider 凭据、trusted extensions 或审批成功的外部化，必须证明其权限单调递减，并经 agent-team threat model 安全评审后才可进入实现。
