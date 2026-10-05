# CombatSolver 测试入口历史卷 20

[返回归档索引](README.md)

## 0.50.0 定稿证据（2026-10-05）

0.50.0 沿用 PR #207、PR #211 和开局药水准入的既有行为证据；本轮只同步版本与更新日志，验证文档并完成 Release 构建。

PR #207 合入0.49.4的本轮证据见[整合验收](../../performance/pr207-upstream-0494-integration-20261005.md)：成本、组件回复及Smart原生合同通过，零额度遗物本地/外部准入先失败后通过；最终整请求及固定回归结果按该报告更新，旧版本数字不冒充本轮通过。

## 社区批次 Q010 与组合补搜

同根夹具和质量界见 [历史卷 17](../../archive/testing/volume-17.md)，各阶段定位与失败证据见 [Q010 复现记录](../../community/drafts/2026-10-04/Q010-claim-reproduction.md)。现行组合入口按共享节点与时间准入，执行期间在每批提交边界处理内存预约、回收和停止。

`python -B tools/testing/checks/BeamWidthPortfolioChecks/run.py`：105 项通过，覆盖生产成员比较、预算、截断与节点／时间门控。`dotnet run --project tools/testing/checks/CombatSolver.GcPolicyChecks/CombatSolver.GcPolicyChecks.csproj -c Release -- default-commit`：2 项真实 CLR 合同通过，覆盖不可分割提交回收续行与取消。

原包搜索入口为 `tools/testing/run-unattended-test.ps1`／`.sh`，从 `start` 用 `SearchOnly` 执行 O042、O045 夹具的断言并在首个结果停止；公开原包由社区资料 Release 的 Q010.zip 提供，本地输入放 `.local`。固定同根、政策、预算与 GC 启动模式对照，单请求超时 120 秒。可见帧时间与整场自动部署分别验收。

2026-10-05 合并候选以 PR `fe976544` 为基线，在同机常规 Server GC 下完成同根对照：O042 两侧 Passed，战损 5、零药、第 7 回合、评分 10001354974；展开 85893→96600、转移 356997→401152、请求搜索耗时 14.700→16.085 秒。O045 两侧 Passed，零损零药、第 9 回合、评分 10002069967，展开 19474、转移 89746，耗时 6.206→6.230 秒。两组 `rootContinuationStamp` 与实际政策分别相等；O042 增加的补搜工作和耗时如实计入，单次对照不构成通用性能承诺。

候选 runId：O042 `dceaceae371d4c42b1831f5c853d5662`、O045 `9cd3f64b3cfe4a028d214117a173d3cc`。独立 BYRDONIS 哨兵 `73be250581904263b276ae1eb8ae962a` Passed，保持先前同源码基线的 16 战损、零药、第 4 回合、评分 10000919984、展开 4784／转移 18223，搜索 1.836 秒。启动策略有效关闭 NoGC，强行启用的请求在设置断言处失败，因此真实 NoGC 区域重建与可见帧时间未验证。原生测试实例均已删除。

## 开局药水补搜准入

`SMART-OPENING-POTION-ADMISSION` 固定两槽满栏，使用原版奖励 RNG 捕获确定掉药／不掉药的搜索根。最小合同覆盖低战损时两类药水搜索的共同准入、换药抵扣后可接受的用药路线，以及原价省血达标的路线；检查真实战斗与冻结根保持一致。

```text
pwsh -NoProfile -File tools/testing/run-unattended-test.ps1 -ScenarioId SMART-OPENING-POTION-ADMISSION -CharacterId SILENT -EncounterId FUZZY_WURM_CRAWLER_WEAK -EnemyCurrentHp 1000 -EnableNoGcRegionForTest 0 -TimeoutSeconds 120 -CleanupInstanceOnExit
./tools/testing/run-unattended-test.sh --scenario-id SMART-OPENING-POTION-ADMISSION --character-id SILENT --encounter-id FUZZY_WURM_CRAWLER_WEAK --enemy-current-hp 1000 --enable-no-gc-region-for-test 0 --timeout-seconds 120 --cleanup-instance-on-exit
```

2026-10-05：`cc718337` 的生产源码在同一夹具中 Failed，runId `b0a21e369c6c4fab9bf57038efee2a17`。两槽满、确定不掉药、零药胜利 3 战损及梯度拒绝断言均通过，随后在“零准入配额应在生成具体药水前缀之前结束”断言处失败。此证据来自最小原生场景，玩家原战斗与可见界面尚未回放。

夹具建立时两次前置断言失败：铁甲战士首局使用教程奖励，无法提供确定预测；A0 默认有三个药水槽，放入两瓶尚未满栏。最终入口使用静默猎手并显式固定两槽。

修复后的 runId `2613abde700a4f6994c275f15831d4af` 中，低损跳过与换药抵扣两项通过：不掉药保留 3 战损／零药，展开 2 节点且生成前缀与具体药水进度均未出现；确定掉药时选中零战损／一药，实际省血 3、要求 1，展开 3 节点。该请求整体为 Failed：原价省血哨兵只增加主动扣血，搜索可绕行，预设的 12 HP 基线没有成立。

哨兵补入敌方力量，使零药的低损绕行成本高于主动扣血。`SMART-OPENING-POTION-VALUE` 只复跑这项修改后的边界，runId `b4533ab82c1e4f5e9e18b54bb8794c54` Passed（23.271 秒，包含建局）：确定不掉药时保留零战损／一药，省血 12、要求 9，展开 3 节点；真实战斗与冻结根一致。使用上述命令将 ScenarioId 改为 `SMART-OPENING-POTION-VALUE` 可单独执行。两项已通过输入未变，结果沿用同一生产源码的既有证据。Release 构建、结构门禁、文档与工具检查通过；实例由启动器清理。原玩家战斗、整场自动部署和可见 UI 尚未验证，不将最小搜索结果外推为实机耗时结论。
