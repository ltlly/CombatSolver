using System.Runtime.CompilerServices;
using System.Text.Json;
using CombatSolver;
using HarmonyLib;
using CombatSolver.Engine.Common;
using MegaCrit.Sts2.Core.Combat;
using MegaCrit.Sts2.Core.Extensions;
using MegaCrit.Sts2.Core.Models.Cards;

namespace OfflineSearchHarness;

// Opportunity counts only: instrumented runs are not performance measurements.
internal static class SnapshotOpportunityProbe
{
    private const int MaximumKeysPerSolver = 100_000;
    private static readonly object Gate = new();
    private static readonly ConditionalWeakTable<CombatBeamSolver, Counts> Solvers = new();
    private static readonly List<Counts> Results = [];
    private static readonly ConditionalWeakTable<CombatBeamSolver, Family> Families = new();
    private static readonly List<Family> FamilyResults = [];
    private static readonly ConditionalWeakTable<CombatBeamSolver, SolverIdentity> Identities = new();
    private static int _nextSolver;
    private sealed record SolverIdentity(int Id);
    private sealed class Family
    {
        public long Total, SameWorkerRepeats, CrossWorkerRepeats, LimitBypasses, FeatureMismatches;
        public int Workers;
        public readonly Dictionary<(StateFingerprint, int, SearchBoundaryReason),
            (int Solver, FeatureSignature Features)> Inputs = [];
        public readonly List<object> Mismatches = [];
    }
    // Selected detached values are a counterexample check, not a complete snapshot oracle.
    private readonly record struct FeatureSignature(
        long ScoreBits, int ProjectedHp, StateFingerprint ShuffleKey, int ShuffleValue,
        int PersistentBuff, int LatentSetup, int RetainedAttack, int ReachableHand,
        int FutureHeal, int GrowthCredit, int RelicCredit, int PotionCost, int HistoryCount,
        int Shuffles, bool Risk)
    {
        public static FeatureSignature Capture(SimulationSnapshot s) => new(
            BitConverter.DoubleToInt64Bits(s.Score), s.ProjectedPlayerHp,
            s.ProjectedShuffleOrderKey, s.ProjectedShuffleOrderValue,
            s.PersistentBuffValue, s.LatentSetupValue, s.RetainedAttackValue,
            s.ReachableHandValue, s.FutureHealPotential, s.GrowthHpCredit,
            s.RelicCounters.HpCredit, s.PotionStrategicCost, s.HistoryEntryCount,
            s.ShufflesCrossed, s.HasRisk);
    }
    private sealed class Counts
    {
        public int Total;
        public int DuplicateStates;
        public int DuplicateEvaluationInputs;
        public int StateLimitBypasses;
        public int EvaluationLimitBypasses;
        public Dictionary<string, int> Boundaries = [];
        public HashSet<StateFingerprint> States = [];
        public HashSet<(StateFingerprint, int, SearchBoundaryReason)> Evaluations = [];
    }

    internal static void Install(string output)
    {
        string? mode = Environment.GetEnvironmentVariable("OFFLINE_HARNESS_SNAPSHOT_PROBE");
        if (mode == "worker-families")
        {
            GameBootstrap.Harmony.Patch(AccessTools.Method(typeof(CombatBeamSolver), "CreateExpansionWorker"),
                postfix: new HarmonyMethod(typeof(SnapshotOpportunityProbe), nameof(ObserveWorker)));
            GameBootstrap.Harmony.Patch(AccessTools.Method(typeof(CombatBeamSolver), "Snapshot"),
                postfix: new HarmonyMethod(typeof(SnapshotOpportunityProbe), nameof(ObserveFamily)));
            AppDomain.CurrentDomain.ProcessExit += (_, _) =>
            {
                lock (Gate) File.WriteAllText(Path.Combine(output, "snapshot-worker-families.json"),
                    JsonSerializer.Serialize(new
                    {
                        observationOnly = true, maximumKeysPerFamily = MaximumKeysPerSolver,
                        scope = "Worker families follow actual CreateExpansionWorker ownership. Keys and selected feature equality are not a cache-safety proof. No snapshot/model graphs retained.",
                        families = FamilyResults.Select(f => new
                        {
                            f.Total, f.Workers, retainedInputs = f.Inputs.Count, f.SameWorkerRepeats,
                            f.CrossWorkerRepeats, f.LimitBypasses, f.FeatureMismatches, f.Mismatches,
                        }),
                    }, new JsonSerializerOptions { WriteIndented = true }));
            };
            return;
        }
        if (mode != "1") return;
        GameBootstrap.Harmony.Patch(AccessTools.Method(typeof(CombatBeamSolver), "Snapshot"),
            postfix: new HarmonyMethod(typeof(SnapshotOpportunityProbe), nameof(Observe)));
        AppDomain.CurrentDomain.ProcessExit += (_, _) =>
        {
            lock (Gate) File.WriteAllText(Path.Combine(output, "snapshot-opportunities.json"),
                JsonSerializer.Serialize(Results.Select(c => new
                {
                    c.Total, c.DuplicateStates, c.DuplicateEvaluationInputs, c.Boundaries,
                    c.StateLimitBypasses, c.EvaluationLimitBypasses, maximumKeysPerSolver = MaximumKeysPerSolver,
                }), new JsonSerializerOptions { WriteIndented = true }));
        };
    }

