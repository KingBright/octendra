# Android 应用内无线 ADB：真机验收（2026-11-06）

0.12.8 已正式签名并安装到当前两部手机。远程 Agent 通过原 Octendra 通道到达手机；手机应用自行在 `127.0.0.1` 配对、连接系统无线调试服务并激活内置 shell helper，无需电脑作为 ADB 中继。

## 交付身份与检查

- APK 源码：`7a3a6bf7859c3f4ce3bfda82b72a55120436a8be`，`main` 已推送；本验收文档在后续仅文档提交中记录。
- 包名：`fun.hackerlife.octendra`；版本：`0.12.8` / `1208`。
- 两台已安装 APK 的 SHA-256 均为 `30c09099a988fce4c26802f478da2d5c00a4b2eddce16c5d262584d20f7f4d99`，分别从安装路径校验。
- 沿用原机主签名证书：`f84d9b71f1a00f4b5e3084420e5cee3327053389983b2428d9bec71755e5539d`；构建、签名与交付使用现有脚本，设备配置和数据保留。
- 34 项 Debug JVM 单元测试、29 项相关 Python 测试通过，Release lint 无错误；Debug / AndroidTest / Release APK 构建通过。AndroidTest 构建不等于本次执行了模拟器仪器测试。
- 这次交付为原机主签名包和两台现有手机升级，没有发布新的公共下载渠道。

## 实机结果

| 验收 | 努比亚 NX741J / Android 16 | Redmi 2409BRN2CC / Android 15 |
| --- | --- | --- |
| 手机本地读取系统配对码并完成配对 | 通过 | 通过 |
| 远程 shell 身份 | UID 2000 | UID 2000 |
| 排除 USB 激活 | 暂停 MacBook USB watcher；关闭手机 USB 调试；电脑 ADB 设备列表为空 | 暂停 Studio USB watcher；关闭手机 USB 调试；电脑 ADB 设备列表为空 |
| 无 USB，主动结束已确认的本应用 helper 后自动恢复 | 通过；PID 29527 → 30028，activation_count 1 → 2 | 通过；PID 21300 → 21967，activation_count 1 → 2 |
| USB 仍关闭时远程写、读、删除临时 shell 文件 | 通过 | 通过 |
| 原生 MCP 文件上传、expected_version 覆盖编辑、下载 | 通过；最终 SHA-256 一致 | 通过；最终 SHA-256 一致 |
| Wi-Fi 关闭再开启后的远程通道 | 可访问；VPN 默认路由下 network_generation 仍为 0 | 自动恢复；network_generation 变为 1 |
| 系统无线调试重新授权后发现新 TLS 端口 | 通过；connection_count 最终为 3 | 通过；connection_count 最终为 4 |
| 最终无线 / shell 状态 | paired、ready、UID 2000、无错误 | paired、ready、UID 2000、无错误 |

所有 USB watcher 和 USB 调试开关已恢复，Wi-Fi 与无线调试均已开启；测试未修改 Weavelo。MCP 传输测试在各手机专用工作区保留一个 37 字节验收文件，因 Android 尚未开放原生文件删除接口；shell 临时文件已删除。

## 本次修复与边界

实际配对过程修复了 NSD 监听生命周期竞争、AndroidKeyStore 与 Conscrypt 原始 RSA 签名参数不匹配，以及 LibADB 在正常关闭流时等待 EOF / 报错的问题。最后一项使用有界读取和带退出码的显式结束标记，避免把正常关闭误判为启动失败。

Wi-Fi 重连或切换接入点后，两台系统都可能关闭无线调试或再次显示网络授权。现有远程连接可恢复，Agent 已利用已有 shell / 无障碍能力远程处理系统设置和正常授权页面，再连接成功；不能称为系统权限被撤销后也能完全自动恢复。Redmi 的“始终允许此网络”持久勾选未得到确认。

整机重启、首次解锁、系统撤销配对、没有无障碍权限时的通知手工配对路径，以及其他厂商 / ABI，尚未独立完成本轮真机验收。完整后台显示、输入法共存、应用分身和 VM 隔离仍按[规划对账](planning-status-20261105.md)推进。
