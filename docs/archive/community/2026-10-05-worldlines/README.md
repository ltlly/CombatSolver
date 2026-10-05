# 2026-10-05 更优世界线任务资料

已发布 Q011～Q015，共25场独立战斗、25个代表包，总折算预测战损改善量268 HP。按战斗session去重和折算改善降序选包；大、小候选分别降序，分散大任务后补齐每批五个条目，再按批次总量降序展示。

报告版本为0.49.1～0.50.0，查询窗口沿用最低版本0.44.0的未修复报告。检索快照共65份报告、65个session。逐包数字、资源证据、任务档位与回执见[选择清单](selection.json)。

| 批次 | 总折算改善 | 大任务 | 小任务 | 资料 |
| --- | --- | --- | --- | --- |
| [Q011](https://github.com/Torch1230/CombatSolver/issues/218) | 94 HP | 1 | 4 | [下载](https://github.com/Torch1230/CombatSolver/releases/download/community-tasks-2026-10-02/Q011.zip) |
| [Q012](https://github.com/Torch1230/CombatSolver/issues/219) | 59 HP | 1 | 4 | [下载](https://github.com/Torch1230/CombatSolver/releases/download/community-tasks-2026-10-02/Q012.zip) |
| [Q013](https://github.com/Torch1230/CombatSolver/issues/220) | 46 HP | 1 | 4 | [下载](https://github.com/Torch1230/CombatSolver/releases/download/community-tasks-2026-10-02/Q013.zip) |
| [Q014](https://github.com/Torch1230/CombatSolver/issues/221) | 44 HP | 1 | 4 | [下载](https://github.com/Torch1230/CombatSolver/releases/download/community-tasks-2026-10-02/Q014.zip) |
| [Q015](https://github.com/Torch1230/CombatSolver/issues/222) | 25 HP | 0 | 5 | [下载](https://github.com/Torch1230/CombatSolver/releases/download/community-tasks-2026-10-02/Q015.zip) |

每多用一瓶药扣9 HP。选中代表的报告、比较日志与保留结果中的战损和总用药已对账；25包开战材料与录制事件齐备，可读快照的模型类型属于原版游戏或.NET，单人身份为1，开战存档modifiers为空。公开诊断已去身份及个人路径，replay/*保持原字节。

证据等级均为shared_mechanism，提供具体动作窗口与路线差距。当前源码的恢复、模拟/部署正确性、实际战损、同根同政策同预算质量及耗时由认领者验证。材料准备的验证范围为静态核对。

批次说明：[Q011](Q011.md)、[Q012](Q012.md)、[Q013](Q013.md)、[Q014](Q014.md)、[Q015](Q015.md)。每批一人负责，五个条目在同一个PR中分别记录根因、改动与验收，全部通过后关闭批次。

[发布回执](publication.json)保存实际Issue与附件去向。发布后的代表报告清理结果写入对应回执，GitHub资料继续保留。

## 发布后清理

已按[逐报告回执](publication-cleanup.json)删除25条后台报告、25个服务器ZIP和0个COS对象。发布和清理的状态分别记账，任务验收由Issue/PR记录。

本地暂存删除被命令策略拦截，147个暂存文件中包含68个ZIP，保留在本次任务的忽略目录。具体状态见[本地保留记录](local-retention.json)。
