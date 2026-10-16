# Octendra 一次性品牌迁移计划

本文件是实施计划，不代表新包、域名、仓库或系统注册已发布。2026-10-16清点基线为main 6a58a14及开发者未提交功能修复；品牌批次不得混入这些WIP。

## 已定稿

- 名称：Octendra；品牌形象：用户定稿萌章鱼。README中的章鱼字符只是文字品牌符号，不是定稿图标文件。
- Android包名：fun.hackerlife.octendra；所属域：hackerlife.fun。没有指定新子域，不凭名称发明公网入口。
- 使用统一product-brand-final-20261016.zip中的Octendra资源；已确认Library包身份与大小2718837字节，尚未核验ZIP内部清单、像素与透明边缘。
- 不新增长期旧命令兼容层；通过一次性迁移完成早期单版本切换。历史回执、提交、操作身份与旧版本证据保留原名称。

## 精确映射与入口

| 当前 | 目标 | 范围 |
|---|---|---|
| Remote Hosts | Octendra | 中文/英文README、管理页、OAuth页、CLI帮助、Skill文案 |
| KingBright/remote_hosts | KingBright/octendra | 原GitHub仓库改名，不新建仓库；目标占用尚待远端核验 |
| remote_hosts目录 | octendra目录 | 原目录原地迁移；本机目标不存在，不创建副本/worktree |
| remote-hosts-* crates | octendra-* | Cargo包名、引用、Rust模块名、构建测试和发布清单同步 |
| remote-hosts CLI | octendra | 脚本和调用方同批切换，不常驻旧名wrapper |
| io.remotehosts.agent | fun.hackerlife.octendra | applicationId/namespace/Kotlin包/Manifest/providers/ADB桥/测试 |
| com.remote-hosts.code-agent | fun.hackerlife.octendra.agent | 建议的新macOS签名标识，需专项签名/TCC迁移验证 |
| com.remote-hosts.api/connector/code-upgrade | fun.hackerlife.octendra.api/connector/upgrade | 建议服务标识，先核已安装项与事务恢复标记 |
| remote-hosts-code-agent.service | octendra-code-agent.service | Linux服务单次迁移 |
| Remote Hosts Code Agent / RemoteHostsCode | Octendra Code Agent / Octendra | Windows任务与数据目录；保留登记身份和配置 |
| Codex注册remote-hosts/remote-hosts-code | octendra/octendra-code | 已读取注册键；迁移时实际核对宿主，不保留重复注册 |
| MCP server identity remote-hosts-code | octendra-code | 工具前缀、adapter、catalog、宿主清单同批验证 |
| android ic_agent.xml | 定稿Octendra图标派生资源 | 密度、自适应图标、安全区和缩小可读性 |
| 管理页Remote Hosts标题/页头 | Octendra | favicon入口仍需定位并接入；不得声称已换图 |

共享资产建议唯一源码位置assets/brand/octendra/，包含核验过的原图、必要平台派生图和哈希清单。禁止重新生成替代定稿图标，禁止把其他产品资源一并拷入。当前未创建该目录或导入资产。

## 单次事务顺序

1. 与开发者确认功能WIP基线及活动操作，保留当前未提交内容；本批只改README和本计划。
2. 恢复官方资产读取能力，核ZIP条目安全性、Octendra选择、实际像素/透明度与SHA；只导入所需资源。
3. 同批改源码包名、AppID、页面/图标、发布工具及测试引用，禁止批量替换历史回执。精确列出活动入口，避免并存两套服务。
4. 完成同源码回归和正式产物绑定，再制定Gateway→Agent的一次性切换窗口；源目录、配置路径、服务名、原登记身份的迁移映射必须显式。
5. OAuth客户端名/redirect、TLS证书SAN、macOS签名要求/TCC、Android包身份分别核验；不关闭认证或改权限以完成换名。Android新包名不是旧包原位更新，数据及重新授权边界须说明。
6. 原GitHub仓库名称冲突核验后改名并更新origin/链接；活动workspace结束后原目录原地改名。此处顺序可按路径引用事务调整，但不能新建仓库或源码副本。
7. 更新私有部署资料及已交付拓扑差异，核CLI、MCP工具清单、OAuth、网页favicon、桌面/Android图标、服务和回滚。回滚只撤销本事务，保留用户内容及原操作证据。

## 当前批次与验收

README先展示Octendra并明确现有命令仍属迁移前入口，避免宣传不存在的新可执行包。迁移完成后删除该过渡说明，不构建长期兼容层。本轮不改安全设置、不安装、不部署、不推送。

资产像素检查、英文README同步、远端目标仓库名占用检查和系统身份切换仍未完成；不能将计划当完成回执。轻量文档验收检查本地链接、Markdown空白和本批唯一暂存文件；功能WIP原样保留。
