# 管理员执行组件：受管 Linux 服务后端（代码增量）

本步在 octendra-admin 内复用原 Unix socket、Policy、Engine、Store、
Request 和 Record；没有第二套任务 UUID、数据库、credential 或网络入口。
本步仅源码与隔离测试，不安装或启用 helper，不变更 sudoers、DNS、NNP、
主机权限或线上版本。

## 已实现的范围

- Engine 的后端选择封闭动作，并把后端 root grant 合入原 plan.policy_sha256。
  旧 remove_legacy_weavelo 后端的默认动作和策略摘要保持原格式。
- Linux ManagedServiceBackend 仅支持 managed_service_restart，
  唯一资源为 octendra-code-agent.service。客户端不能指定 unit、shell、
  argv、环境或脚本；DNS 修复、Agent 升级及其它服务不在本后端范围。
- transport::serve_managed_service 是 Linux 库入口，复用原 root 安装检查、
  root 所有 policy/state/socket 目录、helper 独占锁、串行请求处理和内核 peer UID。
  客户端复用原 root peer 校验。后续原生 CLI 与禁用包准备入口见
  [Linux 调用与安装准备](octendra-linux-helper-entry.md)；没有自动 root 安装入口。
- 认证范围是授权本地 UID，不是应用签名隔离：同一 UID 的其它程序也处于边界内。
  远程 owner/session 的连接认证仍由既有 Gateway 执行，未新建旁路身份系统。
- ServiceGrant 是单独安装、root 所有且 0600 的动作策略，绑定原 device/grant UUID、
  明确生效/到期时间、动作、唯一 unit 以及 unit、ELF 和 Agent config 的摘要。
  Plan/Apply 均重新验证；过期、禁用、撤销或策略漂移均拒绝新执行。
- 服务单元必须是固定 /etc/systemd/system/octendra-code-agent.service，
  无 drop-in、非 transient、不需要 daemon-reload，且原服务 active。
  不支持自动启用或修正服务单元来适配 profile。
- 单元 profile 要求非 root User/Group 匹配策略 UID/GID，NNP=yes/true，
  ExecStart 精确为 root 策略中 executable + " agent --config " + config；
  只接受受限 simple 服务字段。拒绝 shell、执行前缀、生命周期 hook、环境文件、
  路径/namespace 覆盖、额外服务依赖、重复指令、续行和路径中的 systemd specifier。
- unit、ELF、config、systemctl 的路径祖先必须 root 所有、不可被其它用户写入，
  不跟随 symlink；资源必须单链接普通文件。Agent ELF 不得带 setuid/setgid，
  不接受脚本。快照绑定实际文件 hash/inode/device/mode，内容不进入回执。
- 唯一系统动作是固定 argv 的 /usr/bin/systemctl restart -- <固定 unit>，
  复用清空环境、固定 cwd、空 stdin、输出丢弃和有界等待。
  命令失败/超时/等待失败及执行后无法确认均保留 outcome_unknown。
  不执行调用者提供的程序、脚本或命令文本。

## 原 operation、回执与恢复

apply_bound 复用原 request UUID，并先经原 root-peer socket 读取原 Record；
校验设备、动作、完整 plan 摘要、策略摘要和目标快照摘要后，只提交一次 Apply。
调用方已知 unknown 或 running 时不发送 Apply；原 receipt 已完成/running 时只返回原回执。
传输失败直接返回，要求观察原操作，没有 reconnect/retry 或新 UUID 回退。

Engine 在任何副作用之前 fsync Running，再由后端写 accepted step。
OutcomeUnknown 保留 Running、有限脱敏错误和原回执，不写成可安全重试的失败。
原回执目录内同动作的 running/unknown 还会阻塞另一个 UUID 的新 Plan/Apply，
包括事先已经 prepared 的其它计划。helper 仍依赖原独占锁与串行 transport；
不能把 Engine 直接作为无锁并发服务器使用。

