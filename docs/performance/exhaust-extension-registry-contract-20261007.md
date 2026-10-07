# 剪枝扩展来源：注册清单与原生变更合同

继续[真实根与扩展审计](exhaust-root-extension-audit-20261007.md)，本轮在原始猎手首领根中捕获此前未覆盖的Ritsu注册表。最终原生来源盘点、注册变化检测、一次原生克隆回调及16个初始Fork隔离检查通过。尚未建立完整耗尽顺序等价证书，实际新增剪枝0，没有整请求速度、峰值内存或固定回归结论。[结构化证据](exhaust-extension-registry-contract-20261007.json)保留最终临时源码、失败记录、根清单与依赖入口。

## 原生结果

沿用游戏0.111.0 / 41cef1ea的实际DLL，哈希与MVID复用上一报告。使用相同冻结生成器请求`dev-06-silent-boss-scenario.json`，保留实际卡组、遗物、药水与RNG；临时入口`EXHAUST-EXTENSION-REGISTRY`只做根与扩展检查，不调用Solve、Coordinator或战斗路线。

最终runId `3c34de9b673945aab849d1086d8e392a`，Passed；请求记录26.3530199秒、启动器33.968230912秒，普通上限120秒。最终Release构建15.41秒、0警告/0错误。上述诊断时间不是优化速度。

| 来源 | 初始根实测 | 证据范围 |
| --- | --- | --- |
| typed生命周期订阅 | 24项；CardExhaustedEvent订阅0 | 按事件类型和调用次序记录方法签名、MVID、token及目标字段；未证明全部回调语义 |
| 通用生命周期observers | 0 | 从独立注册表捕获，不能用ModHelper计数代替 |
| 克隆监听器 | 1项，`secondary_resource_costs` | 记录predicate/listener及typed包装器；内部委托尚有不透明引用 |
| 默认能力注册 | 0 | 从Modifiers捕获；未覆盖所有其他能力生产规则 |
| 已有能力集合 | 0个owner、0项能力 | 只读取原生已存在集合，不创建集合 |
| 保存槽 | 0，注册已经finalized | 当前安装DLL的Register在finalized后拒绝新增槽；不是任意版本或其他保存机制证明 |
| 已有附加数据 | 58个owner | 读取现存bag字段；不执行导入或生成新bag，不是全部数据内容认证 |

新增一个未知CardExhaustedEvent订阅后，typed订阅数24→25，清单变化；解除订阅后恢复。新增一个未知克隆监听器后，监听器数1→2，清单变化；原生CardModel.MutableClone确实调用该新增监听器一次，随后解除登记。

最终清单与初始清单相同，冻结记录、完整父分支状态戳/丰富历史及live状态保持。16个子分支串行Fork后并行读取初始状态，完整状态/RNG/历史和父/live隔离通过；没有修改这些子分支，没有新增并发Fork、两代Fork或卡牌动作差分。

一次初始来源捕获为4.5402ms，包含冷反射和格式化；不是完整证书的成本，也不是重复采样或后台查询成本。

## 纯读取边界

安装的Runtime.dll中，ModelSavedDataRuntime.TryGetBag找到现存bag后会调用EnsureImported，可能运行适用保存槽的导入。因此本轮直接读取其底层AttachedState.TryGetValue。该实现实际位于Shared.dll，通过ConditionalWeakTable只取已有值；共享程序集哈希及方法正文单独归档。

克隆、默认能力、保存槽及通用observer清单使用对应System.Threading.Lock保护读取；typed生命周期topic读取自己的原子handler快照。捕获阶段位于主线程，后台只读取本轮冻结记录。各注册表分别捕获，不声称跨注册表原子一致性或注册变更的永久epoch保护。

元数据仍有明确限制：目标对象与不透明字段使用进程内RuntimeHelpers.GetHashCode标识，不是无碰撞身份；嵌套引用没有深拷贝内容，嵌套值类型无法解释时明确标记未审计。清单变化检测不能替代目标状态/方法语义的认证，整根证书始终false。

## 当前克隆入口的下一步

安装DLL的SecondaryResourceCloneBridge注册名为`secondary_resource_costs`的typed CardModel监听器，调用CopySecondaryCostsTo和CopySecondaryResourceUsesTo。对应正文读取源牌的附加费用/使用集合，存在层时把Clone结果写入目标牌；两种集合的Clone都建立新字典并复制各列表。这些入口正文没有枚举耗尽堆。

这只定位源码对应的注册入口。当前根的typed包装器内仍有不透明委托，未核对实际内部方法token、全部层元素、目标拥有权或未来写入来源；不能直接发放整根顺序证书。模拟引擎已有克隆路径跳过consumer通知，其明确边界继续复用，没有改变实机克隆或第三方支持范围。

后续应先解析实际内部委托及附加状态，再与[已审计的原生读者及生成闭包](exhaust-reader-closure-20261007.md)组成正向证书。只有证书完整且在分支变化后有效，才实现[有限耗尽顺序合并](exhaust-projection-subsets-20261007.md)，随后测实际剪枝、整请求时间和峰值内存。

## 失败记录与交付范围

启动器首次在进入游戏前因诊断目录缺少manifest退出；补齐原manifest后重新启动，没有重做成功构建。

原生`6827a552d9fd45119d3b86461cb4ec0f`在ConcurrentDictionary枚举转换中失败；改为读取泛型原子快照。`b4ae27734c9c4daaa43446fc9584b745`的聚合收尾断言失败，缺少分类清单，不能确定是哪一项差异。后续递归字段诊断`b3fe418592cf419d8d87d7b928e1c166`以进程139退出，没有结果文件；退出原因未确认。最终限制读取深度、明确处理原生整数并分类记录断言后通过。全部失败保留，不计入通过或性能统计，也不声称已经解决一个生产崩溃。

临时执行入口逐字恢复，任务源码、props、备份及诊断构建清理；各独立原生实例已删除。只读反编译缓存及原始调查日志暂留`.local`供继续研究，临时源码和失败证据已归档。

生产运行时保持27c28f66，诊断DLL未部署；五文件部署、运行时结构、覆盖和未变工具证据沿用[现有成功结果](retained-victory-strict-hp-pruning-20261007.json)及[元数据工具验证](exhaust-reader-closure-20261007.json)。本轮交付为文档与证据，不新增达标场景或正式性能候选。
