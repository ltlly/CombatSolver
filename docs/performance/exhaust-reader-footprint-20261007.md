# 耗尽顺序研究：读牌方法与调用边界

本轮把[来源闭包](exhaust-reader-closure-20261007.md)的原生IL清单与[根来源调查](exhaust-root-extension-audit-20261007.md)连接，得到24个候选读牌方法、37处引用，并核对关键方法正文。候选引用不等于耗尽顺序依赖，调用图也不是安全证书。生产认证仍未扩大，新增剪枝0；没有新原生、Fork、整请求速度或峰值内存验收。[结构化证据](exhaust-reader-footprint-20261007.json)保存生成脚本、引用及调用路径。

## 固定来源与清单

实际安装游戏0.111.0 / 41cef1ea，DLL SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`。复用同DLL的schema5槽位/事件/引用清单，本轮新增反编译CardSelectCmd和SingletonModel；未替换游戏、搜索配置或政策。

| 项目 | 数量 | 含义 |
| --- | ---: | --- |
| 选定来源类型 | 42 | 旧根调查的调查名单，包含界面/徽章等；不是已认证模型集合 |
| 起始方法 | 434 | 来源声明方法、部分基类生命周期及180个原生Hook模型入口的保守并集 |
| 图中到达方法 | 5262 | 静态引用、异步状态机和选定模型虚派发扩展；包含非搜索阶段 |
| 选定虚派发边 | 145 | 补足选定模型及抽象基类继承，不代表所有动态接收者已证明 |
| 牌堆相关引用 | 259 | 元数据清单中调用者被图覆盖的引用 |
| 候选读取 | 37处 / 24方法 | 按ExhaustPile、AllCards、AllPiles、Cards、GetCards名称筛选，仍需正文及阶段核对 |
| 未知继承链 | 0 | 仅针对选定42类型的元数据继承链 |
| 未解槽位记录 | 42 | 全部追溯至System.Object；不是42个已发现战斗错误，也不作为安全依据 |

早期脚本只用具体模型建立继承链，会在抽象基类提前停止。最终加入allVirtualModelMethods中的基类，并依据SingletonModel实际声明补齐其AbstractModel父类；新增识别EndOfTurnCleanup。旧粗清单与最终清单分别保留，不混用计数。

## 正文核对与剪枝含义

| 方法片段 | 当前安装DLL行为 | 对顺序证明的意义 |
| --- | --- | --- |
| CardModel.Pile | 在Owner.Piles中查找包含此卡的堆 | 此片段查成员归属；不能据此认证整个CardModel |
| CardSelectCmd.FromHand | 明确选择Hand.Cards，再应用过滤与选择流程 | 不能把该入口误算成普通耗尽堆选择；其他选牌入口仍独立审计 |
| Flechettes、PreciseCut的变量函数 | 读取手牌计数，后者考虑自身是否在手牌 | 仅此变量计算片段不依赖耗尽堆排列 |
| DrawInternal、ShuffleIfNecessary | 实际抽牌读Draw顶部；普通洗牌检查Draw/Discard | 抽牌顺序保持精确；未发现此普通洗牌入口直接洗Exhaust，不涵盖Hook副作用 |
| PlayerCombatState.AllPiles / AllCards | 按Hand、Draw、Discard、Exhaust、Play串联 | 将耗尽堆接入全牌有序遍历，不能因牌堆离场而忽略 |
| RecalculateCardValues | 对AllCards逐卡调用Enchantment.RecalculateValues | 原生基类空实现、当前Inky未覆盖该方法；仍需下游补丁与可达附着来源证明 |
| EndOfTurnCleanup | 遍历全牌，清临时标志/费用，可能发出费用变化事件 | 每卡写入本身不足以证明操作独立，事件接收者可能观察中间状态 |
| CardPile.AddInternal / RemoveInternal | 修改有序列表，维护Tracker订阅，并发出牌堆事件 | 事件、插入位置与批处理输入顺序都必须保留或证明不可观察 |
| CardPileCmd.Add | 按输入顺序迁移卡，处理位置、随机插入及进堆Hook | 随机流和回调不能只用最终牌堆多重集代替 |
| CombatStateTracker.OnCardValueChanged | 排队延迟通知，之后重算牌值并通知CombatStateChanged | 延迟不等于无副作用；还需实际订阅者、补丁和执行阶段证明 |
| RemoveDeadPlayerCardsFromCombat | 要求turn非空、有战斗状态，且并非所有玩家死亡；随后遍历五堆移除 | 单人真实死亡时条件为false；这是条件片段，尚未证明所有入口阶段不可达 |

网络、存档和界面方法出现在保守图中，不表示搜索一定执行它们。本轮没有用“不像战斗方法”排除入口。ldftn只表明方法可能用于委托，取消订阅等路径也会引用它，不能当作实际回调执行证据。

## 如何使剪枝更合理

顺序合并应以未来可观察行为为依据：先保持卡牌、附着效果、历史、RNG及现有目标标签，只合并已证明未来读取和写入无法区分的排列。计数或成员查询可以成为局部证明；有序自动出牌、返回、事件或随机插入必须保序。不能以“当前没有回血”“原版模型”或“此类没有覆盖Hook”直接认证整根。

当前缺口仍是实际事件/委托接收者、接口/反射/外部扩展、牌堆字段别名、补丁调用链及未来写入/生成闭包。选定42类型并非完整可达接收者集合；该脚本的虚槽处理也不替代完整CLR派发证明。只有这些前提成立且分支持续满足资格后，才能接入转置合并；未知来源继续使用原有有序键。已有[反例](exhaust-equivalence-certification-20261007.md)、[指定轨迹正例](exhaust-positive-native-contract-20261007.md)和[真实克隆绑定](exhaust-clone-binding-contract-20261007.md)按原范围复用，不能扩写成通用认证。

本轮只做静态调查，未测认证耗时、实际剪枝或性能收益，也未改变评分/预算/药水政策。临时生成脚本归档后删除；只读反编译缓存和原始清单暂留.local供后续闭包工作。生产运行时仍为27c28f66，同源码构建和五文件部署复用[既有成功证据](retained-victory-strict-hp-pruning-20261007.json)，没有部署诊断DLL或重复既有成功检查。尚未新增两倍达标场景。
