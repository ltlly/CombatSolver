# 虚方法槽绑定与常量调用证明（2026-10-08）

25张当前战斗牌的四个原始虚方法槽共检查100次：75次常量调用通过，25次因实际Ritsu补丁拒绝。没有新增完整界面或生命证书，没有接入生产剪枝，也没有性能验收。

基于`e3497960`继续[界面模型读取研究](ui-model-getter-proof-20261008.md)，正式运行时保持`704dc994`。原始结果、失败输入及一次性源码见[结构化记录](visual-slot-binding-proof-20261008.json)。

## 修正调用分类

复用此前175条界面调用引用，解析24个模型目标的实际方法定义：10个虚方法、14个非虚方法。`callvirt`指令也能调用非虚实例方法，不能据此识别实际覆写槽或证明无副作用。此前报告的“24个虚方法”标签已修正；原始归档字段保留并标明更正。

10个虚方法包括Pool、TargetType、HasStarCostX、IsPlayable、内部金/红高亮、附魔金/红高亮、能量描边颜色和DynamicVar.UpdateCardPreview。14个非虚方法仍需逐方法核对效果，不能自动认证。

## 局部证明与原生验证

检查器从原始`MethodInfo.GetBaseDefinition()`定位接收者实际槽，核对实际游戏模块、原始槽及实际方法的已发布Harmony补丁，并只接受无局部变量/异常区的`ldc.i4.0/1; ret`布尔方法。检查期间不执行未知覆写、补丁或能力。

四个槽为CardModel内部金/红高亮、IsPlayable，以及AbstractModel.ShouldPlay。当前25张战斗牌的75次已认证调用与原始槽的CLR派发结果一致；完整快照、历史和RNG保持。全部25次IsPlayable检查因真实Ritsu postfix拒绝，不能记为非恒定方法或认证通过。牌组、临时离场对象及未来生成对象不在此次覆盖范围。

同名隐藏夹具使用已分配但未运行构造器的外部派生对象。它的`new ShouldGlowGoldInternal`没有覆写原槽；原槽实际调用基础`false`方法一次，隐藏方法调用数0。这只证明这一次方法调用，不证明外部构造器、初始化或未来生命周期安全。

五种注入未知来源在拟议root/live入口共10次拒绝，未知调用数0。预先创建16个自有分支，每分支执行一次伤害及二代Fork；同时更改live界面模型引用，分支使用冻结根HP和所属模拟器，父分支/live完整状态、历史和RNG保持隔离。正式root/live入口未集成这个原型。

## 实际补丁与失败记录

游戏为`0.111.0 / 41cef1ea`，MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`。Ritsu Runtime为0.6.6，MVID `f7f18a36-8ebb-4db2-b1ea-7bd340579646`。哈希复用未变化的实际安装审计，见结构化记录。

IsPlayable的postfix属于`com.ritsukage.sts2-RitsuLib.framework-core`，实际类型为`CardModelCapabilityPatches+IsPlayablePatch`。已读源码调用`ApplyCanPlay → ApplyLastOverride → ModelCapabilityHost.GetCapabilities<ICardPlayStateContributor> → CanPlay`；能力返回值或异常处理不能证明无回复副作用。当前能力宿主、默认来源及未来附着需另行认证。

初次编译成功但有CS0628警告，该产物未执行，随后去掉夹具的sealed。第一次原生请求`7e89c243e1814470b56a917563b648c3`在复合断言失败，缺少详细诊断，不能推断错误派发。第二次`25800912db044e85b334f4e7c638d1f9`明确显示槽绑定正确，但IsPlayable因真实补丁返回unknown-patch、未调用；夹具误把保守拒绝当成错误。最终改用无补丁的金色高亮槽验证同名隐藏，保留补丁拒绝门槛。

最终`b76fd41c058c4bfaad6b148693980f76` Passed，请求26.273秒、启动器33.882秒，编译零警告/错误。耗时含启动与诊断，不是整Coordinator搜索耗时，也不是提速结果。三个原始结果均保留。

## 交付范围

一次性代码/脚本/临时路由归档后移除，Executor按原始字节恢复。正式代码及五文件部署保持`704dc994`，复用同源码Release、结构、工具、覆盖检查与既有部署；只对新文档执行文档门禁。

完整费用、预览、高亮Hook、描述、标签、Godot查询、界面信号及未来来源闭包仍未认证。实际生产剪枝0，独立认证开销样本0，纯性能样本0；没有新增回归或PR，不提升版本、不合并或发布。
