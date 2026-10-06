using CombatSolver.Engine.Common;
using CombatSolver.Engine.InCombat.Simulation;
using MegaCrit.Sts2.Core.Entities.Players;

namespace CombatSolver;

/// <summary>Frozen initial inventory within the separately audited no-acquisition closure.</summary>
internal sealed class ClosedInitialPotionInventory
{
    private readonly record struct Slot(int Index, string Id, int Cost);
    private readonly string?[] _initialIds;
    private readonly Slot[] _byCost;

    private ClosedInitialPotionInventory(string?[] ids, Slot[] byCost)
    {
        _initialIds = ids;
        _byCost = byCost;
    }

    internal static ClosedInitialPotionInventory? Capture(
        CombatPredictionSimulator simulator, Player player, bool renewableRock)
    {
        var combat = (SimulatedCombatState)simulator.State.CombatState;
        // Inventory-entry, alias and callback audit is pinned to this source
        // closure. Do not admit a gameplay subscriber merely certified non-healing.
        if (simulator.State.GetPlayerCombatState(player).Phase != MegaCrit.Sts2.Core.Combat.PlayerTurnPhase.Play
            || StrategicHpRecoveryBound.ComponentRootSourceRejection(simulator, player,
                requireClosedPotionInventory: true) is not null
            || combat.RootRunModSubscriberCount != 0 || combat.RootCombatModSubscriberCount != 0
            || StrategicHpRecoveryBound.ComponentHealingUpperBound(simulator, player, 0) == int.MaxValue)
            return null;
        int count = ((ICombatPredictionPlayerLimits)combat).GetPotionSlotCount(player);
        if (count > 64) return null;
        string?[] ids = new string?[count];
        List<Slot> costs = [];
        for (int index = 0; index < count; index++)
        {
            if (combat.GetPotionAtSlot(player, index) is not { } potion) continue;
            if (combat.IsFreeEntropicPotionAtSlot(player, index)) return null;
            ids[index] = potion.Id.Entry;
            costs.Add(new(index, potion.Id.Entry, PotionUsePolicy.StrategicHpCost(potion, renewableRock)));
        }
        return new(ids, costs.OrderBy(slot => slot.Cost).ThenBy(slot => slot.Index).ToArray());
    }

    // A finite component allowance witnesses the branch's reviewed source types;
    // its numerical HP value is irrelevant to the independent inventory proof.
    // Every slot is considered, even if protected, unsearchable or unusable now:
    // including a cheaper impossible use can only loosen this lower bound.
    internal int? MinimumExplicitCost(CombatPredictionSimulator simulator, Player player,
        int requiredUses, int componentHealingAllowance)
    {
        if (componentHealingAllowance == int.MaxValue || requiredUses <= 0 || simulator.HasPendingChoice)
            return null;
        var combat = (SimulatedCombatState)simulator.State.CombatState;
        if (((ICombatPredictionPlayerLimits)combat).GetPotionSlotCount(player) != _initialIds.Length)
            return null;
        ulong consumed = 0;
        int explicitUses = 0;
        int spent = 0;
        foreach (PredictedPotionUse use in combat.PotionUses)
        {
            if ((uint)use.Slot >= (uint)_initialIds.Length
                || _initialIds[use.Slot] != use.PotionId || (consumed & (1UL << use.Slot)) != 0
                || use.StrategicHpCost < 0)
                return null;
            consumed |= 1UL << use.Slot;
            if (use.Automatic) continue;
            explicitUses++;
            spent = checked(spent + use.StrategicHpCost);
        }
        for (int slot = 0; slot < _initialIds.Length; slot++)
        {
            var potion = combat.GetPotionAtSlot(player, slot);
            if (potion is not null && (potion.Id.Entry != _initialIds[slot]
                || (consumed & (1UL << slot)) != 0 || combat.IsFreeEntropicPotionAtSlot(player, slot)))
                return null;
        }
        int needed = Math.Max(0, requiredUses - explicitUses);
        foreach (Slot slot in _byCost)
        {
            if (needed == 0) break;
            if (combat.GetPotionAtSlot(player, slot.Index) is null) continue;
            spent = checked(spent + slot.Cost);
            needed--;
        }
        // This prototype does not turn insufficient stock into a no-route prune.
        return needed == 0 ? spent : null;
    }
}
