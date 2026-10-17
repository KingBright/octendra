# Octendra 跨平台管理员能力：第一步合同

本步在原 remote-hosts-admin crate 内增加 privilege 模块及只读 CLI。
没有安装器变更、sudoers、服务配置、密码采集、管理员执行入口或新的任务数据库。
旧 remove_legacy_remoteplay 协议、任务日志和 transport 保持原语义。

## 实际已实现与边界

- privilege status：在当前进程中观察平台、Unix 有效 UID、Linux/Android NoNewPrivs，
  及原 helper socket 的存在性；不会连接 socket、读管理员策略或取凭据。
- socket 存在仅报告 detected_unverified，缺失报告 not_installed；
  无法读取报告 unavailable，绝不因路径存在就认定安装、身份或授权成功。
- grant 默认 not_checked；not_granted、revoked、verified 是未来经认证后端的合同值，
  不是本步从本地文件或调用者声明授予的权限。
- Windows 身份尚未接入令牌探测，明确报告 unverified。
- Android 区分普通 UID、UID2000 的 ADB shell、UID0；UID0 也不使本步执行可用。
  普通应用或 shell 不会被描述成具有 root 权限。
- 四平台统一接口及序列化合同可用；新的维护执行后端全部 execution_implemented=false、
  execution_available=false、arbitrary_root_commands=false。没有 execute/apply/install/grant 子命令。
- NoNewPrivilegesBlocksSudo 只说明当前进程不能用 setuid sudo 提权，
  不代表独立 root 系统服务不能执行经授权动作。
- macOS/Linux 原 local_sudo_v1 与本合同分别报告，不以功能标签当作运行时提权成功。

## 调用与现有回执

以下命令须使用本次源码构建的候选，不代表已部署 Agent 暴露了新 MCP 工具：

```sh
remote-hosts-admin privilege status
remote-hosts-admin privilege plan \
  --request-id <原操作的规范UUID> --device-id <设备UUID> \
  --action exact-source-dns-repair \
  --expected-policy-sha256 <64位小写摘要> \
  --expected-target-sha256 <64位小写摘要>
```

action 是封闭枚举：managed-service-restart、verified-agent-upgrade、exact-source-dns-repair。
它们是待实现的维护配置类型，不是已授予动作；没有任意文件、命令、环境或脚本参数。

plan 是无状态的描述性预览，不创建请求记录、root 计划文件、锁或授权。
绑定原 request/device、动作、预期策略/目标摘要、观察结果和原 PLAN_TTL；
摘要固定完整预览，声明 grants_authority=false、execution_enabled=false。
输入摘要是预期值，不是本组件已验证目标或策略内容的证据。

库的 plan 接收原 protocol::Record 和显式未知状态。running/unknown 仅 observe_original；
已成功/失败仅 read_original_receipt，不建议换 UUID 重试，未知优先于完成声明。
原 receipt 的 request/device 必须匹配。
CLI 没有认证原回执，始终标记 original_receipt_observed=false 并要求 observe_original。
正式 Gateway 适配器将来必须传实际原回执与未知状态，并执行原授权与语义去重检查；
本步尚未接入 Gateway 执行链路，不能宣称已部署的安全执行或恢复验收。

## 平台后端路线

| 平台 | 后端 | 首次启用 |
| --- | --- | --- |
| Linux | 独立 root systemd helper、本机 socket peer UID、root 所有策略 | 管理员核验签名/摘要后安装固定动作策略；Agent 保留 NoNewPrivileges |
| macOS | 签名系统 helper，通过系统支持的注册机制与认证 IPC | 本机管理员批准安装，核验调用签名；TCC 等隐私授权独立 |
| Windows | 独立管理员服务、仅本机 named pipe、显式 ACL 和调用者 token 核验 | 本机 UAC 安装；普通 Agent 不成为 SYSTEM |
| Android | 普通应用、ADB shell、已有 root 后端分别发现 | 应用权限、配对和 root 管理器授权分别由用户完成；重启/撤销可能需要重新授权 |

后端不得开放新网络入口或扩大既有远程受众；同时校验既有 owner/device 授权与本地调用身份。
只按 UID 校验时，同一用户的其他程序也在调用边界内，不能宣称应用隔离。
root/SYSTEM 不是取消 SELinux、TCC、UAC 或其他平台边界的理由。

## 下一步最小实现及首次确认

1. 扩展原 helper 的封闭动作，不复制调度、journal 或凭据系统。
2. 每个维护配置固定服务集合、可改字段、合法参数、预期快照、校验及备份/回滚方式。
   HO5 的精确源域名修复与一次 sing-box 重启必须单独绑定审阅后的实际域名和配置哈希。
3. 建立认证 helper handshake 与 root 所有 grant，实施、支持并逐平台验收。
   审计绑定原操作、设备、调用身份、策略版本、计划摘要、执行阶段及副作用；
   日志不能存密码，且 root 本身仍能改变本机日志，不能称不可篡改审计。
4. 首次启用前用户确认每台设备、本地与远程调用者、动作/资源集合、持久期限、
   是否允许无人值守，以及明确的撤销方式；任意 root shell 是独立范围，默认禁用。
5. 撤销先拒绝新执行，已接受/running/unknown 操作仍按原回执观察；
   卸载只处理 helper，保留审计和备份，不自动撤回已发生副作用。

本步不启用任何 grant，不改 HO5 网络配置，不部署或改变线上版本。
macOS 本机编译/测试不等于 Linux、Windows、Android 目标平台的实际管理员验收。

参考：
- https://docs.kernel.org/userspace-api/no_new_privs.html
- https://developer.apple.com/documentation/servicemanagement/smappservice
- https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights
- https://source.android.com/docs/security/app-sandbox
