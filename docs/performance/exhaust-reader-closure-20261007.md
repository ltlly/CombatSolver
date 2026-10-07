# 消耗堆等价剪枝的回调槽与来源闭包（2026-10-07）

**下一步应建立按组件组合的正向证书，不能把“没有声明回调”直接当作安全条件。** 本轮把同名方法先验推进到实际IL虚方法槽，补全牌堆/卡牌/基类事件入口，并复核猎手重根的直接生成来源。没有启用新剪枝、宣称提速或扩大生产认证。[结构化证据](exhaust-reader-closure-20261007.json)保存工具结果、原生来源位置/哈希、九项元数据边界检查与明确未验证项。

## 来源和复用

继续`perf/common-state-cost-20261005` / `617a8a90`，运行时仍为`27c28f66`。游戏为0.111.0 / 41cef1ea，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`，SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`。工具扫描直接消费该安装DLL；既有缓存来源按原出处复用，新读取三个原生类，不根据其他游戏版本推断。

[上一轮保序投影](exhaust-projection-subsets-20261007.md)的六次完整请求，以及[8/6伤害反例](exhaust-equivalence-certification-20261007.md)及其Fork/原生差分已成功，输入没有变化，本轮没有重跑。旧潜在准入76679/21198次仍是只读机会统计，不能计作新剪枝或新增性能结果。

## 审计工具从名称到实际槽

`HealingSourceAudit` schema5增加：

- 原生Hook直接引用的180个AbstractModel入口，其中179个虚方法、1个非虚方法`InvokeExecutionFinished`。包括战斗外回调，不把180都称为战斗虚回调。
- 8340个模型虚方法的槽标志、显式override、模块内解析链、槽根和失败原因；8336个解析到本地槽，4个明确保留为外部`System.Object`入口。
- 21个卡牌/牌堆/附着/基类/玩家状态事件和82条静态订阅或退订访问。事件是否有活动订阅、委托体及反射扩展仍须另审。

继承关系同时检查程序集作用域、参数与返回类型；参数/返回中的泛型参数、数组/修饰类型及普通类型也保留作用域身份。外部同名类型、多个显式槽、协变返回或歧义不猜测；不跨越最近的不匹配基类去找更远的同名方法。完整CLR泛型替换、类型转发及方法有效性验证未实现，虚槽结果不能证明方法体、补丁或未来来源安全。

对卡牌声明槽定位到36条Hook虚方法重写、27个类型；相对上一轮29类名称先验，只排除了两个mock类型，没有新增此前遗漏的已知卡牌回调类。CardModel和EnchantmentModel没有声明重写这179个Hook虚槽，但继承的默认调用及其他生命周期入口仍存在。两个重根各18个初始卡牌类型均无声明Hook重写；这只是一项元数据事实。

相对schema4的29个旧payload字段完全相同，原limitations前缀保持。源码变化对应五次工具构建/扫描，均零警告/错误；不把扫描本身当作认证覆盖。

## 默认回调仍然经过事件

原生`Hook.AfterCardExhausted`逐监听者执行`PushModel → AfterCardExhausted → InvokeExecutionFinished → PopModel`。即使某张牌继承默认空回调，仍可能触发`ExecutionFinished`订阅，因此只检查虚槽是否重写不够。

同版本静态引用仅定位到NPlayerHand的`ExecutionFinished`订阅/退订；尚未复核该UI委托体或排除动态扩展，不能从目录归属推导无副作用。原生`AbstractModel.AfterCloned`清空该事件，`CardModel.AfterCloned`另清空自身十个事件；这不是对当前影子复制全部路径的新增验收。

新定向读取还明确：

