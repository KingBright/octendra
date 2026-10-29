# Octendra 持续使用与版本规划

我们同时是使用者和维护者。实际使用中暴露的问题进入本目录，不再只留在对话或某一次长报告中。

## 一个事实源

`backlog.json` 是问题清单的唯一可编辑事实源；`BACKLOG.md` 是生成的阅读视图。每项有稳定 RH 编号、现象、来源、当前处理、验收条件、优先级、依赖和目标版本。使用中发现新问题时先检查是否已有对应项，避免重复记录；不能确定根因时明确写未定位。

```sh
python3 scripts/product-backlog.py --render --check
```

校验会拒绝重复编号、未知状态、循环依赖及没有验收证据的关闭项。暂时移出当前版本只能修改目标版本并说明原因，不删除问题。

## 每轮迭代

开始时读取清单和最新发布回执，选定一组可交付问题；实际观察到的新失败立即记录操作ID、设备、版本、发生阶段和可公开的诊断，不保存密码、签名下载URL或完整私人命令。实现后添加针对问题的回归与场景验收，更新当前处理和证据路径。

候选测试通过标记 candidate_verified；部分修复仍为 partial。只有满足该项验收条件才可 closed，并填写 closure_evidence。跨进程续传不能用进程内Range测试关闭，原生ChatGPT附件不能用模拟HTTP客户端验收替代，安装二进制不能代替运行构建确认。

## 版本与状态边界

当前 repository/release 版本以源码中的 package version 和固定 release evidence 为准。版本历史描述产品能力演进，但不记录某个维护者实例当前有哪些设备在线、安装了哪个版本或是否已经完成现场验收。公共仓库与 private deployment/live runtime 的边界见 [Repository Content Model](../repository-content-model.md)。

目标版本永远是规划，不是完成承诺。已知数据损坏、身份隔离、错误副作用重放或不可恢复发布风险属于上线阻塞项；明确的非阻塞功能缺口可以留在 backlog 中继续迭代。

## 验证与发布职责

一个通用 release pipeline 按固定输入验证 → 平台构建与打包 → 生成不可变 manifest 完成 repository/release 闭环。是否自动推送、部署到哪些 Gateway/Agent、目标的 drain 策略、真实域名和设备清单属于 private deployment policy，不写死在公共产品文档中。

具体部署系统可以在 release candidate 通过后执行 Gateway-first / Agent-second 的独立目标升级和现场验收。平台拒绝、任务未排空、验证失败、输入变化或健康检查失败时，应记录具体阶段并保留候选和原操作标识；这些现场 receipts 属于部署实例，不回写成公共仓库的“当前状态”。

## 稳定构建与发布入口

在唯一主工作区的 `main` 直接修改，完成本次改动所需验证后提交并推送。普通任务复用连接和设备的现有授权；宿主 Agent 负责操作确认，Octendra 不增加任务审批、限时任务授权或手工任务 ID 步骤。

正式构建复用 Mac Studio 的主工作区和 `~/rust-target`，通过现有入口产出固定 current 包：

```sh
python3 scripts/release-code.py --report target/iteration-runs/current-release.json --start --platforms macos linux windows linux-arm64
```

进度通过同一报告的 `--status` 查询；已有报告对应的操作继续观察，不重复启动。`source_snapshot.py` 仅用于读取输入身份和历史证据，常规构建不复制源码。需要绑定已推送的准确提交时，使用 `release-code.py --source-commit <完整 SHA>` 的固定输入入口。

构建完成后，使用 `scripts/fleet-upgrade.py` 和显式部署配置，将验证过的本地包直接传到目标：先 Gateway，再 Agents。macOS 使用 `scripts/launch-code-upgrade.py --start`。复用现有安装器和设备系统签名，记录交付 hash、运行版本与实际操作结果；发现问题直接在 main 修正并继续交付。

完整复盘和新纪律见 [`ITERATION-EFFICIENCY.md`](ITERATION-EFFICIENCY.md)。Agent/MCP 上下文与终端输出的固定优化契约见 [`TOKEN-EFFICIENCY.md`](TOKEN-EFFICIENCY.md)。

源码必须有可追溯 Git 基线；构建与部署直接关联已推送的提交。发布配置、密码、运行数据库、缓存和大型临时二进制不应进入提交。功能范围先锁定、最终候选再改版本号，避免每个小补丁都触发无意义的全量重链接。下一轮功能优先级见 `NEXT.md`；实际状态以 `backlog.json` 为准。

## 发布证据

公共仓库只保存 repository/release evidence：固定源码身份、测试门禁、artifact manifest/SHA、平台矩阵和已知限制。当前 repository release 以源码 package version 与对应固定验证结果为准。

早期 `docs/releases/` 中同时存在 release verification 与真实部署 acceptance/deployment 记录；这些现场记录现在统一视为 **legacy deployment evidence**，不能用来推断当前在线设备或运行版本。新的真实发布目标、installed/running/accepted 状态和现场 receipts 进入独立 private ops/runtime 系统。现场验收只有实际执行成功才可标 passed；宿主在执行前拦截、参数验证失败等情况必须保持未执行/失败状态。

工具平台拒绝某个操作时，记录为“未执行/外部阻塞”，不把它记为测试失败，也不通过改名或包装等价操作绕过。其余独立、正常授权的工作仍可继续。

## 沟通

过程反馈说明已经完成的变化、当前运行的检查和真实阻塞；不让用户靠连续追问才能判断状态。远端任务存在不等于模型会在回复结束后继续工作。每次收尾明确本地源码、构建包、线上运行及未完成项各自的状态。
