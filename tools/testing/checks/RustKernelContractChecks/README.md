# RustKernel 协议契约失败关闭检查

```bash
dotnet run --project tools/testing/checks/RustKernelContractChecks -c Release
```

直接编译生产 `InCombatRustKernelMetadata.g.cs` 与 `InCombatRustKernelContracts.cs`，验证原生控制面响应（准入校验与结束回合风险复核）对畸形 JSON、截断负载、超限缓冲区（`> 1 MiB`）、缺省必要字段及不变量违背均按契约失败关闭且安全回退。无需游戏依赖。
