using System.Reflection;
using HarmonyLib;
using CombatSolver.Engine.Common;
using CombatSolver.Engine.InCombat.Simulation;
using MegaCrit.Sts2.Core.Combat;
using MegaCrit.Sts2.Core.Entities.Cards;
using MegaCrit.Sts2.Core.Entities.Players;
using MegaCrit.Sts2.Core.Models;
using MegaCrit.Sts2.Core.Models.Afflictions;
using MegaCrit.Sts2.Core.Models.Cards;
using MegaCrit.Sts2.Core.Models.Enchantments;
using MegaCrit.Sts2.Core.Models.Powers;

namespace CombatSolver;

internal sealed partial class UnattendedTestRunner
{
    private void AssertProjectedHookReceiverConstruction(CombatState live, Player player)
    {
        string liveBefore = ContinuationStamp.CaptureLive(live).StateText;
        CombatRootSnapshot root = CombatRootSnapshot.Capture(live);
        using IDisposable isolation = SimulationNotificationIsolation.Enter();
        CombatPredictionSimulator parent = root.ForkSimulator();
        for (int index = 0; index < 300; index++)
            parent.AddToPile(PredictedCard.Create(ModelDb.Card<StrikeIronclad>(), player), PileType.Draw);
        parent.AddToPile(PredictedCard.Create(ModelDb.Card<Reflex>(), player), PileType.Draw);
        int pileCacheChecks = AssertPileHookProjectionCache(parent, root, player);
        string parentBefore = DescribeContinuationContractState(parent, root, player);
        CombatPredictionSimulator candidate = parent.Fork();
        CombatPredictionSimulator whole = parent.Fork();
        DisableMirroredFilterForOracle(whole);
        int comparisons = 0;
        int projectedBuilds = 0;
        var filter = new MirroredHookListenerFilter(enabled: true);

        void Compare(CombatPredictionSimulator simulator)
        {
            var source = (ICombatPredictionHookListenerSource)simulator.State.CombatState;
            // Request the projection before constructing the complete oracle snapshot.
            IReadOnlyList<AbstractModel> combat = source.MirroredHookListeners;
            IReadOnlyList<AbstractModel> run = source.MirroredRunHookListeners;
            IReadOnlyList<AbstractModel> complete = source.HookListeners;
            filter.VerifyProjectedReceivers(complete, combat);
            filter.VerifyProjectedReceivers(source.RunHookListeners, run);
            if (combat.Count < complete.Count / 2)
                projectedBuilds++;
            foreach (MirroredHookMask mask in Enum.GetValues<MirroredHookMask>())
            {
                AbstractModel[] Select(IReadOnlyList<AbstractModel> listeners)
                {
                    if (listeners is not MirroredHookListenerSnapshot snapshot)
                        return [];
                    return Enumerable.Range(0, snapshot.Count)
                        .Where(index => (snapshot.Layout.Entries[index].Mask & mask) != 0)
                        .Select(index => snapshot[index]).ToArray();
                }
                if (!Select(filter.Filter(complete)).SequenceEqual(Select(combat), ReferenceEqualityComparer.Instance)
                    || !Select(filter.Filter(source.RunHookListeners)).SequenceEqual(Select(run), ReferenceEqualityComparer.Instance))
                    throw new InvalidOperationException($"Projected dispatch differs for {mask}.");
            }
            comparisons++;
        }
        void ComparePair()
        {
            Compare(candidate);
            if (DescribeContinuationContractState(candidate, root, player)
                != DescribeContinuationContractState(whole, root, player))
                throw new InvalidOperationException("Projected listeners changed full state, fingerprint, history or RNG.");
        }
        void Mutate(Action<CombatPredictionSimulator, PredictedCard> action)
        {
            foreach (CombatPredictionSimulator simulator in new[] { candidate, whole })
                action(simulator, simulator.State.GetPlayerCombatState(player).DrawPile.TopCard!);
            ComparePair();
        }
        ComparePair();
        foreach (CombatPredictionSimulator simulator in new[] { candidate, whole })
        {
            PredictedCard attack = simulator.State.GetPlayerCombatState(player).Hand.Cards
                .First(card => card.Preview is StrikeIronclad);
            SimulatedCombatState combat = (SimulatedCombatState)simulator.State.CombatState;
            combat.BeginActionChoices((IReadOnlyList<PlanCardChoice>?)null);
            try
            {
                using (combat.BeginCardExecutionScope(new ForkableSet<uint>()))
                    if (!simulator.ManualPlay(attack, root.Enemies[0], out _))
                        throw new InvalidOperationException("Projected listener fixture could not play its attack.");
                if (!CombatBeamSolver.SettleReplayActionBoundary(simulator, combat))
                    throw new InvalidOperationException("Projected listener fixture unexpectedly opened a choice.");
            }
            finally { combat.EndActionChoices(); }
        }
        ComparePair();
        Mutate((_, card) => card.MutablePreview.BaseReplayCount++);
        Mutate((_, card) => card.Afflict(ModelDb.Affliction<Hexed>().ToMutable(), 1));
        Mutate((_, card) => card.Enchant(ModelDb.Enchantment<Swift>().ToMutable(), 1));
        Mutate((_, card) => card.ClearAffliction());
        Mutate((simulator, card) => simulator.AddToPile(card, PileType.Discard, CardPilePosition.Random));
        Mutate((_, card) => card.MutablePreview.HasBeenRemovedFromState = true);
        foreach (CombatPredictionSimulator simulator in new[] { candidate, whole })
            ((SimulatedCombatState)simulator.State.CombatState).SetAmount<StrengthPower>(player.Creature, 3);
        ComparePair();
        CombatPredictionSimulator[] siblings = Enumerable.Range(0, 16).Select(_ => candidate.Fork()).ToArray();
        Parallel.ForEach(siblings, simulator =>
        {
            using IDisposable workerIsolation = SimulationNotificationIsolation.Enter();
            SimulatedCombatState combat = (SimulatedCombatState)simulator.State.CombatState;
            combat.SetAmount<StrengthPower>(player.Creature, 4);
            PredictedCard card = simulator.State.GetPlayerCombatState(player).DrawPile.TopCard!;
            card.MutablePreview.BaseReplayCount++;
            var source = (ICombatPredictionHookListenerSource)combat;
            IReadOnlyList<AbstractModel> projected = source.MirroredHookListeners;
            filter.VerifyProjectedReceivers(source.HookListeners, projected);
        });
        if (projectedBuilds == 0 && !FastLaneVerification.Enabled)
            throw new InvalidOperationException("Large listener fixture did not exercise projected construction.");
        if (DescribeContinuationContractState(parent, root, player) != parentBefore
            || DescribeContinuationContractState(candidate, root, player)
                != DescribeContinuationContractState(whole, root, player)
            || ContinuationStamp.CaptureLive(live).StateText != liveBefore)
            throw new InvalidOperationException("Projected listeners escaped sibling, parent or live isolation.");
        AssertProjectedGetterPatchFallback(live, player);
        _completedChecks.Add($"ProjectedHookReceivers:Comparisons={comparisons}:ProjectedBuilds={projectedBuilds}:PileCacheChecks={pileCacheChecks}:Masks=64:FullStateFingerprintHistoryRng:MovesRemovedAttachmentsPowerOrder:Fork16ParentLive:GetterPatchFallback");
    }

