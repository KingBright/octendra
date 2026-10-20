# macOS 固定 helper 文件签名身份门槛

本步仅补原生只读核验入口和离线测试，不安装、不签名、不启动 helper，不修改 launchd、ACL、认证、系统安全、DNS 或网络。没有真实 root 执行验收结论。

```text
octendra-admin macos-helper verify-identity --team-id <独立审阅的真实 Team ID> --expected-sha256 <可信交付回执的完整 helper 文件 SHA-256>
```

路径固定为 /Library/PrivilegedHelperTools/fun.hackerlife.octendra.admin，signing identifier 固定为 fun.hackerlife.octendra.admin。不存在任意 helper 路径、shell、服务名、安装或激活选项。Team ID 与摘要只是此只读查询的预期值，不是权限来源；不能从被检查文件本身抄出一个身份就当安装授权。

核验先复用 root 所有、不可被普通用户写入、祖先无 symlink 的路径检查和原 FileState；拒绝非单链接普通文件、用户所有、组/其它可写、非可执行或 setuid/setgid 文件。随后只运行固定系统 /usr/bin/codesign，清空继承环境，固定 cwd/空 stdin，15 秒有界等待，stderr 最多 16 KiB；永不执行被检查的 helper。

签名验证使用 strict、all-architectures，并用单个 -R= 参数绑定 Apple generic anchor、Developer ID Application 证书 OID、精确 Team ID 与 signing identifier。Team ID 只允许 10 个 ASCII 大写字母/数字，不允许调用者提供 requirement 文本。display 元数据还须精确匹配身份，拒绝 ad-hoc、缺字段或重复字段；验证前后完整 FileState 必须一致。整个文件 SHA-256 绑定全部架构；不把某个架构的 CDHash 当整个文件摘要。

返回复用原 Capability，明确 execution_implemented=false、execution_available=false、peer_authenticated=false、grant_verified=false、grants_authority=false。即使文件签名核验成功，Helper 仍是 DetectedUnverified：未认证实际运行 peer。不存在第二套请求 UUID、授权数据库、执行 journal 或撤销通道。将来的执行入口仍须复用原 PlanRequest/Record/原 UUID/unknown 不重放及持久撤销；本查询不能清除撤销或把 unknown 改成成功。

本 profile 是 root 控制的系统 helper 文件身份，不能覆盖当前用户所有的 Agent 或 LaunchAgent。普通用户服务维护不因此需要 root；root 不执行用户可写程序或 drop-in。

限制：不是 Gatekeeper/notarization、在线证书撤销、实际 peer 签名或运行时身份验证；本机没有安装且经独立审阅的真实签名 helper，正向验收使用隔离 fixture，不宣称现场通过。后续还需真实 Team ID/交付签名、管理员独立安装确认、内核 peer 与运行代码身份绑定、逐动作 root 策略及实机断连/撤销验收。Windows SID、服务身份和 named-pipe ACL 验证仍未实现，不因本步变成可执行。

依据：[Apple TN3127 Requirements](https://developer.apple.com/documentation/technotes/tn3127-inside-code-signing-requirements) 与 [TN3161 Certificates](https://developer.apple.com/documentation/technotes/tn3161-inside-code-signing-certificates)，以及本机 codesign 手册/帮助。Apple requirement 负责签名身份条件，产品授权仍由原有系统权限和动作策略决定。