| 入口 | 实际行为及安全前提 |
|---|---|
| `NetCombatCardDb.StartCombat/OnPileContentsChanged` | 按牌堆顺序给未登记牌分配递增ID；所有牌已登记时不重分配。不能把牌堆事件统称为纯UI。未来生成和当前身份映射须保留。 |
| `CombatStateTracker` | 事件汇集后延迟重算附着效果并通知CombatStateChanged；TestMode若有该事件订阅会拒绝。可见实机的活动订阅及重算来源仍需审计。 |
| `PlayerCombatState.RecalculateCardValues` | 遍历AllCards的Enchantment.RecalculateValues，包括消耗牌；Inky继承默认空实现，但未知附着不能沿用。 |
| `PlayerCombatState.EndOfTurnCleanup` | 按AllCards清理牌上临时标志/费用并可能触发事件；逐牌字段修改不等于完整回调可交换。 |
| `CardPileCmd.Shuffle` | 当前本体只合并Draw/Discard，不消费Exhaust；其Hook及未来回收来源仍须闭包。 |

这些是调用片段结论，不是完整根认证。

## 猎手根的直接来源范围

读取冻结根的18类初始牌、4种遗物、2瓶药水及相关叶Power/生成体。69份来源位置与哈希仅表示可用证据，不计作69份完整模型认证。

| 来源 | 本体实际产生或修改 | 对闭包的作用 |
|---|---|---|
| `FanOfKnives` | FanOfKnivesPower，并直接生成Shiv | 不引入随机卡池；Shiv目标受该Power影响。 |
| `BladeOfInk` | 直接生成Shiv，再给这些新牌Enchant Inky | 当前无附着不够；证书必须显式包含未来Inky及新牌身份。 |
| `TheInsatiable.LiquifyMove` | 直接生成6张FranticEscape到Draw/Discard，并施加SandpitPower | FranticEscape虽CanBeGeneratedInCombat=false，敌人的显式生成仍可达。 |
| `MasterPlannerPower` | 给刚打出的技能加Sly | 修改关键字，不能把牌永久视作不可变；本体不回收Exhaust。 |
| `LuckyTonic` | 施加BufferPower | 本体不生成药水或卡牌，不能根据名称假设随机来源。 |
| `Fortifier` | 按目标当前格挡GainBlock | 本体不生成卡牌/药水。 |
| `TheHunt` | Fatal成立时增加战后CardReward和TheHuntPower | 并非战内生成卡牌或获得药水；搜索外奖励边界须保持。 |

这组本体直接生成的新增卡牌类型只有Shiv/FranticEscape；连同18类初始牌构成20类直接来源候选。**尚未证明所有根Power、modifier/badge、待返回牌、扩展订阅和私有引用均满足这些前提，故完整生成闭包认证仍为0。** 不能据此给任意猎手、任意首领或原版程序集默认通过。

亡灵根的JackOfAllTrades消费经解锁/玩家约束及原生战斗过滤的无色池，需要另做递归闭包。当前Apotheosis为Ancient，原生FilterForCombat排除该稀有度，不能将未过滤卡池中的它直接列为JackOfAllTrades可达；初始持有和其他显式来源单独检查。

## 验证、决定与下一步

一次性Cecil元数据夹具先检查8个边界，补充参数类型程序集身份后最终检查9个：继承链、参数不同的新槽、同名隐藏、外部同名父类、外部同名参数、协变返回、最近协变基类不能跳过、显式改名槽、多显式槽拒绝。全部通过；它不执行游戏语义，部分刻意构造异常元数据，不冒充原生/Fork合同。夹具源码/输入/结果归档后删除项目、源码与构建目录。

下一候选按实际组件认证：保存回调/附着/未知牌槽位及每类内部完整顺序，正向封闭全部可达读者和生成规则；根上拒绝未知初始/待返回来源、活动订阅及相关补丁，分支发现未认证新来源时保留原搜索。原完整键继续服务排序/续用/原生对账，转置投影独立维护。只有这些前提、实际身份/回调、完整状态/Fork/RNG/live差分成立，才接入真实剪枝并测量收益。

本轮新增完整证书0、实际剪枝0、游戏原生测试0、纯性能请求0、最终回归0。文档/工具门禁通过，未变的结构、覆盖、原生行为和五文件部署证据复用[当前运行时](retained-victory-strict-hp-pruning-20261007.md)。两倍目标及历史战损缺口仍未完成；没有新PR、合并、版本提升或发布。Windows、可见Steam与完整Mod栈未验证。
