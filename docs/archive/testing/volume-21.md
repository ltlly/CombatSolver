# 测试记录归档 21

本卷保存已完成的 PR #203、#204 合并验收，来源和未验证项保持历史口径；归档不表示本轮复测。

## PR #203、#204 合并验收（2026-10-04）

以下为本轮直接运行结果；全部使用120秒上限和实例清理，已通过的请求未重复运行。无头验证停在对应机制边界。

| 场景 | runId | 结果与范围 |
| --- | --- | --- |
| `GOLD-HEALING-MECHANISMS` | `3bc7928290c343cca88248c0c28dd95e` | Passed；金币修正、三个回调、完整原生状态/RNG和父分支隔离 |
| `MAX-HP-HEALING-CALLBACKS` | `cd6549478924451882fc600e67724a53` | Passed；最大生命实际增量、封顶与回复回调的原生差分 |
| `FEED-MAX-HP-CAP` | `ddcf661e391c4013a0aa87861f8542f3` | Passed；两次致命出牌的实际增量与成长计数 |
| `RELIC-MAX-HP-HEALING-BOUNDS` | `bee94b31824d449e8f7e9fa55cbe94f5` | Passed；两项直接原生遗物回调与熔化来源排除 |
| `AXEBOT-JOSS-DEFERRED-FORK` | `0fd9902a8a594e36a0a9fbf533ceee65` | Passed；根计数、零状态指纹、父子/兄弟Fork与逐分支消费 |
| `KNOWN-HEALING-MEMBERS` | `fab7a8170a3942e9920e09fa466aa34e` | Passed；严格增量、DOP2、控制质量、成长门与live隔离 |
| `B013-FIXED-PREFIX-TERMINAL-BOUNDARY` | `5b097fd5f17745a0868a6b49d7af3b37` | Passed；终局前缀截断与可交付胜利 |
| `B013-FIXED-PREFIX-TURN-END-CARD` | `c778f285a52a4b9fbbe0e370688e6ec1` | Passed；强制结束回合卡的固定前缀推进 |
| `B013-RADIANT-PEARL-HAND-DRAW` | `ca904a20eafd43b690d8c824f60e2069` | Passed；抽牌前生成的原生数量、升级与归属对账 |
| `B013-DEFAULT-GC-LIMIT` | `3e440c65f9564b5ea6a65d094521b8d7` | Passed；限额、真实Gen2回收续搜、退出清理与CLR模式 |
| `KNOWN-HEALING-OPENING` | `3fecd35f80a34cb792a43ca3b5bc3b1c` | Passed；非认证根延后计划、单次执行与控制质量 |
| `FIXED-PREFIX-TURN-OUTCOMES` | `b18d23d1829b4438a557a5f75acf6425` | Passed；独立前缀完整状态oracle、多回合结果与终局截断 |
| `B013-BLOCK-DECIMAL-BOUNDARY` | `b556026e627c4153bb3d2bf23948a095` | Passed；原生虚弱倍率、完整状态/RNG；模拟96/95/200层与零格挡 |

真实CLR工具：`default-entry` 2项、`diagnostic-failure` 8项、`scopes` 8项 Passed。入口失败基线为日志抛错后信号仍启用；修复后同异常传播、信号清理及后续独占准入均通过。

失败记录：首次无头启动在私有进程身份检查处失败并清理，未进入游戏测试，原因未确定；金币首轮14项对账已通过，但缺EvidenceDirectory导致产物写入失败；金纸首轮缺遗物输入。补齐请求参数后仅重跑失败请求，成功记录在上表。固定前缀09f94286012d420d81242f480ebd1803仍执行旧的终局拒绝断言，合同更新为开局探测和完整搜索共同截断，并断言终局动作数及回合。新增格挡夹具初次编译因原生调用参数及私有setter失败，修正后Release零警告/错误。

L0：原生回复审计工具迁入tools/inspection后构建和真实DLL扫描通过。CoverageCatalog重新生成3035项目录，状态字段未分类为0；`--verify-state-writes`仍因既有InfusedCore.AfterSideTurnStart缺运行证据失败（1项）。PowerShell结构检查247文件通过，工具检查290文件/37项目通过，文档427文件/1635链接及覆盖目录检查通过。额外Bash结构检查因运行耗时停止，未完成；两平台脚本静态语法检查通过。

范围：本轮没有重跑作者长预算全根性能筛查，没有验证低内存玩家宿主、原生整场部署或可见Steam性能；原作者失败与未验证项保留在所属报告。

## PR #224 与 PR #226 合并验证（2026-10-06）

保留合并时的证据、失败与未验证项；智能药水修复的本轮验证见[当前记录](../../issues/potion-opportunity-20261006.md)。

## Ctrl+F9 面板可见性（PR #226）

