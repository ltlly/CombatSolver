# 更严格的剪枝来源审计：跨程序集事件引用

承接[免费根合同](exhaust-free-root-contract-20261007.md)，本轮发现并修复审计工具的跨程序集遗漏：Ritsu没有在自身定义原生牌堆事件，旧专用列表因此为0，但完整静态引用图实际包含10处原生事件访问。新增候选清单保留这些引用及程序集身份，所有候选仍标为未解析。没有新增生产剪枝或性能结果。[结构化证据](exhaust-event-reference-audit-20261007.json)保存输入、源码、21项元数据检查和正文片段。

## 输入与工具修复

源码基线8b272893，生产运行时保持27c28f66。游戏0.111.0 / 41cef1ea，SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`。Ritsu Runtime 0.6.6.0，SHA256 `fe387f765a3951690b161c129731e6c10016215c06074aa3356dd96290f13b52`，MVID `f7f18a36-8ebb-4db2-b1ea-7bd340579646`。

[HealingSourceAudit](../../tools/inspection/HealingSourceAudit/README.md) schema7新增`cardStateEventAccessorCandidates`：扫描七个明确原生类型的`add_`/`remove_`方法引用，覆盖本模块及依赖引用，保留调用token、操作码、程序集范围、参数/返回类型身份、实例标志及调用约定。原有本地事件定义/引用列表含义保持。候选可能只是同名普通方法，因此`DefinitionResolution`一律为`not-resolved`；不能按名字、所属程序集或列表数量认证。

| 输入 | 本地事件定义 | 本地访问引用 | 新候选引用 | 与schema6对照 |
| --- | ---: | ---: | ---: | --- |
| 游戏DLL | 21 | 82 | 82 | 原有35个数据字段完全相同 |
| Ritsu Runtime | 0 | 0 | 10 | 原有35个数据字段完全相同 |

10项包括5处订阅、5处退订，不是10个独立回调或10个可达搜索动作。工具限定七个类型，其他事件、反射、列表别名及外部回调仍须单独审计。

## 新增引用与原生正文

| 来源 | 引用数 | 已核对行为 | 认证状态 |
| --- | ---: | --- | --- |
| ModExtraHandPlayCoordinator | 2 | TryBegin注册捕获origin的CardRemoved委托；OnSourceCardRemoved调用ClearOrigin，修改`_active`、PendingOrigins及Closed，并退订 | 未认证；不能统一当成显示回调 |
| ModCardPileScreenViewController | 2 | Install订阅RefreshCards；回调按当前牌堆顺序调用NCardGrid.SetCards | 未认证；间接节点与扩展调用链未闭包 |
| NModCardPileButton | 6 | AttachPile订阅ContentsChanged及两个Finished事件；回调读取数量并修改文字或动画；DetachPile退订 | 未认证；完整UI调用链及未来可达性未证明 |

以上是当前安装依赖的静态候选与正文调查，不代表原版搜索实际使用额外牌堆，也没有新增第三方内容适配。ILSpy缓存含缺失引用/未知结果类型注释；片段不替代实际补丁、委托目标及执行顺序证明。

## 原生与分支入口的区别

原生CardPile.AddInternal在战斗堆且战斗进行时订阅Tracker的牌事件，即使`silent=true`也会订阅；silent只控制牌堆事件调用。RemoveInternal在战斗堆退订Tracker，再按silent决定调用CardRemoved、ContentsChanged和CardRemoveFinished。模拟SimCardPile只修改分支列表、所属堆和指纹缓存；首次进战斗及生成效果分别走分支事件接收器与AfterCardGeneratedForCombat镜像。不能从两边都“进堆”推导事件路径相同。

原生CardModel.DeepCloneFields重新附着克隆附魔/负面附着效果，随后AfterCloned才清除十个声明事件。模拟PredictionUtils在DeepCloneFields之前清除CardModel声明事件，以阻止复制来的观察者泄漏到live；这一步不包含继承的AbstractModel.ExecutionFinished。这里仅核对入口正文，没有执行新的克隆差分或认定继承事件错误。清理局部合同复用[已有原生证据](exhaust-cleanup-contract-20261007.md)。

这说明“根没有有效费用层”和“依赖没有定义事件”都不能推出未来没有观察顺序的状态。只有对剩余可达动作、生成/复制/回收、附着效果及回调写入形成封闭证明，才可合并不同顺序的分支。组件条件应当记录证明失败的来源，而不是把未审计来源归为安全。

## 下一步剪枝的证明要求

1. 主线程捕获实际委托的方法、目标、顺序及相关注册/补丁身份；后台只消费冻结条件和分支状态。
2. 生成、复制、进堆、回收和附着路径必须递归保持资格；新来源或登记变化使对应优化失效，继续原搜索。
3. 消耗堆顺序合并必须证明所有剩余观察者与写入者都与该顺序无关，包括延迟回调和继承ExecutionFinished；仅逐牌清理等价不够。
4. 战损上界剪枝仍以完整合规胜利为见证，保留原药水、成长、保命、遗物和资源追回政策。相等战损或其他目标无法证明时保留分支。

本轮没有形成整根证书；新增认证来源0、实际剪枝0。后续资格认证开销、完整协调器速度、战损、药水、回合数及峰值内存均未测量，不能把扫描耗时计作搜索开销。

## 验证与清理

最终工具构建1.98秒、0警告/0错误；初次相对Cecil路径被项目按不同目录解释，构建失败后改用已存在绝对路径，没有修改依赖。一次性元数据项目构建2.11秒、0警告/0错误。

21项Cecil检查通过：原有9项槽边界及5项歧义/token检查；新增7项覆盖本地列表不变、外部订阅/退订、程序集与版本同名、实例签名、仅创建委托、伪访问器不认证及token保留。游戏和依赖原有35项数据字段保持，限制说明保留旧前缀。没有新增CLR/原生/Fork/RNG或完整搜索测试。

一次性项目、源码、夹具及专用构建归档后删除，原始扫描和反编译缓存暂留.local。没有修改测试Executor或启动独立游戏实例。生产Mod源码未变，构建与五文件部署复用[已有成功证据](retained-victory-strict-hp-pruning-20261007.json)，不部署扫描器或测试DLL。全部原始慢场景的最终目标仍未完成。
