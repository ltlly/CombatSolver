using CombatSolver.HotSearch;
using MegaCrit.Sts2.Core.Combat;

namespace CombatSolver;

internal readonly record struct LiveEndTurnRiskProjection(
    int HpBefore,
    int HpAfter,
    int HpLost,
    bool PlayerDead,
    string MonsterMoves);

internal static class LiveEndTurnRiskEvaluator
{
    public static LiveEndTurnRiskProjection Evaluate(
        CombatState state,
        IReadOnlyList<PlanCardChoice>? turnStartChoices = null)
    {
        if (SolverSettings.Current.UseRustCombatKernel
            && InCombatRustKernelBridge.HasInstalledBackendArtifact()
            && !InCombatRustKernelBridge.IsCorrupt
            && InCombatRustKernelBridge.RunInCombatHookForTesting is null
            && InCombatRustKernelEligibility.AssessHostEligibility(state).IsEligible)
        {
            try
            {
                BattleDamageSnapshot damage = BattleDamageTracker.Observe(state);
                (RustKernelLiveStateDto liveState, _) = InCombatLiveStateCapturer.Capture(state, damage);
                InCombatRustHostMetadata meta = InCombatLiveStateCapturer.CaptureHostMetadata(state);
                RustKernelEndTurnChoiceDto[] mappedChoices = MapChoices(turnStartChoices);
                var req = new RustKernelEndTurnRiskRequestDto(
                    AbiVersion: RustKernelAbi.Version,
                    LiveState: liveState,
                    CharacterId: meta.CharacterId,
                    Ascension: meta.Ascension,
                    GameMode: meta.GameMode,
                    DominionId: meta.DominionId,
                    RunModelIds: meta.RunModelIds,
                    EncounterIsWeak: meta.EncounterIsWeak,
                    InitialEncounterMonsterCount: meta.InitialEncounterMonsterCount,
                    TotalFloor: meta.TotalFloor,
                    ActIndex: meta.ActIndex,
                    MapCol: meta.MapCol,
                    MapRow: meta.MapRow,
                    HasMurderousModifier: meta.HasMurderousModifier,
                    TurnStartChoices: mappedChoices);

                if (InCombatRustKernelBridge.TryEvaluateEndTurnRisk(
                        req,
                        out LiveEndTurnRiskProjection rustProjection,
                        out string? failureReason))
                {
                    return rustProjection;
                }

                Entry.Logger.Info(
                    $"[CombatSolver/InCombatRustKernel] END_TURN_RISK_FALLBACK reason={failureReason}");
            }
            catch (InCombatLiveCaptureUnsupportedException ex)
            {
                Entry.Logger.Info(
                    $"[CombatSolver/InCombatRustKernel] END_TURN_RISK_FALLBACK code={ex.FailureCode} detail={ex.Message}");
            }
        }

        return HotEndTurnRiskEvaluator.Evaluate(state, turnStartChoices);
    }

    private static RustKernelEndTurnChoiceDto[] MapChoices(IReadOnlyList<PlanCardChoice>? turnStartChoices)
    {
        if (turnStartChoices is null || turnStartChoices.Count == 0)
            return [];

        var mapped = new List<RustKernelEndTurnChoiceDto>(turnStartChoices.Count);
        for (int i = 0; i < turnStartChoices.Count; i++)
        {
            PlanCardChoice c = turnStartChoices[i];
            if (c.Timing is not (PlanChoiceTiming.PlayerTurnEnd or PlanChoiceTiming.EnemyTurn))
                continue;

            var cards = new RustKernelChoiceCardTokenDto[c.Cards.Count];
            for (int j = 0; j < c.Cards.Count; j++)
            {
                PlanCardToken t = c.Cards[j];
                cards[j] = new RustKernelChoiceCardTokenDto(
                    t.CardId,
                    t.UpgradeLevel,
                    t.OptionOccurrence,
                    t.SourceOccurrence);
            }
            mapped.Add(new RustKernelEndTurnChoiceDto(
                c.Timing.ToString(),
                c.SourceId,
                c.Effect.ToString(),
                c.SourcePile.ToString(),
                cards));
        }
        return mapped.ToArray();
    }
}