`OVERLAY-VISIBILITY-LIFECYCLE` 在原生单人战斗中验证快捷键输入、已有及新建 CanvasLayer 的隐藏状态、禁用／手动／搜索中／停止显示、监控刷新，以及 `BeginCombat` 重置后的初始化消费与恢复显示。初始化置位在重置返回时断言，可操作边界的初始化完成在等待旧会话释放后断言。使用现有停止开关在初始合同后结束，搜索状态显示通过 UI 入口注入。

PowerShell：`pwsh -NoProfile -File tools/testing/run-unattended-test.ps1 -ScenarioId OVERLAY-VISIBILITY-LIFECYCLE -CharacterId IRONCLAD -EncounterId FUZZY_WURM_CRAWLER_WEAK -EnemyCurrentHp 1000 -StopAfterCombatRootSnapshotAssertion -EnableNoGcRegionForTest 0 -TimeoutSeconds 120 -CleanupInstanceOnExit`；Bash：`./tools/testing/run-unattended-test.sh --scenario-id OVERLAY-VISIBILITY-LIFECYCLE --character-id IRONCLAD --encounter-id FUZZY_WURM_CRAWLER_WEAK --enemy-current-hp 1000 --stop-after-combat-root-snapshot-assertion --enable-no-gc-region-for-test 0 --timeout-seconds 120 --cleanup-instance-on-exit`。

2026-10-06 本轮失败证据：PR 头 `0725fe21` 加入新图层断言后，runId `2f55f493930144c6800f6835042f134a` 在新建 CanvasLayer 默认可见边界 Failed。直接应用快捷键隐藏意图后，runId `53fc7f11eff84cb4b2d6b7e9cb27ddc4` 通过快捷键、新图层及各显示入口，随后重置断言 Failed：等待旧会话释放期间监控已消费初始化请求。合同按实际生命周期在重置返回时检查置位，在释放后检查完成状态。

最终 runId `37ee21f7569a43c3b5fed01a4e5b4d28` Passed（22.66 秒），三组界面合同全部通过；玩家原生结果保持回合 1、80/80 HP。全部三次请求均完成实例目录清理。本轮 .NET SDK 9.0.300 Release 构建为 0 警告、0 错误，结构门禁、工具检查和文档检查通过。可见 Steam 人工操作、完整 SL 场景和 Linux 运行未验证；贡献者提供的实机记录保留在 PR 正文。

## 手牌上限状态一致性（PR #224）

贡献者[测试记录](https://github.com/tianyilt/HextechSolverCompat/blob/main/docs/TESTING-PR-HAND-LIMIT-20261003.md)来自 0.48.1：可选 BaseLib `IMaxHandSizeModifier` 的上限 13→16→13 验证旧根／兄弟隔离、新根指纹区分与续用戳恢复，Dredge 13、CrashLanding 5 完整实际／预测状态通过，兼容层同项修复关闭。基线 `2ead87d9c9e35b1588a760efff0bd6154545a77c`，候选 SHA-256 `04c70a0c0ff4d9169a8184a327beae1e246bca75179ed8bd521331e08d384d1c`。耗时门槛 NotPassed：中位数 17.0191→24.3435 ms，保留 76.4900 ms 尾项，CPU 负载未测，因果归属未知。重定基至 0.50.0 `0d290fbee7e2779d2cebd8b8f652d82d00b6e8fc` 后仅构建通过（SDK 9.0.318、RitsuLib 0.6.5、游戏 0.111.0、零警告／错误、关闭自动部署与祖先 props/targets 导入），原生与耗时证据仍属 0.48.1。

当前原版根／Fork 复跑：PowerShell 使用 `tools/testing/run-unattended-test.ps1 -ScenarioId HAND-LIMIT-ROOT-CONSISTENCY -EnemyCurrentHp 1000 -VerifyCombatRootSnapshot -StopAfterCombatRootSnapshotAssertion -EnableNoGcRegionForTest 0 -TimeoutSeconds 120 -CleanupInstanceOnExit`；Bash 使用 `./tools/testing/run-unattended-test.sh --scenario-id HAND-LIMIT-ROOT-CONSISTENCY --enemy-current-hp 1000 --verify-combat-root-snapshot --stop-after-combat-root-snapshot-assertion --enable-no-gc-region-for-test 0 --timeout-seconds 120 --cleanup-instance-on-exit`，两端默认 IRONCLAD／FUZZY_WURM_CRAWLER_WEAK。

2026-10-06 合并验证：SDK 9.0.300 Release 构建零警告／错误；上述原版合同 runId `63b5712a8b034b8388dbf4d71f42c311` Passed（22.56 秒），核对基础手牌上限、根与 Fork 的 live/predicted 续用文本及捕获隔离，实例目录已删除。动态上限 13→16、Dredge／CrashLanding 与耗时对照本轮未复测，历史 NotPassed 保留。
