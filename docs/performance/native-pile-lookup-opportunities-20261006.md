# 原生牌堆查询与指纹关键词机会（2026-10-06）

当前原版查询的大多数重复发生在Snapshot内部，实际路径例子来自卡牌指纹先后读取诡诈／保留标志。新增三个固定根的观察和一次可选宿主模式；原算法继续执行，未建立永久空结果缓存，也未新增正式提速成果。原始慢根整请求两倍及10%峰值门槛继续有效。

[结构化证据](native-pile-lookup-opportunities-20261006.json)保存原生来源、三次请求、全部接收者类型/来源标签/阶段计数、首次路径例子、根与质量/工作核对；当前基点`dde487d5`，运行时仍为合入PR #213的`bb004176`／上游`9a4489d8`、v0.50.0。控制DLL SHA256 `2aa06f3e5adfc9e82492d2a43c806f2b3aa1964e92b9c250da013ffa2d68e45e`，原始产物在 `.local/tool-tasks/native-pile-lookup-20261006/`。

## 原生边界

使用实际安装0.111.0 DLL，沿用已记录MVID `8a76776c-0ce1-4d4f-90bd-8cce653dad8e`及SHA256 `2b40d2df538db1ceb5fa48d958c80ab730ada1e07db88a870aff01a661768b9f`。定向反编译AbstractModel、CardModel和CardPile，未重跑原回复来源审计。

原生`CardModel.Pile`沿owner.Piles按顺序调用Cards.Contains；生产替代路径在通知隔离和Ritsu牌堆注册冻结且无扩展时，先扫描战斗五牌堆，再扫描永久Deck。CardPile以readonly List保存成员，对外给IReadOnlyList；AddInternal、RemoveInternal、移动和Clear均会改变成员或顺序。基类源码没有Equals覆写，但这不是所有子类或外部相等回调的安全证明。只凭克隆身份、原版程序集或曾经查不到都不足以永久缓存空结果。

实际`CardModel.CombatState`除了牌堆战斗资格，还允许UpgradePreviewType=Combat返回owner.Creature.CombatState。因此Pile为空不等于CombatState必为空，优化不能漏掉预览类型和原回调边界。

三个离线原生见证均执行：通知隔离内克隆一张当前牌，查询为空；静默AddInternal加入真实Hand，查询返回该Hand；RemoveInternal后再次为空，完整live ContinuationStamp恢复。这里验证当前DLL在离线宿主中的实际入口，并非Godot原生/模拟整战斗差分。见证计数单列，不混入搜索统计。

## 可选诊断及实际观察

扩展现有SnapshotOpportunityProbe，`OFFLINE_HARNESS_SNAPSHOT_PROBE=pile-lookups`始终执行原Find，只记录其结果。弱键标签仅表示CloneCardStateForSimulation／CreateCard helper返回过该Model，不是离场证书。每行按模型类型、标签及Snapshot包含阶段计数，保存一个首次调用路径；结果只持有标量/字符串，弱标签不强留模型/模拟器图。Snapshot finalizer退出计数，不吞异常。未启用该模式时不安装这些补丁或创建诊断State。

宿主构建2.79秒、零警告/错误。7840H、8核16线程、约58GiB、CoreCLR9.0.19；三个固定根各一次Evaluate，VeryHigh/DOP16、25000节点、beam135、分支72/42/54、60000ms预算、十进制16GB配置NoGC、普通进程120秒上限。关闭其他探针／增量插桩；本模式有额外弱表、锁和StackTrace开销，时间/RSS不参与性能验收。

| 根 | 搜索查询总数 | 查询为空 | Snapshot内查询 | Snapshot内为空 |
| --- | ---: | ---: | ---: | ---: |
| 猎手396张牌 | 540679 | 540641 | 432460 | 432460 |
| 储君首领 | 344341 | 344293 | 272277 | 272277 |
| 亡灵首领 | 1385370 | 1385348 | 1210430 | 1210430 |

三根搜索共2,270,390次查询，其中2,270,282次为空；已打helper标签的查询在这批搜索全部为空，未打标签的38/48/22次查询返回牌堆。Snapshot包含阶段约占79%～87%，该阶段已观察查询全为空；这只是当前三个根与预算的结果，不能推断全部合法场景、未知类型或未来分支。

首次路径例子：Backflip、Acrobatics等卡牌的CaptureCardStateFingerprintForTesting → IsSlyThisTurn → Keywords → GetKeywordsWithSources → CombatState → Pile。两标志的原生getter各自读取完整Keywords，再检查单回合私有标志；指纹随后另写本地关键词。这里可以研究一次指纹内减少重复的完整关键词获取；本地关键词与全局关键词仍需分开，不能把仅Local的集合替代All。关键词或布尔getter补丁、预览类型及全局修改回调未知时继续原路径。

每个(type,helper标签,阶段)只保留一个首次StackTrace；将其按整行次数加权不是完整调用方画像，因此不把加权GetCombatState数当作所有调用的确定归因。此前[完整请求CPU](upstream-common-cpu-20261006.md)的Find最近归因4.270%沿用，查询次数不是CPU权重，也不能当作可实现的提速倍数。

## 核对与后续范围

复用已有同DLL、同根及预算的三个成功控制结果，没有再跑基线。三根完整根戳、9项Evaluate导出质量及展开/转移/选择工作量全部相同；comparisonQuality为空，未替代14维完整Coordinator质量验收。插入/移除见证的live恢复与这项核对分别记录，不能外推成所有状态/Fork/RNG原生证明。

本轮只交付可重复使用的可选观测能力与报告；生产源码和五文件部署保持`bb004176`，复用上游合并时的同源码成功证据。未新增运行时缓存、性能对照、最终全根回归、DOP1/2/16、Windows、可见Steam或魂枢长测；女王丢路仍按用户要求暂停。下一步从指纹内重复关键词读取入手，先建立版本/补丁/回调权限及最小严格差分，再检验实际收益。
