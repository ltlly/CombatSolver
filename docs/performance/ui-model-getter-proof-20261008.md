# 界面模型读取与刷新回调边界（2026-10-08）

**单字段模型读取可以局部证明，整个界面刷新仍需要独立认证。** 本轮确认两个真实getter只读一个字段，10次原生读取、5次辅助调用观察、16次拒绝及16分支伤害/Fork通过。完整节点查询和生命通知证书仍为0，未扩大生产剪枝。[配套记录](ui-model-getter-proof-20261008.json)保留实际IL、源码、原始请求、失败构建及回调清单。

## 来源与最小证明

从 `perf/common-state-cost-20261005` / `20a1e43c` 继续[牌堆定位子证明](native-pile-membership-proof-20261008.md)，正式运行时保持 `704dc994`。上一轮属于进展，本轮定位原生UI模型读取及刷新调用方，不重复牌堆合同或性能验收。

游戏 `0.111.0 / 41cef1ea`、MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`；Ritsu Runtime `0.6.6`、MVID `f7f18a36-8ebb-4db2-b1ea-7bd340579646`，原生CLR9.0.7。哈希复用对应实际DLL证据，新增读取NCardHolder；NCard、CardModel和全量IL复用当前版本缓存。

| 真实getter | 实际字段 | 原生IL |
| --- | --- | --- |
| NCardHolder.CardNode | `<CardNode>k__BackingField` | `027B963100042A` |
| NCard.Model | `_model` | `027B223100042A` |

两方法均为非static、非virtual、无参数、无局部变量或EH，IL精确为 `ldarg.0; ldfld; ret`。原型核对实际模块、字段类型及token解析结果，并在未知已发布补丁存在时拒绝，避免仅根据方法名或程序集身份放行。

该方法体条件只证明已创建对象上的getter调用。节点构造、附树、setter、信号、未来工厂及返回模型的后续方法都不属于同一证明。`NCardHolder.CardModel` 还是另一条virtual别名；本轮只覆盖所检查Ritsu路径直接使用的 `CardNode.Model`，不能把同名近似入口一起认证。

原型分别报告 `GetterLeavesClosed` 与 `CurrentModelSourcesBound`。后者读取当前字段，要求精确holder/card节点及当前玩家的原生模型绑定，只描述当前对象来源和owner，不表示模型本身没有回血能力。外围 `NativeNodeQueriesReviewed` 和 `WholeHealthCertificate` 始终为false。

## 原生观察与隔离

普通根有5个holder/model，两个局部条件通过。真实getter各调用5次，共10次，返回引用与实际字段一致。真实Ritsu `TryGetCardModel` 调用5次，也返回同一模型；旧式每敌战斗快照1份、父/live续用状态、历史及RNG保持。辅助方法观察通过不能代替其中Godot有效性、ready、tree查询的通用证明。

八类各在拟根/拟live检查一次，共16次拒绝：未知holder输入、空CardNode、未知节点类型、空模型、未知模型类型、非当前owner，以及两个getter的未知补丁。检查期间未知补丁调用0次，没有执行setter、未知模型方法或未知getter来判定安全。拟根/live仅为原型位置，未接入正式采用边界。

父分支串行创建16个孩子后，主线程将一个live UI节点的模型引用暂时换成另一张真实持有牌。各worker只读取先前不可变结果和自己的模拟状态，独立伤害1点，再检查自己的第二代Fork；完整分支状态/历史/RNG、父/live隔离通过。伤害预期HP从根冻结，没有在worker读live HP，也没有调用UI getter。finally恢复UI引用。该变化只用于隔离合同，不被声称为未知回复来源或完整UI生命周期。

## 刷新为什么还不能获得资格

当前源码中 `UpdateVisuals` 会进入画像、标题、能量/星能费用、附魔、动态变量预览和描述写入；手牌费用显示再调用CanPlay，高亮又有独立谓词。CanPlay调用Hook.ShouldPlay及资源/卡牌逻辑，这些入口需要核对实际接收者和回调，不能用getter证明省略。

复用同DLL全量IL，将十组实际刷新/费用/高亮调用方的175个直接引用与24个使用callvirt指令的模型调用目标记录下来。[实际定义核对](visual-slot-binding-proof-20261008.md)将其分为10个虚方法和14个非虚方法；callvirt指令本身不等于虚回调。清单不解析全部接收者、通知受众或合法未来，也没有将缺少直接Heal引用认作安全。另保存按方法名检索的声明清单；0项可能表示非virtual或方法名不存在，不表示该路径已经证明无副作用。

后续组合应继续核对Godot节点查询、实际费用/预览/高亮回调、描述和信号，以及未来生成/选择/回收来源。当前字段读取证据不能循环证明这些下层或后续能力。

## 验证与交付

唯一原生请求 `2bf808ff0fa44884903a065be574cd42` Passed：请求26.185秒、专门合同0.288秒、完整启动器墙钟33.738秒。首次构建因Godot派生夹具缺partial的GD0001失败，改正后构建0警告/错误；失败原型和日志保留。合同阶段包含全部检查、夹具修改和Fork，不是独立认证开销。

16为分支任务数，本轮没有极高/DOP16的Coordinator请求、峰值采样、实际剪枝或最终回归；战斗未结束，原生最终60/75生命，不形成整场质量结论。完整来源证明、所有原始慢请求两倍目标、魂枢原包恢复和历史战损缺口继续未完成。

临时源码、props、启动脚本、构建和路由副本归档后清理，Executor按原字节恢复。正式源码、Release/结构/工具/覆盖与五文件部署复用704dc994，只运行新文档门禁。没有新PR、合并、版本提升或发布。
