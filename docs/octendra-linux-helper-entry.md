# Linux helper 调用与安装准备

本入口复用原 Capability、PlanRequest、Record、Engine、Store 和 Unix socket。动作仍仅为固定 `octendra-code-agent.service` 的重启；DNS、sing-box、Weavelo、任意 shell 和软件升级均不包含。本文给出后续管理员验收路径；本轮不安装、启动或启用 root helper。

## HO5 实际服务模型

现场 Agent 使用 liang（UID/GID 1000/1000）的 systemd 用户服务；用户 bus 可连接、CanStart/CanStop=yes、RefuseManualStart/Stop=no。普通重启应走既有用户管理器，保持 UID 1000，无需 root helper；本轮只读验证控制条件，没有实际重启。现有用户 unit、ELF/config、%h 路径和 drop-in 不适配下述系统服务 profile。不得把它们交给 root 执行、去掉 root 检查、删除 drop-in 或迁移为系统服务来强行适配。当前没有为 HO5 普通重启新增 root 权限的必要项；真正 root 资源维护须另按固定 root 所有目标审阅。

准备包的 --account/UID/GID/home 必须来自目标机 NSS 证据，不能沿用打包机账户；后续 inspect-site 会再次核对。禁用包是通用系统服务候选，不是 HO5 用户服务的安装方案。

## 固定准备入口

在原 main 主目录执行 `python3 scripts/prepare-admin-helper.py --profile managed-service --device-id <设备 UUID> --account <本机账户> --uid <实际 UID> --gid <实际 GID> --home /unconfirmed`。它复用 `~/rust-target`、既有 rust-storage registry 和 current 发布事务；不接收外部二进制或任意输出目录。交付为 `dist/octendra-admin-linux-current` 和同名 `.tgz`。包内只有静态 Linux x86_64 ELF、策略数据、固定 unit 模板、清单和说明，无安装脚本。policy 与 service grant 默认均为 false，时间窗为零，资源路径和摘要尚未确认。`unsigned-dev` 仅表示准备与校验，不是签名发行或 Linux 实机验收。

从独立可信回执取得 manifest SHA-256；不能只信可一起替换的旁置 checksum。原生 `octendra-admin linux-helper verify-package --bundle <包> --manifest-sha256 <可信摘要>` 验证固定成员、摘要、静态 ELF、禁用策略及固定服务模板。

## 后续独立管理员会话

1. 管理员先核对设备、真实 NSS 账户 UID/GID/home、带外恢复通道和 helper ELF 摘要；将已核验 ELF 暂存到 `/var/lib/octendra-admin/bin/octendra-admin`，root:root 0755、所有祖先 root 控制且不可写，再用可信系统工具独立复核目标摘要。不得直接以 root 执行用户可写包中的程序或脚本。本步骤没有自动安装命令。
2. 由该 root 控制的原生 ELF 执行 `linux-helper inspect-site --bundle <包> --manifest-sha256 <可信摘要> --output <新 site.json>`。仅 Linux root 可用；再次核对自身 ELF、实际固定 unit、无 drop-in、Agent ELF/config 摘要与属主、NNP、实际身份和 boot ID。不会修改现场以适配 profile。
3. 执行 `linux-helper prepare-install --bundle <包> --manifest-sha256 <可信摘要> --request-id <原 operation UUID> --site <site.json> --not-before <Unix 秒> --expires-at <Unix 秒> --allow-unattended --output <新 review.json>`。现场证据必须新鲜且重新检查。未提供现场、明确时间窗或无人值守边界时只生成 blocked review，不授予权限。
4. 核对 review 中实际设备/账户、固定目标、所有摘要、期限与撤销方式，明确接受其 `record.plan_sha256`，再执行 `linux-helper render-approved --bundle <包> --plan <review.json> --accept-plan-sha256 <已批准摘要> --output <新私有目录>`。再次检查现场，只导出批准的数据，既不安装也不启动服务。该准备 Record 的动作不能作为服务重启 Record 使用。
5. 经单独系统授权，管理员自行把批准的数据安装到 `/var/lib/octendra-admin/{policy.json,service-grant.json}`（root:root 0600），保留原 receipts（root:root 0700）；模板目标为 `/etc/systemd/system/octendra-admin.service`（root:root 0644）。不得覆盖未知安装或删除原撤销标记。启动、enable 和 daemon-reload 是另外的系统动作，本工具不执行。把包内默认模板安装过去仍是禁用状态；批准的导出数据才包含明确有效期的动作授权。

同 UID 其它程序也能访问该本地能力。启用有效 grant 意味着该 UID 可在窗口内无人值守调用，不等于每次 OS 交互认证；不接受该边界时不能启用。

## 普通调用者链

使用 `/run/octendra-admin/helper.sock`（可用全局 `--socket` 显式指定既有地址）：

```text
octendra-admin linux-helper status --device-id <设备 UUID>
octendra-admin linux-helper plan --device-id <设备 UUID> --request-id <原 operation UUID> --output <新 plan.json>
octendra-admin linux-helper apply --device-id <设备 UUID> --plan <plan.json> --accept-plan-sha256 <已批准 plan 摘要>
octendra-admin linux-helper receipt --device-id <设备 UUID> --request-id <同一 operation UUID>
octendra-admin linux-helper revoke --device-id <设备 UUID>
```

status 只有内核确认 root peer、身份/profile/策略有效才报告 shared capability 可用。Apply 在一次副作用前先读原 Record，绑定设备、UID、策略、目标和完整计划；禁止改变 UUID 重放。未知结果只观察原回执，`--outcome-unknown` 不会重试。撤销持久化原标记并禁止新执行，原 Receipt 继续可读。不得删除标记恢复权限；卸载须保留审计证据。

## 验证与剩余门槛

隔离测试覆盖打包默认禁用、无脚本、篡改/链接/动态 ELF 拒绝、明确摘要批准、现场漂移、私有导出、原操作读取、未知结果不重试和撤销。Linux 静态构建与 macOS 原生 CLI 测试不能替代 Linux root IPC、实际固定服务重启、Agent 断连/恢复、带外读取、撤销/卸载以及原 unknown Record 结清的实机验收。root 侧提供[离线行政关闭入口](octendra-linux-unknown-settlement.md)，仍须 Linux root 实机验收；Gateway outcome_resolve 不能代替 helper Record 结清。

macOS 后续必须用签名 helper、受保护 launchd 和内核 peer 身份实现同一合同；Windows 必须用受保护服务、pipe ACL 与调用者令牌；Android app/shell/root 必须独立证明执行边界。当前没有这些平台的可执行适配，不允许回退到 sudo、脚本或放宽 DNS/网络权限。
