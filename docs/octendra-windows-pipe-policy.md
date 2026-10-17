# Windows SID 与命名管道离线策略

现有 windows_pipe 离线判定器已接入 Win32 调用方 token 采集库及 windows_caller::assess_after_read 适配；没有管道监听、服务安装、执行器、ACL 写入或新 CLI。采集仅限调用方身份，其它管道/SCM/映像/请求绑定证据仍未原生认证。fixture 的 offline_policy_checks_passed 只表示给定数据符合策略，不表示内核已认证调用方。native_peer_authenticated、grants_authority 和原 Capability.execution_available 始终为 false。

## 固定边界

- 管道固定为 `\\.\pipe\RemoteHosts.Admin.v1`；helper SCM 服务固定 RemoteHostsAdmin，操作目标是另一个固定系统服务 RemoteHostsAgent。只接受 ManagedServiceRestart；拒绝 upgrade、DNS、任意服务名、shell 及用户服务 scope。Windows 用户服务维护不能借此迁移到系统身份。
- SID 必须使用有界、规范的 S-1-5 数字格式。调用方必须是精确 TokenUser 账户 SID（本 profile 限 S-1-5-21-domain-RID，RID >= 1000，不支持内置账户或 Entra SID）；不按用户名、管理员组、客户端声明或进程主 token 授权。
- 登录 SID 独立绑定，同一账户的不同登录会话仍被拒绝。登录组须带 SE_GROUP_LOGON_ID、enabled 且非 deny-only；AppContainer 和 restricted token 当前不支持，拒绝而不是放宽。
- 管道须本地、拒绝远程、原首实例归属确认、句柄不继承。服务端身份要求 LocalSystem、精确 enabled/非 deny-only service SID、SCM 与 pipe PID 对应及 helper 摘要；目标系统服务和目标映像摘要另行绑定。
- 描述符要求 LocalSystem owner、protected DACL，精确三条非继承 allow ACE：LocalSystem 与 helper service SID 为服务端读写/属性/READ_CONTROL/同步及 create-instance 权限 0x0012019f（不含 DELETE、WRITE_DAC、WRITE_OWNER），精确 logon SID 只可读写数据及同步等待 0x00100003。拒绝 NULL/空 DACL、额外/重复/继承 ACE、通配主体、客户端 FILE_CREATE_PIPE_INSTANCE、generic write、WRITE_DAC 或 WRITE_OWNER。此函数只生成预期模型，不把它应用到任何对象。
- 策略协议现为 v2，仅接受本地 SECURITY_IMPERSONATION 级 token；anonymous、Identification、Delegation 和主进程 token 均拒绝。v1 不自动迁移。修正依据是 Microsoft GetTokenInformation 明确要求：Identification token 的 TokenIsAppContainer=0 不足以证明非 AppContainer，应拒绝访问。采集期间只查询元数据，不执行业务、不 await；结束或异常展开均恢复线程身份，RevertToSelf 失败直接 abort 进程。

## 统一请求、授权与回执

复用 privilege::Action/PlanRequest/PrivilegePlan/Grant 和 protocol::Record，不生成第二套 UUID、Windows journal 或撤销数据库。策略摘要包含原 grant_id/device_id、SID、动作和映像 pin；请求绑定策略/目标摘要，观察绑定同一 pipe 最后消息的原 PlanRequest 摘要。输入的 Grant::Revoked 保留为本次阻塞证据，NotChecked/NotGranted 均拒绝；本模块不持久化或修改撤销状态；fixture 的 Verified 输入不会提升输出 Capability 为已认证。

原 Running/ClosedUnknown、显式 unknown 只 ObserveOriginal，已完成只 ReadOriginalReceipt；不改原回执，不推断 not-applied、不重复执行。旧 Record 的 Unix caller_uid 不被伪造为 Windows SID；这里仅复用其原操作观察语义。Windows 持久策略/撤销来源及真实 per-action 执行 journal 仍需原合同的原生适配。

## 验证范围与后续

离线测试覆盖规范 SID、最小 DACL、同账户跨会话、token 查询/模拟/恢复失败、服务与目标替换、逐动作/摘要/frame 绑定、撤销、原 unknown/完成回执。macOS 上跑这些测试与 Windows MSVC cargo check 只证明逻辑和目标编译，不是 Windows 真机验收。

后续须在真实 Windows 验收新增 FFI/token 生命周期；原生管道安全描述符/SCM/映像/持久策略采集、ACE 解码、服务端反欺骗、frame 与实例/PID 复用防护、原撤销存储/逐动作执行仍未完成。完成前不能安装或启用此候选，也不能把反序列化输入、PID 相同或交叉编译结果当运行时安全事实。

## Win32 采集边界

remote-hosts-admin 保留 forbid(unsafe_code)。FFI 隔离于小型 remote-hosts-admin-win32 库的 native 模块，其余解码/控制流使用有界 slice，不解引用 token 中的指针。只接受已有 BorrowedHandle：验证是服务端管道并确认线程原本没有 token；随后 ImpersonateNamedPipeClient、OpenThreadToken(TOKEN_QUERY, OpenAsSelf=true) 和有界 GetTokenInformation。用户/登录 SID 来自内核 TokenUser/TokenGroups，组须带完整 LOGON_ID/ENABLED 且非 DENY_ONLY；TokenType/level/AppContainer/HasRestrictions 查询不明确即拒绝，不回退进程 token 或客户端身份声明。

查询分配上限 64 KiB、组数上限 512，校验返回长度、内部指针偏移、SID 完整长度/修订版/子权限数、登录组唯一性；缓冲区与 token handle 保留到解码结束。RAII 覆盖成功、失败与 unwind 的 RevertToSelf；恢复失败不能返回身份，生产实现直接 abort。离线测试中的 fatal 路径用受控 panic 观察，没有运行真正终止进程的路径。

返回 CallerIdentity 无公开构造器或 Deserialize。公开 Windows 适配器只调用真实采集器，先丢弃 observation 中自报的 caller/成功标志，采集错误直接返回 Err；采集成功仍调用原策略/计划，不把其它自报边界变为可信或授予执行。API 名称 after_read 是调用前置条件：未来服务端必须独占保留同一连接，在最后 frame 读取后同步采集；本步不读 frame，不提供完整协议认证。

参考：[GetTokenInformation](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-gettokeninformation)、[OpenThreadToken](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openthreadtoken)、[RevertToSelf](https://learn.microsoft.com/en-us/windows/win32/api/securitybaseapi/nf-securitybaseapi-reverttoself)。

依据：[Microsoft 管道权限](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-security-and-access-rights)、[ImpersonateNamedPipeClient](https://learn.microsoft.com/en-us/windows/win32/api/namedpipeapi/nf-namedpipeapi-impersonatenamedpipeclient)、[impersonation level](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-security_impersonation_level) 与 [CreateNamedPipe](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createnamedpipea)。
