namespace CombatSolver;

internal sealed partial class CombatBeamSolver
{
    // An exact optional-potion layer can affect the Smart selection only if it
    // meets the existing HP-saving threshold. This is an eligibility ceiling,
    // not an incumbent route. The closed Prism environment cannot generate a
    // cheaper potion, heal, increase max HP, or recover stolen resources.
    private readonly int? _smartPotionEligibilityHpCeiling =
        directSearchPurpose == DirectSearchPurpose.SmartPotionGradient
        && policy.PotionPolicy == SolverPotionPolicy.Smart
        && !policy.PotionStrategy.HasForcedDirectives
        && policy.TheftPolicy != SolverTheftPolicy.PreserveResources
        && root.PlayerIdentity.Character.GetType() == typeof(MegaCrit.Sts2.Core.Models.Characters.Defect)
        && root.Enemies.Count > 0
        && root.Enemies.All(enemy => enemy.Monster?.GetType() == typeof(MegaCrit.Sts2.Core.Models.Monsters.InfestedPrism))
        && CanUseStrictHpRelicBound(root.CanCertifyRemainingHealing
                && root.InitialRemainingHealingUpperBound == 0,
            policy.EffectiveHasGrowthTargets, policy.RelicTargets)
        && root.SearchablePotions.Count > 0
        && root.SearchablePotions.All(potion => potion.PotionId
            == MegaCrit.Sts2.Core.Models.ModelDb.Potion<MegaCrit.Sts2.Core.Models.Potions.FoulPotion>().Id.Entry)
        && minimumPotionUses is > 0 && maximumPotionUses == minimumPotionUses
            ? SmartPotionEligibilityHpCeiling(potionFreePolicyBaseline,
                minimumPotionUses.Value, root.MinimumSearchablePotionStrategicCost!.Value,
                root.PotionRewardOutlook.ReplacementHpCredit,
                ActEndingBossPolicy.ResolveStrategicHpRelief(root.BossHpRelief,
                    policy.ActTransitionBossHpStrategy, policy.FinalBossHpStrategy))
            : null;

    private int _smartPotionEligibilityBranchesPruned;

    internal static int? SmartPotionEligibilityHpCeiling(
        PotionFreePolicyBaseline? baseline, int exactPotionUses,
        int minimumPotionStrategicCost, int replacementHpCredit, BossHpRelief bossHpRelief)
    {
        if (baseline is not { Won: true, DeathSaveUseCount: 0 } audited
            || exactPotionUses <= 0 || minimumPotionStrategicCost < 0)
            return null;
        int minimumCost = checked(exactPotionUses * minimumPotionStrategicCost);
        int requiredHpSaved = PotionUsePolicy.SmartRequiredHpSaved(
            PotionUsePolicy.ApplyReplacementCredit(minimumCost, exactPotionUses, replacementHpCredit),
            bossHpRelief);
        return audited.HpDeficit - requiredHpSaved;
    }

    private List<SearchNode> ApplySmartPotionEligibilityBound(List<SearchNode> retained)
    {
        if (_smartPotionEligibilityHpCeiling is not { } ceiling)
            return retained;
        List<SearchNode>? bounded = null;
        for (int index = 0; index < retained.Count; index++)
        {
            SearchNode node = retained[index];
            if (StrategicHpLowerBound(node.Snapshot, _strategicBossHpRelief,
                    RemainingHealingPotential(node.Snapshot)) > ceiling)
            {
                if (bounded is null)
                {
                    bounded = new List<SearchNode>(retained.Count);
                    if (index > 0)
                        bounded.AddRange(retained.GetRange(0, index));
                }
                _smartPotionEligibilityBranchesPruned++;
                continue;
            }
            bounded?.Add(node);
        }
        return bounded ?? retained;
    }

    private void EmitSmartPotionEligibilityBoundDiagnostics()
    {
        if (_smartPotionEligibilityHpCeiling is { } ceiling)
            policy.Diagnostics.Info($"[CombatSolver/Test] SMART_POTION_ELIGIBILITY_BOUND "
                + $"ceiling={ceiling} pruned={_smartPotionEligibilityBranchesPruned}");
    }
}
