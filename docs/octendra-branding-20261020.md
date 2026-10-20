# Octendra 当前命名与交付状态

范围：当前源码、命令、应用标识与新产物。2026-10-20 实施中；尚未发布或部署本次改名产物。历史提交、SQL migration、历史发布记录及原验收字节保持不变。

## 当前源码

- 正式源码仓库为 https://github.com/KingBright/octendra；部署配置仓库为 KingBright/octendra_ops，由各自负责人管理。
- 16 个主工作区 Cargo 包及独立 desktop worker 包统一为 `octendra-*`，同步修改依赖、二进制名、构建入口、安装脚本与文档。CLI 为 `octendra`，Gateway/Agent 为 `octendra-code`，管理员组件为 `octendra-admin`。
- 当前服务与应用标识统一使用 Octendra；macOS 标识使用 `fun.hackerlife.octendra.*`，桌面入口使用 `fun.hackerlife.octendra.desktop-worker`。
- Gateway/Agent 源码版本为 0.12.0。请求头、状态页 cookie、MCP 管理工具和内部临时标识统一使用 Octendra。
- Android 包 ID、Kotlin namespace 与 instrumentation 使用 `fun.hackerlife.octendra`，显示名与既定图标保持 Octendra。版本统一从 `android/version.properties` 读取，为 0.12.0 / 1200；shell 激活、线程与临时文件标识同步更新。
- Android 构建只使用既有 owner 签名，按公开证书指纹选择唯一私钥条目；不生成替代密钥、不更改权限。密钥别名与证书主题属于既有签名材料，不能为显示名称变化重新签发。签名文件不进入产物。
- 实例同步使用 protocol 2 与新的头名称/credential envelope domain。双方须一起升级；旧协议 envelope 明确拒绝，持久实例 UUID、凭据和 peer 权限不重建。
- 跨项目的当前源码引用统一使用 Weavelo；本任务不操作该项目的运行服务。

## 部署与身份切换

源码命名已改变，不代表现有服务、客户端登记、签名授权或目录已经迁移。切换继续复用原数据库、设备 UUID、权限及加密凭据。

1. 原 current 产物仍由原生命周期登记表管理。正式产包入口拒绝在既有名称未迁移时新增第二套 current。迁移必须由同一权威核验字节、原验证回执、空闲状态和恢复路径；不手工改登记表。
2. 已安装 Agent 的版本前缀、服务标识和升级头与当前源码不同。使用一次性受控迁移逐设备切换；不得提前发布会让旧 Agent 误判的自动升级指针，也不在新源码永久保留旧品牌别名。
3. macOS 签名标识变化需核验现有证书、designated requirement 与 TCC。源码目标不能代替系统授权；未经该阶段验收不宣称升级完成。
4. 外部域名、DNS/TLS 与 OAuth redirect 仅使用确认的真实目标。本次已核实 Git 仓库改名，不据品牌猜测新公网域名。
5. Android 继续保留同一应用 ID 和签名指纹，验证 APK 后由手机负责人完成覆盖安装与现场验收。凭据导入由 Book 唯一执行，本任务不重复传输。

## 验证

`python3 scripts/check-branding.py` 检查活跃源码及新文件中的旧名称，输出位置而不回显内容；历史证据按明确路径保护。`cargo metadata --offline --locked --no-deps` 核对改名后的工作区和 lock。原正式验证入口继续执行格式、Clippy、Rust/Python 测试与 workspace check，复用主目录及既有缓存。

完整回执保存在本机本次任务证据目录。源码验证、跨平台构建、真实平台执行、线上切换和恢复验收分别记录；没有发生的阶段不记为通过。
