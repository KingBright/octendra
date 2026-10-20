# Workspace 默认容量策略与回归修正

当前源码明确取消默认逻辑 workspace 的旧 32 个容量门槛。
依据不是本次测试结果，而是以下已有实现与历史：

- 提交 b036fc710e4f9f6d1a16bbe20db54841695d5713
  （refactor: remove built-in permission policies and unify MCP/Code channels）
  明确删除内建 ServerProtectionPolicy quotas。
- crates/octendra-core/src/policy.rs 的 production_default 文档说明：
  Unrestricted channel baseline；default_max_active_workspaces_per_host 返回 u32::MAX。
  decide_workspace_creation 的文档明确不施加人工容量门槛。
- 同一提交已将核心策略回归改为验证 1000 个逻辑 workspace 时仍允许创建。
- docs/task-authorization.md 区分连接授权与已退役的任务授权；
  身份、设备、本地系统权限及未知操作规则仍需分别遵守，不能将无容量门槛解释为 root 授权。

u32::MAX 是当前实现的默认上界表示，不是无限物理资源的承诺。
逻辑 workspace 数量也不等于可同时占用的 SSH 连接、通道或平台资源数量。

## 本次最小修正

API 历史测试仍在创建第 33 个逻辑 workspace 时期待 429，
与上述提交后的生产默认策略不一致。只改测试，不改运行策略：

- 第 33 次创建期待 201。
- 随后 32 个过期且空闲的 workspace 应关闭。
- 仍保护持有排队操作的首个 workspace；清理后活动数仍为 2。
- 加上最后新建的 workspace，历史列表共 34 条。

事务失败时操作、输出及 outbox 回滚、活动任务保护、审计输出和过期清理断言均保留。
没有删除测试、放宽真实运行限制或修改管理员权限。
