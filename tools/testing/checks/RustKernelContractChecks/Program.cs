using System.Text;
using CombatSolver;

static void Assert(bool condition, string message)
{
    if (!condition)
        throw new InvalidOperationException(message);
}

static void CheckEligibilityContracts()
{
    Assert(
        RustKernelWireValidator.TryParseEligibilityResponseBytes(
            Encoding.UTF8.GetBytes("""{"abiVersion":1,"eligible":true}"""),
            out bool okEligible,
            out string? okCode,
            out _,
            out string? okErr)
        && okEligible
        && okCode is null
        && okErr is null,
        "Valid eligible response must succeed.");

    Assert(
        RustKernelWireValidator.TryParseEligibilityResponseBytes(
            Encoding.UTF8.GetBytes("""{"abiVersion":1,"eligible":false,"failureCode":"modifiers_present","detail":"bad mod"}"""),
            out bool inelig,
            out string? failCode,
            out string? detail,
            out string? ineligErr)
        && !inelig
        && failCode == "modifiers_present"
        && detail == "bad mod"
        && ineligErr is null,
        "Valid ineligible response must preserve failure code.");

    foreach (string badJson in new[]
    {
        "",
        "{not json",
        """{"abiVersion":1,"eligible":true""",
        """{"abiVersion":1}""",
        """{"abiVersion":2,"eligible":true}""",
        """{"abiVersion":1,"eligible":false}""",
    })
    {
        Assert(
            !RustKernelWireValidator.TryParseEligibilityResponseBytes(
                Encoding.UTF8.GetBytes(badJson),
                out _,
                out _,
                out _,
                out string? err)
            && !string.IsNullOrEmpty(err),
            $"Invalid eligibility payload should fail closed: {badJson}");
    }

    byte[] oversized = new byte[RustKernelAbi.MaxControlResponseBytes + 1];
    Array.Fill(oversized, (byte)' ');
    Assert(
        !RustKernelWireValidator.TryParseEligibilityResponseBytes(
            oversized,
            out _,
            out _,
            out _,
            out string? sizeErr)
        && sizeErr!.Contains("length_out_of_range", StringComparison.Ordinal),
        "Oversized eligibility response must be rejected.");
}

static void CheckEndTurnRiskContracts()
{
    Assert(
        RustKernelWireValidator.TryParseEndTurnRiskResponseBytes(
            Encoding.UTF8.GetBytes("""{"abiVersion":1,"outcome":"Succeeded","hpBefore":40,"hpAfter":32,"hpLost":8,"playerDead":false,"monsterMoves":"JAW_WORM:CHOMP"}"""),
            expectedHpBefore: 40,
            out int hpBefore,
            out int hpAfter,
            out int hpLost,
            out bool playerDead,
            out string moves,
            out string? okErr,
            out bool okViolation)
        && hpBefore == 40
        && hpAfter == 32
        && hpLost == 8
        && !playerDead
        && moves == "JAW_WORM:CHOMP"
        && okErr is null
        && !okViolation,
        "Valid Succeeded end-turn risk response must succeed.");

    Assert(
        !RustKernelWireValidator.TryParseEndTurnRiskResponseBytes(
            Encoding.UTF8.GetBytes("""{"abiVersion":1,"outcome":"CapabilityMiss","failureCode":"model_unmapped","error":"unsupported"}"""),
            expectedHpBefore: 40,
            out _,
            out _,
            out _,
            out _,
            out _,
            out string? capReason,
            out bool capViolation)
        && !capViolation
        && capReason!.StartsWith("CapabilityMiss:model_unmapped:", StringComparison.Ordinal),
        "Clean CapabilityMiss response must request fallback without marking runtime corrupt.");

    foreach (string badPayload in new[]
    {
        "",
        "{truncated",
        """{"abiVersion":1,"outcome":"Succeeded""",
        """{"abiVersion":1,"outcome":"Succeeded"}""",
        """{"abiVersion":1,"outcome":"Succeeded","hpBefore":40,"hpAfter":32,"hpLost":8,"playerDead":false}""",
        """{"abiVersion":1,"outcome":"Succeeded","hpBefore":35,"hpAfter":32,"hpLost":3,"playerDead":false,"monsterMoves":""}""",
        """{"abiVersion":1,"outcome":"Succeeded","hpBefore":40,"hpAfter":40,"hpLost":-1,"playerDead":false,"monsterMoves":""}""",
        """{"abiVersion":1,"outcome":"Succeeded","hpBefore":40,"hpAfter":0,"hpLost":40,"playerDead":false,"monsterMoves":""}""",
        """{"abiVersion":2,"outcome":"Succeeded","hpBefore":40,"hpAfter":40,"hpLost":0,"playerDead":false,"monsterMoves":""}""",
        """{"abiVersion":1,"outcome":"BogusOutcome"}""",
    })
    {
        Assert(
            !RustKernelWireValidator.TryParseEndTurnRiskResponseBytes(
                Encoding.UTF8.GetBytes(badPayload),
                expectedHpBefore: 40,
                out _,
                out _,
                out _,
                out _,
                out _,
                out _,
                out bool violation)
            && violation,
            $"Broken/incomplete end-turn risk payload must fail closed as contract violation: {badPayload}");
    }

    byte[] oversized = new byte[RustKernelAbi.MaxControlResponseBytes + 1];
    Array.Fill(oversized, (byte)' ');
    Assert(
        !RustKernelWireValidator.TryParseEndTurnRiskResponseBytes(
            oversized,
            expectedHpBefore: 40,
            out _,
            out _,
            out _,
            out _,
            out _,
            out _,
            out bool oversizedViolation)
        && oversizedViolation,
        "Oversized end-turn risk payload must fail as contract violation.");
}

CheckEligibilityContracts();
CheckEndTurnRiskContracts();
Console.WriteLine("RUST_KERNEL_CONTRACT_CHECKS_OK cases=19");
