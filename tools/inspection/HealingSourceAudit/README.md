# 原生生命及资源入口扫描

读取指定游戏 DLL 的 IL 元数据，列出生命值命令、内部写入口、直接字段写入和字段取址；同时保存静态方法/委托引用、泛型模型参数、异步方法映射、回复相关回调、全部原版模型虚方法、Hook 调用入口和模型定义。不会加载或执行游戏模型，不会修改 DLL。

schema3另列药水获取/消费/丢弃/槽位容量入口、槽位字段全部访问、Player与PotionModel事件订阅，以及药水相关模型Hook定义。字段读取同样保留，因为List可在`ldfld`之后改变，只读接口也可能返回原列表别名。事件、生成和间接获得遗物的调用仍需人工复核，不能从零回复认证推导库存不变。当前调查见[库存与后续计划研究](../../../docs/performance/potion-inventory-pruning-research-20261006.md)。

schema4追加牌堆命令、完整牌堆/消耗堆读取、返回精确牌堆类型的方法及原生战斗监听枚举入口，保留CardPile自身字段，以及精确CardPile类型或其数组/泛型包装字段的全部IL访问；不将名字含CardPile的其他结果类型算作牌堆根。返回值扫描也覆盖CardModel.Pile和各牌堆getter的间接别名。它帮助定位有序列表别名、根字段和间接读写，不把只读接口、已消耗或没有直接取牌调用视为无交互证明。方法/委托图、原版模型虚方法和异步映射继续复用原格式；反射、扩展、递归生成和完整未来等价仍须单独证明。

构建时传入已安装 ILSpy 的 `Mono.Cecil.dll`，不把该依赖或游戏二进制提交到仓库：

```bash
dotnet build tools/inspection/HealingSourceAudit/HealingSourceAudit.csproj -c Release \
  -p:CecilAssemblyPath=/path/to/Mono.Cecil.dll
dotnet .local/tool-build/HealingSourceAudit/bin/Release/net9.0/HealingSourceAudit.dll \
  /path/to/sts2.dll .local/healing-audit/native-references.json
```

输出包括输入 SHA-256、MVID 和方法数量。保留大型扫描产物于 `.local/`，提交的审计只保留复核所需元数据、来源哈希及人工语义判断。

调用列表只是调查入口。它不能证明目标是玩家、调用实际可达、回复次数有限或生成集合封闭；设置生命还可能是损血、敌人初始化、读档或同步。静态图也不能解决虚调用、反射与外部 Mod 的回调。必须结合当前版本的完整原生实现、生成过滤及原生/模拟差分，未知来源继续拒绝认证。
