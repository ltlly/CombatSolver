# 费用查询来源与局部算术证明（2026-10-08）

100个原生局部费用算术案例、10次未知拒绝和16分支费用修改/Fork通过。规范牌反例同时证明：原方法提前返回、显示费用仍1，Ritsu后置能力却让HP60→62。现有生产全局能力准入正确拒绝该未知能力；完整费用和生命证书仍0，没有生产剪枝或性能结论。

从`40f28058`继续[可打出查询来源研究](can-play-source-proof-20261008.md)，正式运行时和部署保持`704dc994`。[结构化记录](energy-cost-source-proof-20261008.json)保留实际定义、源码片段、失败输入、原始原生结果及一次性代码。

## 原生与框架路径

当前原生CardEnergyCost.GetWithModifiers先读基础费用，并对规范牌、负费用、X费用提前返回；普通牌依次执行局部修饰器和全局Hook。全局Hook遍历两轮受众，分别调用早期、后期费用覆写，始终使用out modifiedCost，不能根据bool false省略实际效果。[既有默认输出合同](default-value-out-proof-20261008.md)按原前提复用，没有重新执行。

实际Ritsu EnergyCostPatch给GetWithModifiers安装transpiler和postfix。全局调用被改写，局部能力可能在Hook前执行；当不计算Global或Card.CombatState为空时，postfix还能执行ApplyEnergyCost。因此规范牌的原始提前返回不能绕过后置能力，不能按原方法的返回条件推导整个入口无副作用。

读取同一安装DLL的15个原生费用虚方法声明：两个基础方法、13个覆写，其中11个正常游戏类型和两个Mock。覆写包括BorrowedTimePower、CorruptionPower、CuriousPower、FreeAttack/FreePower/FreeSkillPower、TangledPower、VeilpiercerPower、VoidFormPower，以及BrilliantScarf、SpikedGauntlets。片段只定位效果和下层依赖，不认证Card.Type、Keywords、Pile、Owner及数据getter。VoidForm的内部数据、BrilliantScarf的动态变量与回合状态仍须单独核对，不能凭未看到直接Heal就放行。

## 局部证明

LocalCostModifier.Modify及Amount、Type、IsReduceOnly三个getter都是非虚方法。原型核对实际游戏MVID、四个精确IL哈希和已发布补丁，只从原生字段冻结amount/type/reduce；未知枚举在调用前拒绝。合法绝对/相对与reduce-only路径只读字段并做整数加法、Math.Min，无玩家引用或生命写入。标准当前CLR/BCL是明确前提，任意BCL补丁或detour未认证。

100个原生案例覆盖两种合法类型、两种reduce、五个剂量和五个输入，包括正负极值及unchecked溢出。一个已创建的外部派生对象隐藏Modify/Amount，原生非虚基方法仍返回7，隐藏方法调用数0；这不认证其构造器或未来生命周期。

四个未知入口补丁及非法枚举在拟议root/live位置共10次拒绝，未知检查调用0。完整快照、父/live状态、历史与RNG保持。

预建16个孩子时根费用为0，主线程将live局部费用临时换成999；孩子仍读到根费用，随后各自修改费用为1～16并二代Fork，完整状态/历史/RNG相等。恢复live字段后父/live完整戳保持。另把算术证据源对象的Amount从2改为500，worker消费的冻结值仍2，未用变化的live值。这是当前所属费用状态合同，不授予完整未来来源资格。

## 原生规范牌反例

对实际规范牌`DefendRegent`确认IsCanonical=true、非X费用、CombatState为空，选中原方法提前返回分支。人工向其原生能力宿主附着ICardEnergyCostContributor，它返回原费用、只在明确调用时额外回复2。

现有PredictionRitsuCapabilityAudit先拒绝该规范来源；明确执行一次原生GetWithModifiers(All)后，费用1→1、HP60→62、能力回调一次。能力及快照缓存随后恢复，独立实例清理。反例后的HP变化是预期，不是完整live等价通过；未运行错误剪枝或实际玩家路线。

这要求未来生成/复制所用规范来源也单独检查，同时审计所有入口补丁。原方法和返回数值相同都不能替代效果证明。局部修饰器算术证书不能认证这个顶层调用。

## 结果与清理

游戏`0.111.0 / 41cef1ea`、MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`；Ritsu Runtime0.6.6、MVID `f7f18a36-8ebb-4db2-b1ea-7bd340579646`。安装哈希及Shared身份复用原对应审计。实际费用transpiler/postfix都属于`com.ritsukage.sts2-RitsuLib.framework-core`。

原生`34ad206025ee4edc8d46d289f7833670` Passed，请求26.127秒、启动器34.254秒；最终构建零警告/错误，DLL哈希与原生结果一致。耗时包含启动、检查和插桩，不作为完整Coordinator耗时或提速。独立认证开销与纯性能样本0。

准备失败分别保留：Cecil/PE元数据类型重名、去掉命名空间后丢失PEReader扩展、临时夹具误用文档示例的ThisTurn枚举及变量重名；修正为明确命名空间、实际EndOfTurn后才启动原生。新goal消息后functions存储丢失导致一次相对目录查找失败，未编辑文件或重启进程，随后恢复原工作树。元数据工具成功导出四个定义；MSB3539路径警告保留，清理按实际task/obj中的项目归属。

源码/脚本/props/临时路由归档后移除，Executor按原始字节恢复，只读反编译和原始结果保留。正式运行时、Release/结构/工具/覆盖门禁及同源码五文件部署沿用已验证成果，新文档执行文档门禁。

还未认证全局受众迭代、全部未来费用来源、星能/超额能量、预览/高亮/标签、Godot信号及生命通知闭包。没有新完整搜索、固定回归、实际剪枝、PR、版本提升、合并或发布；全部原始两倍目标和已接受的场景例外保持。
