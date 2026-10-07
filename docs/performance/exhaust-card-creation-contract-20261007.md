# 剪枝来源原生合同：创建、复制及订阅清理

承接[跨程序集事件审计](exhaust-event-reference-audit-20261007.md)，本轮用实际游戏入口验证复制时点与创建回调。原生复制附魔牌会先调用复制来的订阅，再清理订阅；模拟提前清理。显式创建SpoilsMap也存在原生/模拟初值差异。两项边界复现、最终克隆完整状态、继承事件清理及16路复制隔离通过，仍未启用新剪枝。[结构化证据](exhaust-card-creation-contract-20261007.json)保存完整结果、一次性源码及实际方法身份。

## 来源与执行

源码基线c71d5d78，生产运行时仍为27c28f66。实际游戏0.111.0 / 41cef1ea，SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`。安装依赖来源复用[已捕获清单](exhaust-event-reference-audit-20261007.md)，不是其他版本经验。

冻结原始dev-06猎手首领生成器根，临时EXHAUST-CARD-CREATION没有调用Solve或Coordinator。runId `237e2c7c115d4a68837593bc6bbb23fe`，Passed，请求26.319162秒、启动器33.846516102秒，上限120秒；启动器成功删除独立实例。

最终诊断Release构建14.41秒、0警告/0错误。两次准备构建14.16/14.04秒各有一项一次性源码nullable警告；补充只读来源/牌池记录并修正余下访问后才运行最终DLL，没有运行旧版合同。取证后枚举原始JSON时遇到一个null文档；事实已保存，直接原生结果Passed不受影响。

## 原生结果

| 边界 | 实测 | 含义 |
| --- | --- | --- |
| 复制前订阅 | 脱离牌堆的Shiv附Inky，未知EnchantmentChanged写另一个脱离牌的ExhaustOnNextPlay；原生MutableClone调用1次，模拟额外调用0次 | 最终订阅为空不证明复制过程无跨对象副作用；这里是刻意注入的未知来源 |
| 最终克隆 | 把两边的克隆分别放入自有模拟分支，完整ContinuationStamp及富历史/RNG一致；附魔对象独立、Card指回各自克隆 | 指定最终状态通过，不是所有未来动作等价 |
| 继承事件 | 两边克隆的AbstractModel.ExecutionFinished订阅为0；调用克隆不触发源，源订阅保留且调用1次 | 基类AfterCloned确实清除继承事件；不能仅由CardModel的提前清理器判断最终结果 |
| 显式创建 | 实际live.CreateCard\<SpoilsMap\>的SpoilsActIndex=1，PredictedCard.Create为-1 | 创建新牌还执行AfterCreated，不能统一当成复制已有牌 |
| 已创建牌的复制 | 原生CreateClone和模拟玩法复制均保留SpoilsActIndex=1 | 新建与复制使用不同前置状态；不是重新调用AfterCreated |
| 当前根创建阶段 | 18个原生初始类型加显式Shiv/FranticEscape，共20类型，均解析到空CardModel.AfterCreated，实际0补丁 | 只覆盖这一个阶段和这些来源，不认证完整模型或生成闭包 |
| 当前牌池 | SpoilsMap.CanBeGeneratedInCombat=true、Quest稀有度，但不在实际53张解锁无色牌中，也不在初始来源 | 生成许可标志不是最终生成集合；其他入口、初始持有和复制/回收仍需分别审计 |
| 分支复制 | 16孩子串行Fork后并发做自有玩法复制、修改Inky数量2～17；清除下次消耗标志、附魔所有权及兄弟隔离通过 | 没有并发Fork同一可变父分支；16孙分支随后逐一核对完整状态、历史/RNG |
| 父/live恢复 | 父及种子完整状态保持，live续用保持，显式创建的两个浮动对象移除后原_allCards引用序列恢复 | 复制来的未知回调只改测试自有脱离对象，没有写根牌 |

实际方法MVID、token、IL SHA256及补丁总数保存于JSON。AbstractModel.MutableClone观察到3项补丁，不能因其他阶段0补丁认定整条克隆路径无扩展；本轮没有补丁owner/优先级/正文闭包证明。

## 对剪枝条件的影响

更严格的来源认证应分别处理规范新建、玩法复制、预测COW/Fork复制，以及从其他牌堆返回。未知订阅即便会在最终克隆中被清空，也可能在原生DeepCloneFields中提前执行，必须核对触发阶段、委托目标和可能写入。当前20类型的空AfterCreated可以复用为局部证据，不能替代生成后Hook、附着效果或全体观察者的证明。

SpoilsMap差异是显式Quest创建边界；本轮没有证明其可由普通原生随机战斗生成触达，也没有修改任务/地图语义或泛化模拟修复。不能把CanBeGeneratedInCombat=true当作原生所有生成器都会选它，亦不能因本次无色池没有它排除初始持有、复制、回收或其他生成器。

按机制形成封闭来源证明后，仍需使用完整合规胜利作为战损界，保留药水、成长、保命、遗物及资源追回比较。未知写入、未知未来来源和其他目标无法证明时继续原搜索。

本轮新增剪枝0；整根证书、完整回合/出牌队列、速度、战损、药水、回合数和峰值内存对照未验证。原生请求耗时与进程内存仅是诊断记录，不是性能验收；既有原生动作/下一回合合同按[原范围](exhaust-positive-native-contract-20261007.md)复用，没有为再次确认而重跑。

## 清理与部署

临时Executor逐字恢复，测试源码、props、启动脚本、备份及专用构建归档后删除；原始结果与只读反编译缓存暂留.local。生产源码未改，诊断DLL不部署；五文件部署复用[同源码成功证据](retained-victory-strict-hp-pruning-20261007.json)。工具schema7检查复用[上轮结果](exhaust-event-reference-audit-20261007.json)，本轮没有新增正式PR、版本提升或发布，全部原始慢场景目标继续保留。
