# 回复来源闭包盘点与负伤害结算修正

五个目标根目前都没有严格组件回复证书。盘点全部拒绝来源后，下一批应按生成、抽牌、重放与成长机制补齐闭包，不能只放行每个场景的首个拒绝类型。本阶段先修复审查中发现的原生／模拟扣血差异；没有扩大认证或取得新的整请求提速验收。

基于任务分支 `4fad73ce`，上游基线 `5c773caa`。安装游戏为 `0.111.0 / 41cef1ea`，`sts2.dll` MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`、SHA-256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`。来源哈希、原始命令、失败、最终原生结果、全部回归数字及一次性源码见[结构化证据](pruning-source-closure-and-hp-clamp-20261009.json)。

## 投入选择与盘点范围

复用此前撤回原型的 CPU 记录细分快照成本：`Snapshot` 占整次采样 33.094%，其中状态键、威胁生命投影与牌价值分别有多处成本。它只用于选择研究方向；采样带有原型，嵌套占比不能相加，也不是当前正式版本性能验收。连续小型调用优化未达到筛选门槛后，本轮先检查能减少搜索工作量的回复证书缺口。

临时离线观察调用现有 `ComponentInitialCard`、`ComponentRelic`、`ComponentPower`、`ComponentEnemy` 和根监听盘点，不修改谓词。五次根读取均核对完整预测状态前后、live／根及预测／根文本相等。DOP1、100节点／1000毫秒、Beam1／分支4属于诊断输入，不能当作极高／DOP16验收。

| 根 | 首个拒绝 | 拒绝的不同初始牌类型 | 遗物类型 | Power类型 | 敌人类型 |
| --- | --- | ---: | ---: | ---: | ---: |
| holdout-04-necrobinder | CallOfTheVoid | 9 | 3 | 0 | 0 |
| screen-necro-projected | CaptureSpirit | 18 | 15 | 1 | 1 |
| silent-large-deck | Greed | 11 | 0 | 1 | 1 |
| screen-huge-deck | Havoc | 13 | 0 | 0 | 1 |
| screen-test-subject | CreativeAi | 10 | 0 | 2 | 1 |

表格只计现有类型谓词的拒绝；不是未审查模型总数或全游戏认证率。例如 `SpiritOfAsh` 的类型已在审查表，但其 Swift 附着仍被拒绝。裸怪物谓词拒绝 Osty，而组合根逻辑已按所属玩家接受自己的宠物；CccComboModel、DebufferModel、MultiplayerScalingModel 由已有框架规则处理，不能把观察中的空分类算成未知来源。库存与既有再生政策没有改动。

五根全部保留无限回复上界，严格根覆盖率仍为 **0/5**。没有新增认证、新增证书剪枝或认证开销测量；现有剪枝计数不归功于本轮盘点。

## 下一批机制审查

以下12个来源覆盖 holdout-04 的初始牌／遗物缺口。已复核实际 DLL 正文及依赖，但尚未形成新增生产证书；候选玩家回复界为0的结论仍须完整回调、可达生成及分支证明。

| 来源 | 对象、时点及可重复性 | 继续证明的边界 |
| --- | --- | --- |
| CallOfTheVoid | 出牌施加玩家Power；每次玩家抽牌前按层数逐次生成 | 原角色解锁池、单人、原生过滤与递归生成闭包；不能批量改变RNG |
| CaptureSpirit | 伤害敌人后向Draw逐次插入3／4张Soul；复制、重放及回收可重复 | Soul抽牌、插入RNG与通用抽牌回调 |
| DanseMacabre | 玩家Power按已支付费用门槛给格挡；可叠加、反复触发 | BeforeCardPlayed、格挡命令及后续回调 |
| HiddenGem | 普通随机生成禁用；初始持有时可反复增加Draw牌重放次数2／3 | 原筛选、RNG、复制／回收以及其他回复来源的重复执行 |
| PanicButton | 玩家格挡与NoBlockPower；消耗牌仍可能复制、重放、回收 | 通用格挡、Power与消耗回调 |
| ReaperForm | 玩家或自己的宠物攻击后给受伤目标Doom；按层数反复触发 | Doom、伤害、Power和视觉通知完整调用链 |
| SculptingStrike | 敌人受伤后选手牌加Ethereal；每次执行可触发 | 选牌、关键词变化、消耗及返回 |
| Sow | 对所有对手造成伤害；Retain、复制和重放可重复 | 群体伤害、击杀和死亡回调 |
| TheScythe | 敌人受伤后战斗牌及DeckVersion伤害增长5／7 | 初始／生成角色、永久牌组、成长记录与零HP额度；同HP保留 |
| BeatingRemnant | 玩家回合受伤计数、20点上限与回合开始重置 | 伤害修改、通知、Osty溢伤与隐藏计数；扣血原生命令截零 |
| GamePiece | 玩家Power牌完成且战斗进行时抽1张；每次满足条件触发 | 重放时序、洗牌、选牌和已有／未来抽牌来源 |
| MealTicket | 活着进入商店房间时给遗物owner回复15 | 当前战斗搜索不推进房间；不能标记为该遗物永不回血 |