撤销只拒绝新执行；原 Receipt 可继续读取。未知记录不会因改 grant、
新 UUID、重连、重启 helper 或更新客户端而自动结清。
后续已添加 [root 离线行政关闭入口](octendra-linux-unknown-settlement.md)，
保留 closed_unknown，不推断历史执行成功或未执行；正式使用仍需实机验收。
Gateway 的 outcome_resolve 不能被当作已结清 helper 私有 Record 的证明。

## 平台与验证边界

Platform、Action、PrivilegeBackend、PlanRequest、Record 的既有合同保留；
macOS 签名 helper、Windows 服务/pipe、Android app/shell/root 发现没有变成可执行后端。
privilege status/plan CLI 仍只做第一步合同，不宣称 execution_available。
本机 fixture 使用隔离 Driver、私有临时 Store 和内存状态，不执行 systemctl。
Linux cross-check 只证明 Linux 目标编译，不能证明 root IPC 或真实服务已验收。
原生 CLI 已接入该库；目标机 root IPC、服务重启和 Gateway 实际集成仍待验收。

## HO5 首次启用前的具体确认项（本步不激活）

1. 设备：HO5，实际授权设备 UUID 记录在本地验证回执中，首次批准时逐字核对，
   不将实际设备标识随源码文档发布。
   旧只读维护证据中的本地 UID/GID 为 1000/1000；启用前必须从本机身份再次核对，
   不把设备名称、在线心跳或历史报告当作 root 授权。
2. 调用者：既有 Gateway owner/session 授权不扩大；本机调用者仅核验后的 UID/GID。
   用户需明确是否接受同 UID 其它程序可调用、允许的有效期限，以及是否允许无人值守。
   当前模型没有每次执行的 OS 交互认证；启用 grant 就允许该 UID 在有效期内无人值守调用。
   不接受此边界时不能启用，不能把客户端确认当作 helper 端的逐次系统认证。
   grant UUID 和到期时间应由批准的安装流程生成，本步未生成有效 grant。
3. 动作仅为重启 octendra-code-agent.service；不包含 sing-box、
   Weavelo、DNS、软件包、重启主机、任意 root shell 或文件修改。
   若 HO5 实际 unit 名不同，本后端拒绝，必须先审阅新的代码/资源范围。
4. 资源须管理员在本机核对并固定：实际 unit fragment、无 drop-in、
   Agent ELF 和 config 的 canonical root-controlled 路径及 SHA-256；
   systemctl、helper 二进制、root policy/grant、state 和 socket 的身份/属主/模式。
   这些值尚未现场核验，不能套用 fixture 的 /usr/local/libexec 路径；
   存在可写祖先、symlink 或 profile 不匹配时拒绝，不能自动 chmod/chown/改 unit。
5. 风险：Agent 服务重启会中断本机工作和本请求的连接；
   systemd 已接收重启但回复丢失时结果可能 unknown。独立 helper 通过原 Store 收尾，
   客户端只读原回执；不得重放。需先验证现有带外管理与恢复通道，Agent 恢复失败时
   不能依赖 Agent 自身来读 helper 回执。未知结清流程、撤销/卸载及保留审计需先验收。
   root 可更改本机日志，回执不被描述成不可篡改。
6. 首次 root 安装、启动、固定动作授权和真实一次重启验收应是后续单独批准的步骤。
   当前只有禁用包准备、原生检查与数据导出入口，未完成 Linux root 实机验收，因此不具备
   无人值守上线结论。NNP、SELinux 与其它平台边界保持，不能通过 sudoers 或全局
   polkit 授权绕过。

Sing-box 精确源规则/DNS 修复仍是另外一项未实现、未授权执行的维护范围，
不能用本后端替代原来的单独确认与原操作恢复。

## 本轮验证

- 本机隔离测试：127 个单元测试、3 个 CLI 测试通过；没有执行真实 systemctl。
- Linux x86_64-unknown-linux-musl 的 cargo check 通过；不代表目标机 root 验收。
- 原主目录、保留既有 WIP 的完整 workspace 回归：1062 通过、0 失败、6 个既有忽略。
- 仅本步 7 个文件纳入提交，原有 8 个未提交文件哈希保持一致。
- 本地验证回执：target/dev-verify/admin-managed-service.json。
