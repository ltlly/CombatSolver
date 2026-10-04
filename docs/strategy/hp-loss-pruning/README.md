# Shared HP-loss incumbent pruning

This single-player change targets repeated work after an eligible complete victory is known: unavoidable early damage followed by safe turns, and fast victories rediscovered by portfolio members. Baseline: upstream 5d28a1cfa (0.49.3).

## Scope and safety

PrimaryIncumbentTable stores completed, hard-policy-compliant victories by outstanding stolen resources, explicit potion uses, per-source growth count vector, and satisfied relic target mask. Different growth sources and relic combinations remain separate; scalar HP witnesses cannot replace resource-target witnesses.

Coordinator members share pure-value bounds through frozen policy, retaining independent simulators, frontiers and transpositions. Live combat sessions carry one executable potion-free witness only when the full root continuation stamp, damage ledger and policy match. Changed input invalidates it. Victory publication occurs at the serial commit boundary.

Potion buckets are consumed only when explicit use is closed by member policy or the maximum-use limit. HP lower bounds retain future healing, death protection, post-combat healing and boss HP relief. Unknown sources keep the upstream conservative allowance. Stolen healing cards remain possible recovery sources.

PreserveResources looks up a lower bound on final unrecovered resources, subtracting resources recoverable from living enemies. It does not use the current missing-card count directly. Missing compatible witnesses preserve expansion.

Growth caps require a separate narrow original-content closure: exhausting growth sources, audited basic cards/statuses/curses, selected relics and powers. Generation, exhaust recovery, unknown callbacks and live growth-copying opportunities reject certification. Opportunity metadata alone is not a cap.

For pure growth targets, each possible final source-count vector consumes its own witnessed victory. Shared expansion stops only when every possible vector is bounded. Missing witnesses or more than 256 combinations preserve expansion. Growth/relic mixtures retain the optimistic final-bucket path, without claiming complete independent enumeration of relic outcomes.

DualWield remains uncertified while a growth object can be copied. Once no usable growth object remains, copying ordinary cards cannot reopen growth, and branch certification can resume. Fork isolation is tested.

## Equality tradeoff

Eligible unfinished, risk-free branches may stop when their optimistic strategic HP deficit equals a compatible completed victory. This prioritizes reduced search work over finding an earlier victory with identical resource outcome and HP loss. It is not a proof that the original complete ordering or every finite-Beam result is preserved. New equality pruning leaves completed candidates intact.

No action commutativity prediction, pile-order masking or multiplayer pruning is included.

## Reproduction

Build CombatSolver.csproj and tools/search/OfflineSearchHarness/OfflineSearchHarness.csproj in Release. Installed game/RitsuLib paths come from local.props; personal paths do not belong in committed requests.

Run the harness with --check-primary-incumbents for shared table contracts, or --check-early-turn-continuation-bound for existing continuation contracts.

Set OFFLINE_HARNESS_RESOURCE_BUCKET_CHECKS=1 and run --request coverage/fixtures/scenarios/state/royalties-resource-0170.json --milestone M1 for resource contracts. Set OFFLINE_HARNESS_THEFT_BUCKET_CHECKS=1 on an Ironclad combat for theft contracts. These are shadow-state assertions, not native actual/simulated acceptance.

OFFLINE_HARNESS_RESOURCE_SETTINGS reads a test-only JSON containing GrowthBudgets, RelicStrategyEnabled and RelicCounterRules, for example {"growthBudgets":{"royalties":5}}. Other settings stay CLI-controlled; player settings are not modified.

--disable-shared-incumbents retains new member-local equality behavior, so it is not the entire upstream baseline. --verify-shared-incumbent-reuse checks same-root reuse and policy invalidation on small Coordinator requests. --verify-incremental performs complete prefix replay and is excluded from performance samples.

## Evidence and limits

Rebased validation is recorded in [the test matrix](../../TEST_MATRIX.md). Historical 0.49.1 results included expanded nodes 28,956 to 17,802 on the restored Infested Prisms root and 5,669 to 3,964 on a growth-copy fixture. These are prior-version evidence, not new 0.49.3 measurements.

