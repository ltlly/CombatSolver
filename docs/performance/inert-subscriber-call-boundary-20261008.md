# 订阅器默认回调的实例通知边界

**类型没有覆写战斗回调，不能证明它的实例没有战斗效果。** 当前原生调用方仍会分派实例的 `ExecutionFinished`；基线准入接受了带受众的实例，一次回合结束派发触发三次通知。此次把推断准入收紧为主线程逐实例检查原生事件受众，保留没有受众的原有类型，未知受众继续明确拒绝。[结构化证据](inert-subscriber-call-boundary-20261008.json)保存成功、失败、输入、源码和部署来源。

## 版本、原生路径及范围

沿用 `perf/common-state-cost-20261005` / `4f00da2e`，之前正式行为为 `36370e54`。游戏0.111.0 / 41cef1ea，DLL SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`；实际Godot运行库.NET9.0.7及RitsuLib0.6.6身份沿用输入未变的既有审计。

原生 `ModHelper.IterateAllCombatStateSubscribers` 只枚举订阅委托及非空模型；`CombatState.IterateHookListeners` 直接追加这段后缀。它们不调用 `ShouldReceiveCombatHooks`。因此本轮没有根据 getter 的潜在副作用扩大拒绝，源码中“默认回调仍是空操作”的旧说明已纠正。

`Hook.BeforeSideTurnEnd` 三阶段分别调用默认方法，并经 `HookPlayerChoiceContext.AssignTaskAndWaitForPauseOrCompletion` 分派完成事件。实际默认Task和调用方绑定证据复用[上一轮报告](task-caller-order-proof-20261008.md)。这次额外验证类型准入与实例受众的区别。

两个测试模型由 `RuntimeHelpers.GetUninitializedObject` 构造，只用于类型、事件和默认回调，不注册ModelDb、不挂载真实ModHelper列表。代理只提供这些接收者和原玩家，调用实际原生Hook及已安装Ritsu补丁。根订阅器验证直接调用原有准入函数；不是整次根捕获、完整战斗或全堆状态对账。

正式Mod清单声明不影响玩法；为了实际走到推断准入，最终前后两个隔离测试清单显式声明 `affects_gameplay=true`。它们只存在于测试构建/实例，正式清单仍为false。

## 修正与回归

- `PredictionModHookSubscriberInertness` 保留按类型缓存的覆写元数据；新增实例检查，只在主线程根捕获读取 `AbstractModel` 声明的真实事件字段。
- 原生字段不存在、字段类型不匹配或受众非空，都无法由这一推断路径准入；检查不会执行事件或getter，不缓存可变结果。
- `PredictionModHookSubscriberCapture` 的推断路径消费实例重载。显式已知适配及非gameplay清单的既有放行政策保持。
- 既有 `VerifyPredictionFailureBoundaries` 扩展十种类型及四种实例合同：初始空、添加后拒绝、移除后恢复；包括泛型继承、地图方法及派生同名字段。实例均属于测试，没有新增搜索/Fork状态。

| 最小原生验证 | runId | 状态 | 完整请求耗时 | 启动器耗时 |
| --- | --- | --- | ---: | ---: |
| 修改前，影响玩法测试清单 | `4082980184814c46a214d0e7dbefca12` | Passed，复现错误准入 | 26.2809729s | 33.8429707s |
| 修改后，相同测试清单与根 | `8154a2ae511a415e95443bd0f6cae8ec` | Passed，实例/根验证拒绝及移除恢复 | 26.2080542s | 33.7252928s |

两次getter读取均为0、同一受众收到三次原生完成通知，live捕获戳相等。修改后十种类型和四种实例断言全部通过。首轮探针错误要求原生读取getter而失败；首个候选遗漏using编译失败；另一次候选测试因非gameplay绕过推断入口而失败。材料均保留，纠正夹具后取证，不把失败记作生产已通过。所有启动器成功清理独立实例。

这些请求是建局和最小机制验证，CPU准入2、启动器上限120秒，没有启动Coordinator搜索。表中时间不能用作性能对照；终态HP56/最大70是原根值，不表示此次承受14战损。

## 对后续剪枝的意义

继续采用完整合规胜利作为见证，并且由已闭合来源的分支回复上界证明严格更差战损才剪枝；等战损以及药水、成长、遗物、追回和保命目标继续按现有比较器处理。类型归类只是一项准入前提，不能替代回复证书。

[消耗堆投影观察](exhaust-projection-subsets-20261007.md)已有潜在重复机会，但尚不能把它们当作实际剪枝。下一项证明应组合可达来源、原生调用方、实例事件与有序读者，并保留同类型内部顺序。不能根据没有显式覆写、原版程序集或几条相同轨迹默认认证；Eidolon、消耗堆回收顺序和未知跨卡通知的已有反例继续作为排除条件。

当前严格回复证书的订阅器来源使用独立的固定审计身份，泛型惰性判定不会自动扩大它。本轮没有新增完整未来证书、转置合并或实际剪枝，认证开销未单独测量；完整Coordinator性能、全场景质量和峰值RSS回归均未新增。此修正不计入两倍提速验收。

## 交付

普通最终Release构建14.74秒、零警告/错误；结构、工具及覆盖目录门禁通过。一次性路由恢复，源码和构建配置已存入JSON后清理。已精确覆盖正式本地Mod五文件，DLL SHA256 `017b3f4d24b8c5cfaadb606f9aaecba44ca9b4bd929170f4a3ff4f7cc8920725`，版本保持0.50.0；没有新PR、自动合并或发布。