    private static Family FamilyFor(CombatBeamSolver solver) => Families.GetValue(solver, _ =>
    {
        Family family = new();
        FamilyResults.Add(family);
        return family;
    });

    private static void ObserveWorker(CombatBeamSolver __instance, CombatBeamSolver __result)
    {
        lock (Gate)
        {
            Family family = FamilyFor(__instance);
            Families.Add(__result, family);
            family.Workers++;
        }
    }

    private static void ObserveFamily(CombatBeamSolver __instance, int actionCount, SimulationSnapshot __result)
    {
        lock (Gate)
        {
            Family family = FamilyFor(__instance);
            int solver = Identities.GetValue(__instance, _ => new(++_nextSolver)).Id;
            family.Total++;
            var key = (__result.StateKey, actionCount, __result.BoundaryReason);
            FeatureSignature features = FeatureSignature.Capture(__result);
            if (family.Inputs.TryGetValue(key, out var prior))
            {
                if (prior.Solver == solver) family.SameWorkerRepeats++;
                else family.CrossWorkerRepeats++;
                if (prior.Features != features)
                {
                    family.FeatureMismatches++;
                    if (family.Mismatches.Count < 12)
                        family.Mismatches.Add(new { key.Item2, key.Item3, first = prior.Features, current = features });
                }
            }
            else if (family.Inputs.Count < MaximumKeysPerSolver)
                family.Inputs.Add(key, (solver, features));
            else family.LimitBypasses++;
        }
    }

    private static void Observe(CombatBeamSolver __instance, int actionCount, SimulationSnapshot __result)
    {
        lock (Gate)
        {
            Counts c = Solvers.GetValue(__instance, _ => { Counts result = new(); Results.Add(result); return result; });
            c.Total++;
            string boundary = __result.BoundaryReason.ToString();
            c.Boundaries[boundary] = c.Boundaries.GetValueOrDefault(boundary) + 1;
            if (c.States.Contains(__result.StateKey)) c.DuplicateStates++;
            else if (c.States.Count < MaximumKeysPerSolver) c.States.Add(__result.StateKey);
            else c.StateLimitBypasses++;
            var evaluation = (__result.StateKey, actionCount, __result.BoundaryReason);
            if (c.Evaluations.Contains(evaluation)) c.DuplicateEvaluationInputs++;
            else if (c.Evaluations.Count < MaximumKeysPerSolver) c.Evaluations.Add(evaluation);
            else c.EvaluationLimitBypasses++;
        }
    }

    internal static void RunShuffleWitness(CombatState combat, string output)
    {
        if (Environment.GetEnvironmentVariable("OFFLINE_HARNESS_SHUFFLE_WITNESS") != "1") return;
        string liveBefore = ContinuationStamp.CaptureLive(combat).StateText;
        var player = combat.Players.Single();
        var simulator = CombatRootSnapshot.Capture(combat).ForkSimulator();
        var first = PredictedCard.Create(CanonicalModels.Card<StrikeIronclad>(), player);
        var second = PredictedCard.Create(CanonicalModels.Card<StrikeIronclad>(), player);
        // Same native sorting identity, different persistent combat state.
        first.MutablePreview.BaseReplayCount = 1;
        second.MutablePreview.BaseReplayCount = 2;
        List<PredictedCard> forward = [first, second];
        List<PredictedCard> reversed = [second, first];
        var rng = simulator.Rng.ShuffleState;
        forward.StableShuffle(rng.ToRng());
        reversed.StableShuffle(rng.ToRng());
        string[] a = forward.Select(CardChoiceSupport.ChoiceCardKey).ToArray();
        string[] b = reversed.Select(CardChoiceSupport.ChoiceCardKey).ToArray();
        bool sameMultiset = a.Order(StringComparer.Ordinal).SequenceEqual(b.Order(StringComparer.Ordinal));
        bool sameSequence = a.SequenceEqual(b);
        if (first.CompareTo(second) != 0 || !sameMultiset || sameSequence)
            throw new InvalidOperationException("Expected equal-sort-key shuffle counterexample was not reproduced.");
        if (liveBefore != ContinuationStamp.CaptureLive(combat).StateText)
            throw new InvalidOperationException("Shuffle witness changed the live root.");
        File.WriteAllText(Path.Combine(output, "shuffle-order-witness.json"), JsonSerializer.Serialize(new
        {
            nativeCompare = first.CompareTo(second), sameMultiset, sameSequence,
            forward = a, reversed = b, rng.Counter, liveUnchanged = true,
        }, new JsonSerializerOptions { WriteIndented = true }));
    }
}
