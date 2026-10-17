# Windows SID 与命名管道离线策略

本步实现 windows_pipe 库判定器；没有 Win32 身份采集、真实命名管道、服务安装、执行器、ACL 写入或新 CLI。fixture 的 offline_policy_checks_passed 只表示给定数据符合策略，不表示内核已认证调用方。native_peer_authenticated、grants_authority 和原 Capability.execution_available 始终为 false。

## 固定边界

- 管道固定为 `\\.\pipe\RemoteHosts.Admin.v1`；helper SCM 服务固定 RemoteHostsAdmin，操作目标是另一个固定系统服务 RemoteHostsAgent。只接受 ManagedServiceRestart；拒绝 upgrade、DNS、任意服务名、shell 及用户服务 scope。Windows 用户服务维护不能借此迁移到系统身份。
- SID 必须使用有界、规范的 S-1-5 数字格式。调用方必须是精确 TokenUser 账户 SID（本 profile 限 S-1-5-21-domain-RID，RID >= 1000，不支持内置账户或 Entra SID）；不按用户名、管理员组、客户端声明或进程主 token 授权。
- 登录 SID 独立绑定，同一账户的不同登录会话仍被拒绝。登录组须带 SE_GROUP_LOGON_ID、enabled 且非 deny-only；AppContainer 和 restricted token 当前不支持，拒绝而不是放宽。
- 管道须本地、拒绝远程、原首实例归属确认、句柄不继承。服务端身份要求 LocalSystem、精确 enabled/非 deny-only service SID、SCM 与 pipe PID 对应及 helper 摘要；目标系统服务和目标映像摘要另行绑定。
- 描述符要求 LocalSystem owner、protected DACL，精确三条非继承 allow ACE：LocalSystem 与 helper service SID 为服务端读写/属性/READ_CONTROL/同步及 create-instance 权限 0x0012019f（不含 DELETE、WRITE_DAC、WRITE_OWNER），精确 logon SID 只可读写数据及同步等待 0x00100003。拒绝 NULL/空 DACL、额外/重复/继承 ACE、通配主体、客户端 FILE_CREATE_PIPE_INSTANCE、generic write、WRITE_DAC 或 WRITE_OWNER。此函数只生成预期模型，不把它应用到任何对象。
- 客户端须以 SECURITY_IDENTIFICATION 上限提供身份；未来服务端从同一管道最后读入请求取得 impersonation token，仅用于身份检查，不以该 token 执行业务。ImpersonateNamedPipeClient 或 RevertToSelf 失败、anonymous/更强 impersonation level、主进程 token 回退均拒绝。真实适配器遇到 RevertToSelf 失败必须安全终止受污染线程/进程，不能带该身份继续处理。

## 统一请求、授权与回执

复用 privilege::Action/PlanRequest/PrivilegePlan/Grant 和 protocol::Record，不生成第二套 UUID、Windows journal 或撤销数据库。策略摘要包含原 grant_id/device_id、SID、动作和映像 pin；请求绑定策略/目标摘要，观察绑定同一 pipe 最后消息的原 PlanRequest 摘要。输入的 Grant::Revoked 保留为本次阻塞证据，NotChecked/NotGranted 均拒绝；本模块不持久化或修改撤销状态；fixture 的 Verified 输入不会提升输出 Capability 为已认证。

原 Running/ClosedUnknown、显式 unknown 只 ObserveOriginal，已完成只 ReadOriginalReceipt；不改原回执，不推断 not-applied、不重复执行。旧 Record 的 Unix caller_uid 不被伪造为 Windows SID；这里仅复用其原操作观察语义。Windows 持久策略/撤销来源及真实 per-action 执行 journal 仍需原合同的原生适配。

## 验证范围与后续

离线测试覆盖规范 SID、最小 DACL、同账户跨会话、token 查询/模拟/恢复失败、服务与目标替换、逐动作/摘要/frame 绑定、撤销、原 unknown/完成回执。macOS 上跑这些测试与 Windows MSVC cargo check 只证明逻辑和目标编译，不是 Windows 真机验收。

下一步须在真实 Windows 上实现并验收：从保留句柄及 SCM 采集事实、服务/映像/持久策略自身安全描述符验证、正确二进制 SID 与 ACE 解码、token 生命周期及 fail-closed 恢复、服务端反欺骗、管道实例/PID 复用防护，接入原撤销存储/逐动作执行后再独立授权安装。不能把反序列化输入、PID 相同或此离线模型当安全事实。

依据：[Microsoft 管道权限](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)、[ImpersonateNamedPipeClient](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-impersonatenamedpipeclient)、[impersonation level](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-security_impersonation_level) 与 [CreateNamedPipe](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea)。
