# 0.11.0 测试反馈：code_read 路径校验错误被报告为未知执行结果

日期：2026-10-15 UTC。角色：测试体验者；状态：待开发者确认和修复。未修改功能实现或权限设计。

## 环境与复现请求形态

源码 main：`5576402938ac55e92062e2a80dc02464ce30ffda`。Gateway 和 Studio Agent 均为 0.11.0。

对已经打开的项目 workspace 调用：

```json
{"workspace_id":"<existing-project-workspace>","requests":[{"path":"/Users/<local-user>/.local/share/remote-hosts-code/releases/upgrade-jobs/<job>/result.json","start_line":1,"end_line":220}],"max_bytes":12000}
```

该路径是项目根目录外的绝对路径，不满足 code_read 的相对路径约束。本记录不要求放宽访问边界，不应为复现而重新提交原失败请求。

## 预期与实际

预期：清楚报告路径参数无效、请求读取未完成；保留既有目录边界和原请求身份。具体诊断字段设计由开发者确定。

实际：content 为 `code_read: error: tool_failed`；structuredContent 的 `error_code=INVALID_ARGUMENT`，但内层 `evidence.error_code=execution_failed`，同时 `execution_state=unknown`、`outcome=unknown`、`failure_boundary=device`、`last_confirmed_stage=operation_created`、`stage=agent_execute`，恢复建议为观察原操作。这使确定的参数校验错误看起来像执行中断。

原 request：`req_a2e18446469e439eb780be3dc73c4543`。
原 operation：`1e1b4864-16a0-428a-be5f-fa51160acb2d`。
按原 request_id 观察的 request：`req_152044229e3543c0a4ee630f4fbaf4cb`，仍取得同一 unknown 回执，没有创建替代读取。

## 对照与排除项

- Studio 项目内相对路径 Cargo.toml 读取成功：operation `91a53745-0b9b-469a-a409-cfd6aedccd77`。
- 在既有 shell 本地用户权限内只读取升级回执的白名单状态字段成功：operation `d373dbf4-2e76-4de4-8bfa-325d44c06167`；升级 completed、安装哈希匹配、严格签名校验通过。未读取配置或凭据。
- 最初观察调用 max_bytes=2500 也有独立调用错误：operation_get 产品 schema 下限为 4096。纠正预算后原回执可观察。不能把这个预算错误当作 observation 通道故障。
- 没有证据说明此次读取改变文件，也没有重放未知操作。此次结果不能推广为任意 unknown 写操作可安全重放。

## 源码线索与影响

`crates/remote-hosts-code/src/files.rs:20` 的 relative 校验拒绝绝对路径，错误文本为 `path must be a non-empty relative path without . or ..`；`diagnostics.rs:83` 仅将以 invalid_arguments 开头的文本归为参数错误。此映射缺口是源码推断，最终归因仍需开发者验证。

影响：误导故障分层和恢复判断，增加无必要的未知结果排查；上层 INVALID_ARGUMENT 与内层 execution_failed/unknown 含义不一致。

建议开发者验证无效绝对路径、含 . 或 .. 的路径及其他确定参数错误的诊断一致性，同时保持真实执行中断的 unknown 语义。此记录未选择实现方案，未增加权限，未运行新构建。