CallOfTheVoid等已有源码哈希与[此前生成闭包审计](regent-potion-cap-bound-semantic-audit-20261003.json)一致，游戏DLL哈希也相同；只复用其原适用范围。未找到固定哈希的HiddenGem／Soul及新遗物、ReaperFormPower、Creature、DamageResult直接从当前DLL提取。Feed、NotYet、Alchemize及HiddenGem普通原生生成资格为false；初始持有、复制和回收仍独立检查。不能用生成角色的证明直接放行带DeckVersion的初始成长牌。

## 首个错误状态与修正

原生 `Creature.LoseHpInternal` 将伤害截在 `[0, 999999999]`；影子状态只限制上限，负值会增加生命。未改行为源码的原生基线 `5962ebfaa70f45edb5fffb4c0cb8c6ed` Failed：HP75输入−3.25，原生保持75／UnblockedDamage0，模拟得到78／−3。

`SimCreatureState.LoseHp` 现使用相同上下限，不新增状态、缓存或评分规则。审查 BeatingRemnant 的“20减已受伤计数”表达式提示了这个边界，但本轮没有证明普通战斗自然到达超上限计数，也没有执行完整原生伤害Hook管线。因此不声称修复了指定玩家路线，更不把负值模拟增加生命当作合法回复。

正式 `HP-LOSS-CLAMP` 合同直接调用安装DLL的原生扣血方法：初始HP75／1／0、格挡0／7、两组ValueProp及14种负数、小数、零、致死、封顶和decimal极值，共168组；比较HP、MaxHP、Block、HpDisplay、存活状态与DamageResult全部字段，并核对原生生命通知。16个孩子串行Fork后并行修改，验证负伤害完整状态不变、正伤害被下一代Fork保存及父／live／完整续用／指纹／历史／九RNG隔离。

最终原生 `1d3ea54e0cc849acabc4d2e8a2e236c2` Passed，合同阶段102.642毫秒。它证明扣血原语与分支隔离，未模拟完整Hook结算。失败和成功实例均按启动器清理。最小复跑入口：

```bash
bash tools/testing/run-unattended-test.sh \
  --scenario-id HP-LOSS-CLAMP --character-id IRONCLAD \
  --encounter-id FUZZY_WURM_CRAWLER_WEAK --seed COMBATSOLVER \
  --combat-solver-build-dir <Release构建目录> \
  --verify-combat-root-snapshot --stop-after-combat-root-snapshot-assertion \
  --timeout-seconds 120 --exit-on-complete --cleanup-instance-on-exit
```

## 固定回归与交付

最终Release零警告／错误，DLL SHA-256 `7510a4876b8096e7ae2612676a6b76a0601903b3cace7cfafe2d748d6e287af8`。11个固定根各执行一次完整Coordinator，VeryHigh、DOP16、500000节点、300000毫秒、Beam135、分支72／42／54、Smart、组合及NoGC16GB。离线宿主CLR9.0.20，AMD7840H／16逻辑处理器／58GiB；输入、根牌序／RNG、政策与预算均和已有合格控制一致。

全部14项质量相等，包括胜负、原始战损、战略缺血、药水成本／数量、敌HP、分数、回合、存活、失窃资源、复活消耗与成长。10根工作量完全相等；猎手精英有少量调度工作差异，但质量相同。无超时或时间边界。进程峰值RSS最大比值 **1.002997（+0.30%）**，低于+10%门槛。

本次复用已有合格控制，只新增修改后回归，没有串行交错的多次性能验收；单次整请求时间比范围0.9580～1.0176不构成新增提速结论。完整各次数字在配套JSON，既有猎手首领54／69波动没有被宣称解决。

精确部署manifest、最终DLL、Windows MemoryCleaner及两份许可，保留旧五文件备份；版本仍0.50.1。临时宿主观察已逐字恢复，两份一次性脚本在配套JSON保存后删除。不改变PR #235范围，不推送、合并、发包或发布。

后续先完成这12项及相关回调的原生／分支闭包，再接入现有根证书和incumbent消费者，测实际剪枝、认证成本与整请求串行交错收益。所有原始慢根的两倍、质量和内存目标继续有效；本阶段没有完成总目标。