    private static int AssertPileHookProjectionCache(
        CombatPredictionSimulator parent, CombatRootSnapshot root, Player player)
    {
        int checks = 0;
        var filter = new MirroredHookListenerFilter(enabled: true);
        FieldInfo cache = typeof(SimCardPile).GetField("_hookCardProjection",
            BindingFlags.Instance | BindingFlags.NonPublic)!;
        string parentBefore = DescribeContinuationContractState(parent, root, player);

        void Check(SimCardPile pile)
        {
            if (!pile.TryGetHookCardProjection(filter, out ReadOnlySpan<int> selected))
                throw new InvalidOperationException("Native pile unexpectedly refused receiver projection.");
            int[] expected = Enumerable.Range(0, pile.Cards.Count).Where(index =>
            {
                CardModel card = pile.Cards[index].Preview;
                return filter.HasMirroredCallbacks(card)
                    || card.Affliction is { } affliction && filter.HasMirroredCallbacks(affliction)
                    || card.Enchantment is { } enchantment && filter.HasMirroredCallbacks(enchantment);
            }).ToArray();
            if (!selected.SequenceEqual(expected))
                throw new InvalidOperationException("Pile projection changed exact potential receiver indices.");
            checks++;
        }

        SimCardPile parentDraw = parent.State.GetPlayerCombatState(player).DrawPile;
        Check(parentDraw);
        object shared = cache.GetValue(parentDraw)!;
        Check(parentDraw);
        if (!ReferenceEquals(shared, cache.GetValue(parentDraw)))
            throw new InvalidOperationException("Unchanged pile did not reuse its immutable projection.");
        var child = parent.Fork();
        SimCardPile draw = child.State.GetPlayerCombatState(player).DrawPile;
        if (!ReferenceEquals(shared, cache.GetValue(draw)))
            throw new InvalidOperationException("Fork did not share immutable projection indices.");
        Check(draw);
        if (draw.Cards.Zip(parentDraw.Cards).Any(pair => ReferenceEquals(pair.First, pair.Second)))
            throw new InvalidOperationException("Projection Fork shared mutable card wrappers.");
        PredictedCard changed = draw.TopCard!;
        changed.MutablePreview.BaseReplayCount++;
        if (cache.GetValue(draw) is not null)
            throw new InvalidOperationException("MutablePreview did not invalidate the pile projection.");
        Check(draw);
        changed.Afflict(ModelDb.Affliction<Hexed>().ToMutable(), 1);
        Check(draw);
        changed.Enchant(ModelDb.Enchantment<Swift>().ToMutable(), 1);
        Check(draw);
        changed.ClearAffliction();
        Check(draw);
        // Model a nested listener query between writable access and the completed
        // structural write. The completion notification must invalidate that refill.
        CardModel writable = changed.MutablePreview;
        Check(draw);
        writable.Affliction = ModelDb.Affliction<Hexed>().ToMutable();
        writable.Affliction.Card = writable;
        writable.Affliction._amount = 1;
        changed.NotifyHookListenerStructureChanged();
        if (cache.GetValue(draw) is not null)
            throw new InvalidOperationException("Completed structural write retained a reentrant projection.");
        Check(draw);
        PredictedCard inserted = PredictedCard.Create(ModelDb.Card<Reflex>(), player);
        child.AddToPile(inserted, PileType.Draw, CardPilePosition.Top);
        Check(draw);
        child.AddToPile(inserted, PileType.Discard);
        Check(draw);
        Check(child.State.GetPlayerCombatState(player).DiscardPile);
        PredictedCard callbackCard = draw.Cards.First(card => card.Preview is Reflex);
        callbackCard.MutablePreview.HasBeenRemovedFromState = true;
        Check(draw);
        callbackCard.MutablePreview.HasBeenRemovedFromState = false;
        Check(draw);
        draw.Remove(callbackCard);
        Check(draw);
        draw.Add(callbackCard);
        Check(draw);
        draw.Insert(0, PredictedCard.Create(ModelDb.Card<Reflex>(), player));
        Check(draw);
        CombatPredictionSimulator grandchild = child.Fork();
        SimCardPile grandchildDraw = grandchild.State.GetPlayerCombatState(player).DrawPile;
        grandchildDraw.Clear();
        Check(grandchildDraw);
        Check(draw);
        SimCardPile opaque = child.Fork().State.GetPlayerCombatState(player).DrawPile;
        opaque.DisableFingerprintCache();
        if (opaque.TryGetHookCardProjection(filter, out _)
            || draw.TryGetHookCardProjection(new MirroredHookListenerFilter(enabled: false), out _))
            throw new InvalidOperationException("Opaque or disabled projection did not preserve full scan.");
        var alternative = new MirroredHookListenerFilter(enabled: true);
        object beforeAlternative = cache.GetValue(draw)!;
        if (!draw.TryGetHookCardProjection(alternative, out _)
            || ReferenceEquals(beforeAlternative, cache.GetValue(draw)))
            throw new InvalidOperationException("A different filter reused another capture's cache.");
        PredictedCard alias = PredictedCard.Create(ModelDb.Card<Reflex>(), player);
        SimCardPile oldOwner = new(PileType.Draw, new[] { alias });
        Check(oldOwner);
        SimCardPile newOwner = new(PileType.Hand, new[] { alias });
        if (cache.GetValue(oldOwner) is not null
            || oldOwner.TryGetHookCardProjection(filter, out _))
            throw new InvalidOperationException("Cross-pile wrapper alias retained a reusable projection.");
        Check(newOwner);
        alias.MutablePreview.BaseReplayCount++;
        if (cache.GetValue(newOwner) is not null)
            throw new InvalidOperationException("Aliased wrapper write did not invalidate its current owner.");
        CombatPredictionSimulator[] siblings = Enumerable.Range(0, 16).Select(_ => parent.Fork()).ToArray();
        Parallel.ForEach(siblings, simulator =>
        {
            using IDisposable workerIsolation = SimulationNotificationIsolation.Enter();
            SimCardPile pile = simulator.State.GetPlayerCombatState(player).DrawPile;
            if (!pile.TryGetHookCardProjection(filter, out _)
                || !ReferenceEquals(shared, cache.GetValue(pile)))
                throw new InvalidOperationException("Parallel Fork lost shared immutable projection.");
            pile.TopCard!.MutablePreview.BaseReplayCount++;
            if (!pile.TryGetHookCardProjection(filter, out _)
                || ReferenceEquals(shared, cache.GetValue(pile)))
                throw new InvalidOperationException("Parallel mutation reused stale parent indices.");
        });
        if (!ReferenceEquals(shared, cache.GetValue(parentDraw))
            || DescribeContinuationContractState(parent, root, player) != parentBefore)
            throw new InvalidOperationException("Projection cache escaped parent/Fork state isolation.");
        return checks;
    }

