# 测试记录归档 22

本卷保存截至2026-10-06已结束的公共状态成本研究、撤回原型及诊断批次，原生结果、失败与未验证项按原范围保留。归档不表示本轮复测或整体性能目标完成。

## 性能研究分支


2026-10-06 [空共享见证表查询](../../performance/shared-incumbent-empty-research-20261006.md)：`165855ef32ce4b89915844eea37adb7a`原生合同验证空表同一候选／完整状态／RNG及首次发布恢复；实际14／9成本、同回合及更晚便宜路线、相同／未知／零成本、父live对账通过。12次Evaluate未建立共同收益，源码及专属接入撤回；CPU取原trace，未追加完整请求／最终14维或增量／DOP验收。

2026-10-06 [Fork监听布局复用](../../performance/fork-listener-layout-research-20261006.md)：原生监听合同含16并行兄弟／两代Fork、COW与类型变更拒绝，卡牌续接完整状态／历史／RNG合同通过；第一项Passed后启动器身份竞态单独记录，进程及实例已结束。12次Evaluate保持根／政策／9项质量及逻辑工作，时间无共同收益；独立消费计数及JIT不进入性能，原型撤回，未追加完整请求／14维最终回归／增量／DOP验收。

2026-10-06 [原生牌堆查询观察](../../performance/native-pile-lookup-opportunities-20261006.md)：三次离线实际AddInternal／RemoveInternal见证、完整live续用戳恢复和Evaluate观察；复用三个控制的根／9项质量／工作相同。没有原生Godot新合同、完整性能或14维最终验收；可选宿主模式不改生产。

2026-10-06 [可选监听投影存储](../../performance/external-listener-projections-research-20261006.md)：两版监听合同含完整状态/RNG/Fork/父live/弱生命周期；存在位版实际CARD-CONTINUATION-CONTRACT（`30cfcfce42b942f8809569d72b86e11b`）通过；第一版SEARCH只属helper。16次整请求交错收益较小或不稳定，均撤回，未追加最终七根/DOP1/2/16，原部署来源保持。

2026-10-06 [当前CPU及lane分组诊断](../../performance/upstream-common-cpu-20261006.md)：宿主构建零警告/错误，一次完整请求CPU与两次Evaluate观察通过；发现同指纹历史偏移反例，无生产缓存、原生新验收或提速/RSS结论。

2026-10-06 [静态牌值公共缓存](../../performance/intrinsic-card-value-research-20261006.md)：四项原生合同、24次Evaluate、24次完整Coordinator；九根14项导出质量一致，两个精英根RSS超门槛，原型撤回。拒绝后未追加最终DOP1/2/16，复用上游合并版本既有部署。

2026-10-06 [稀疏回调位置研究](../../performance/sparse-hook-positions-research-20261006.md)六次原生请求、36次Evaluate、14次完整Coordinator；大牌组首版1.043倍但峰值+12.673%，逐回调及调度扩展未证明收益，三版撤回。结果仅对应PR #213之前的源码；完整14维与未获胜控制的范围分别保留，未追加最终回归。

既有组件、药水、魂枢及未达标原型的合同与历史结果见[历史卷18](../../archive/testing/volume-18.md)。PR #207最新上游整合、逐次ABBA、23根品质回归及NoGC波动/超时限制见[当前验收](../../performance/pr207-upstream-0494-integration-20261005.md)。

[公共Fork及牌堆缓存研究](../../performance/fork-pile-cache-research-20261005.md)新增三次原生宿主机制合同与36次Evaluate交错，未建立稳定收益，原型全部撤回；对应完整Coordinator、最终8根回归及DOP1/2未追加执行，旧成功部署继续按原来源复用。

2026-10-06 [牌值存储与根资格](../../performance/intrinsic-value-storage-research-20261006.md)：两项最小原生合同覆盖603牌/16Fork/完整父liveRNG、五getter补丁及真实Ritsu附着/写入/移除回退；24次Evaluate、4次交错完整请求和12次最终哨兵保留。对象分配大小相同但猎手首领RSS+12.158%失败，两版撤回；未追加最终增量/DOP1/2、Windows或可见Steam。

2026-10-06 [战略上下文枚举](../../performance/strategic-context-enumerator-research-20261006.md)：`STRATEGIC-CONTEXT-DEMAND`原生`202f7281e44a43f79143a018f4eb908a`通过603卡、需求/集合/异常/Dispose/Count顺序、真实getter版本错误、16Fork与完整父/live/RNG，实例删除；两次准备失败保留。12次Evaluate仅小幅浮动，撤回原型，未做整请求性能或最终固定回归。

2026-10-06 [未达标首领CPU](../../performance/unresolved-roots-cpu-20261006.md)：储君/亡灵完整Coordinator、FixedBudget=false、VeryHigh/DOP16各一份新perf诊断正常完成，窗口23428/27540样本、丢失0；政策、根与14项质量单列，不能当候选A/B性能或原始质量验收。源码/部署不变，未执行新原生或固定回归。

2026-10-06 [展开前胜利界观察](../../performance/pre-expansion-incumbent-opportunity-20261006.md)：原生`POTION-COST-INCUMBENT`／`d7f2e6305746405f9ab47e5f78b998de`通过现有完整原生合同与额外查询状态/计数断言，实例删除。两根完整Coordinator及大牌堆Evaluate诊断分别261/74175、0/78635、0/11111机会；亡灵工作变化未归因，不作性能验收，临时观察清理。
