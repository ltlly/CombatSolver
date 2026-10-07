# 耗尽顺序研究：扩展费用写入与方法身份

本轮沿[清理回调合同](exhaust-cleanup-contract-20261007.md)的两个实际后置补丁核对免费绑定与附加资源。发现容器自身Changed回调、清理入口创建状态及免费查询缓存写入，补充未来来源证明的条件。同时修复IL审计工具以FullName作为唯一字典键造成的扫描失败。14项托管元数据边界通过，游戏原有33项输出保持；未跑新游戏合同、启用新剪枝或做性能验收。[结构化证据](exhaust-extension-writer-audit-20261007.json)保留失败、检查、临时源码及写入清单。

## 固定输入与工具修正

游戏0.111.0 / 41cef1ea，SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`；Ritsu Runtime 0.6.6.0，SHA256 `fe387f765a3951690b161c129731e6c10016215c06074aa3356dd96290f13b52`，MVID `f7f18a36-8ebb-4db2-b1ea-7bd340579646`。输入与前轮绑定合同对应。

原schema5工具首次扫描Runtime以退出码134终止，原因是GetRitsuDynamicEnumEntries的两个定义显示相同FullName。实际token为100679889/100679890，泛型参数数量为0/1；它们是可区分的方法，不是发现了游戏运行时错误。

schema6保留文本碰撞的全部定义，不选第一项。静态方法引用及异步映射记录CallerDefinitionToken和OperandMetadataToken；与输入模块MVID配合解释。operand可能是MemberRef或MethodSpec，不能当作已解析MethodDef。按文本定位的槽位遇到方法/类型碰撞时保守拒绝；继承链和显式override不默认安全。

工具Release构建2.20秒，0警告/0错误。实际Runtime扫描覆盖5057类型、36800方法、175356方法引用，记录1组文本碰撞、0组类型碰撞。新版本游戏输出对照旧schema5，33项原字段在移除新增token字段后逐项一致，旧限制说明前缀保持，游戏方法/类型文本碰撞均0。

已有9项继承、同名隐藏、外部同名、协变及显式槽边界因工具源码变化重跑通过；新增5项覆盖普通/泛型同文本、显式槽碰撞、重复声明类型、重复基类及两个调用点token。共14项是Cecil元数据检查，不执行CLR方法或游戏行为。一次性项目构建保留MSB3539中间目录警告，检查本身通过；正式工具构建没有该警告。

## 写入和回调的新边界

| 入口 | 当前安装实现 | 认证要求 |
| --- | --- | --- |
| ClearCardFreeThisTurn / AfterPlayed | CardStates.Update调用GetOrCreate；清理即使返回false也可能创建空绑定 | 审计查询不能调用清理代替只读检查；容器不存在与零次数须区分 |
| FreePlayBindingRegistry.Resolve | PlayStates.GetOrCreate/Set缓存解析结果 | “查询”名称不等于纯读，后台不能借此读取或改变live绑定 |
| EvaluateRegisteredDetectors | 锁内复制检测器，随后逐个调用Func<CardPlay,bool> | 需要根上的实际方法、目标、注册变化与语义证明，不能只看容器数量 |
| SecondaryResourceCostSet / PlayUseSet | Set/Require、ClearDuration及重置可Invoke各自Changed | CardModel事件清单不包含这两个容器事件；无卡牌订阅不够 |
| 容器Clone | 新字典、列表复制；层元素共享，Changed未复制 | 不代表所有层成员/回调不可变，也不支持未知附加资源语义 |
| Secondary资源清理后置 | 先看全局ModSecondaryResourceRegistry.HasAny，再读取牌的费用/条款容器 | 需证明注册、既有层与未来写入；当前无实际收费不能代替无层 |
| 免费清理后置 | 清理返回true时刷新NCard视觉；Refresh先检查Card.Pile | 脱离牌堆的前轮正例不覆盖实际牌堆上的视觉链；反编译提示仍保留 |

原模拟通知/免费绑定隔离实现按既有范围复用；它们没有证明所有附加资源回调、注册检测器及未来写入无副作用。即使原生DLL没有直接引用Ritsu类型，也不能证明无扩展，因为实际后置补丁建立了跨模块调用链。

## 潜在写入清单

按五个明确的免费/资源类及已核对API名，筛选Runtime静态引用并向调用者追溯：35个被内部引用的写入/创建/缓存入口、43处直接引用、62个潜在调用方法。再与既有原生补丁名称清单连接，得到6个可能桥接，涉及4个原生目标名称及153个潜在原生调用者：

- CardModel.SetToFreeThisTurn和SetToFreeThisCombat。
- CardModel.OnPlayWrapper的免费与附加资源消费后置。
- CardModel.EndOfTurnCleanup的免费与附加资源清理后置。

这是按名称和静态边建立的调查清单，不是完整写入集合或搜索阶段可达证书。没有内部引用的公开入口、字段/集合别名、接口、反射、外部调用者和动态委托仍需单独检查；不会因公开入口没有静态入边就排除它。补丁连接使用既有类型/方法名称，没有重新证明重载、全部补丁身份或执行顺序。清单的Runtime范围没有碰撞方法节点，但图仍不能代表完整动态调用语义。

下一项需要根上免费检测器/绑定、资源定义与容器回调的只读捕获，并审计上述写入入口与已知生成规则的联系。当前生产认证覆盖未扩大，整根证书false，新增剪枝0；认证开销、实际合并、速度及峰值RSS均未测。

## 清理与交付

错误FreePlayCardVisuals命名空间导致的反编译失败保留，随后从正确Patches命名空间得到正文。一次性元数据夹具、项目、生成脚本及构建/输入归档后删除；只读反编译缓存、清单和原始日志暂留.local。正式工具留在原inspection职责目录，README同步schema6。

生产Mod源码及运行时仍为27c28f66，本轮仅修改审计工具与文档；构建、五文件部署及原生/Fork证据按[现有成功范围](retained-victory-strict-hp-pruning-20261007.json)和[清理合同](exhaust-cleanup-contract-20261007.json)复用，没有重复未改变的游戏测试或部署。工具检查不计作战损/药水/回合数/内存通过，尚未新增两倍达标场景。
