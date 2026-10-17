# 原 unknown 回执的离线行政关闭

仅用于原固定系统服务后端的 root 私有 Record，不适用于 HO5 现有用户服务或 Gateway operation。当前没有运行该入口或更改任何现场回执。

运行条件：独立 Linux 管理员会话；受保护且独立核验的原生 ELF；原授权已通过 revoke 持久撤销；管理员确认原服务工作已收尾并单独停止 helper 后，原 helper.lock 可独占取得。程序不会停止 helper、杀进程、重启服务、改 grant 或解除撤销。锁被占用、记录/资源/设备漂移或服务未 active 均拒绝。当前服务 active 不能证明此前 restart 已发生或未发生。

```text
remote-hosts-admin linux-helper settlement-preview --device-id <原设备 UUID> --request-id <原 request UUID>
remote-hosts-admin linux-helper settle-unknown --device-id <同一设备 UUID> --request-id <同一 request UUID> --accept-record-sha256 <preview 的 record_sha256> --accept-observation-sha256 <preview 的 observation_sha256> --acknowledge-outcome-unproven
```

只访问固定 /var/lib/remote-hosts-admin 下的 root policy、grant、receipts 和原 helper.lock，不提供自定义目录、shell、目标服务或 socket 恢复通道。重新核对原 policy/grant 摘要、原计划、完整 Record 摘要、原资源文件及当前 boot/服务观察摘要后，原 Running 转为 **closed_unknown**。

该状态表示管理员关闭了此不确定事件的处置流程，执行结果仍 unknown；不是 verified_applied、verified_not_applied、Succeeded 或可安全重试。原 request UUID、完整 Plan/plan_sha256、原 error 和 steps 保留，附加原 Record/观察摘要并 fsync 原 Store。重复关闭只读同一终态，原 Apply 永远不重新执行。撤销标记保持，不能借关闭恢复权限或删除标记重新启用。

ClosedUnknown 为新增持久状态；旧客户端若不能解析应停止并升级读取器，不能映射为失败后重试。普通 socket 没有 settlement Request，非 root 和非 Linux 在访问路径之前拒绝。没有后台自动结清，也没有断言历史执行事实的 API。

本入口只覆盖“原资源仍匹配且当前服务 active、管理员确认已无在途工作的行政关闭”。服务失败、资源已换、原策略已改、记录缺失/损坏等情况保持阻塞，需保留证据另行处理；不应修改现场凑条件。Linux root IPC、实际未知事件与带外恢复仍待验收。