    private static void DisableMirroredFilterForOracle(CombatPredictionSimulator simulator)
    {
        // Clone immutable root metadata rather than changing the shared capture of siblings.
        // Only the dispatch optimisation is disabled; all captured policies/models stay identical.
        FieldInfo subscribers = typeof(SimulatedCombatState).GetField("_modHookSubscribers",
            BindingFlags.Instance | BindingFlags.NonPublic)!;
        object original = subscribers.GetValue(simulator.State.CombatState)!;
        object copy = typeof(object).GetMethod("MemberwiseClone", BindingFlags.Instance | BindingFlags.NonPublic)!
            .Invoke(original, null)!;
        typeof(PredictionModHookSubscriberCapture).GetField("<MirroredHookFilter>k__BackingField",
            BindingFlags.Instance | BindingFlags.NonPublic)!.SetValue(copy, new MirroredHookListenerFilter(enabled: false));
        subscribers.SetValue(simulator.State.CombatState, copy);
    }

    private static void AssertProjectedGetterPatchFallback(CombatState live, Player player)
    {
        MethodInfo[] getters =
        [
            typeof(CardModel).GetProperty(nameof(CardModel.HasBeenRemovedFromState))!.GetMethod!,
            typeof(PredictedCard).GetProperty(nameof(PredictedCard.OwnerPile),
                BindingFlags.Instance | BindingFlags.NonPublic)!.GetMethod!,
        ];
        MethodInfo prefix = typeof(UnattendedTestRunner).GetMethod(nameof(ProjectedGetterPrefix),
            BindingFlags.Static | BindingFlags.NonPublic)!;
        Harmony harmony = new("CombatSolver.Tests.ProjectedGetterGuard");
        foreach (MethodInfo getter in getters)
        {
            try
            {
                harmony.Patch(getter, prefix: new HarmonyMethod(prefix));
                if (MirroredHookListenerFilter.Capture().CanProjectReceivers)
                    throw new InvalidOperationException("A patched card getter retained projected construction.");
                using IDisposable isolation = SimulationNotificationIsolation.Enter();
                CombatPredictionSimulator simulator = CombatRootSnapshot.Capture(live).ForkSimulator();
                for (int index = 0; index < 300; index++)
                    simulator.AddToPile(PredictedCard.Create(ModelDb.Card<StrikeIronclad>(), player), PileType.Draw);
                var combat = (SimulatedCombatState)simulator.State.CombatState;
                combat.SetAmount<StrengthPower>(player.Creature, 3);
                var source = (ICombatPredictionHookListenerSource)combat;
                IReadOnlyList<AbstractModel> mirrored = source.MirroredHookListeners;
                if (mirrored.Count != source.HookListeners.Count)
                    throw new InvalidOperationException("Getter-patched root did not use the complete producer.");
            }
            finally { harmony.Unpatch(getter, prefix); }
            if (!MirroredHookListenerFilter.Capture().CanProjectReceivers)
                throw new InvalidOperationException("Getter patch removal did not restore root projection eligibility.");
        }
    }

    private static void ProjectedGetterPrefix() { }
}