Offline comparisons do not prove native automatic deployment, visible Steam frame time, all growth sources, all positive potion tiers or global optimality. The upstream comparison harness receives the same test-only resource-settings loader; upstream production source is unchanged.

## 单人共享损血剪枝（2026-10-05）

基线 `5d28a1cfa`（0.49.3），游戏 0.111.0 / RitsuLib 0.6.5。候选主项目及离线宿主 Release 构建均 0 警告、0 错误。初次构建缺 net48 引用程序集，使用本机已有 NuGet 引用包的 FrameworkPathOverride 后构建成功；未修改上游构建配置。

本轮共享表20项、既有早回合界143项、成长资源35项、偷窃边界10项全部通过。成长小根的增量完整前缀回放及同根共享见证续用、政策变化失效检查通过。这些不是原生 actual/simulated 整场验收。

独立 .NET 进程、Coordinator及组合开启、Disabled、DOP1、牌堆掩码0、No-GC关闭；两侧均 Boundary=None。上游仅在测试宿主移入相同 resource-settings 读取方法，生产源码保持基线；没有用关闭共享表代替整个上游基线。

| 固定根 | 相同终局 | 展开：上游 → 候选 | 转移：上游 → 候选 | 单次搜索秒：上游 → 候选 |
| --- | --- | --- | --- | --- |
| Royalties 成长 | 胜利、收益1次/额度5、战损0、零药、第1回合 | 7572 → 11 | 20336 → 43 | 5.66 → 0.60 |
| NotYet 回血哨兵 | 胜利、先回血再击杀、战损0、零药、第1回合 | 243 → 243 | 527 → 527 | 0.62 → 0.72 |

成长根：REGENT / FUZZY_WURM_CRAWLER_WEAK / GROWTHBUCKET20261004，飞升0、敌HP6、玩家75/75、能量3、原生遗物。清空牌组与牌堆，手牌永久Royalties、2张StrikeRegent、4张DefendRegent；抽牌堆5张StrikeRegent。测试设置 `{"growthBudgets":{"royalties":5}}`。Beam45、20000节点、20000ms。回血根使用 `coverage/fixtures/scenarios/state/not-yet-heal-resource-0170.json`，Beam20、12000节点、20000ms。完整命令与边界入口见[复跑说明](#reproduction)。

耗时是单次离线观察，回血哨兵本次多0.10秒，不能称为所有场景提速。未执行原问题包恢复、完整原生自动部署、可见Steam性能、全部成长来源及正数药水档整场验证。本机产物保存在忽略目录 `.local/pr-validation/`。

### 等战损药水成本胜利界

`POTION-COST-INCUMBENT` / REGENT / THE_INSATIABLE_BOSS / seed `PR215_POTION_COST_20261005`：最小原生合同，普通120秒上限，独立实例退出清理。两次Strike的真实完整胜利、实际药水成本14/9、成员内部与共享表保留较便宜分支、共享消融、同成本仍剪枝、缺失成本保留、零药完整胜利界仍剪枝，原生药水完整Continuation、父分支/live/RNG隔离。旧入口失败复现`0a69bcdd3c2644a6b9e73638fc153829`，修正后扩展合同`0bfdd959a37f49cda8f24f3adcf1a39c` Passed；长期入口合同`8cdb3b3ff63540c3ae04b52760d33065` Passed。20项primary-incumbent及143项early-turn-continuation合同Passed。未做拟提交组合版本完整性能、RSS或固定全根回归；不能据此认定可以正式合并。

最小入口：`bash tools/testing/run-unattended-test.sh --scenario-id POTION-COST-INCUMBENT --character-id REGENT --encounter-id THE_INSATIABLE_BOSS --seed PR215_POTION_COST_20261005 --timeout-seconds 120 --exit-on-complete --cleanup-instance-on-exit`。建局移除遗物/Power并注入两张Strike、两瓶原版药水；完整胜利只通过生产准备与回放入口得到，不注入伪造结果。
