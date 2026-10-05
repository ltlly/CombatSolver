#requires -Version 7.4
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'headless-runtime.ps1')

function Assert-ProfileFixture([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "Headless profile assertion failed: $Message" }
}
function New-ProfileFixture([string]$Name) {
    $source = Join-Path $testRoot "$Name/source"
    $destination = Join-Path $testRoot "$Name/private"
    New-Item -ItemType Directory -Path $source, $destination -Force | Out-Null
    return @($source, $destination)
}

$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('combatsolver-profile-selection-' + [Guid]::NewGuid().ToString('N'))
try {
    $source, $destination = New-ProfileFixture 'steam'
    $account = Join-Path $source 'steam/123'
    New-Item -ItemType Directory -Path "$account/modded/profile1/saves", "$source/ModConfig", "$source/mods/config" -Force | Out-Null
    Set-Content -LiteralPath "$account/settings.save" -Value '{"fixture":"steam"}'
    Set-Content -LiteralPath "$account/modded/profile1/saves/progress.save" -Value 'progress'
    Set-Content -LiteralPath "$source/ModConfig/mod.json" -Value 'config'
    Set-Content -LiteralPath "$source/mods/config/mod.json" -Value 'nested-config'
    Initialize-HeadlessProfile $source $destination
    Assert-ProfileFixture ((Get-Content "$destination/default/1/settings.save" -Raw).Trim() -eq '{"fixture":"steam"}') 'Steam account was not normalized'
    Assert-ProfileFixture (Test-Path "$destination/default/1/modded/profile1/saves/progress.save") 'progress was not preserved'
    Assert-ProfileFixture (Test-Path "$destination/ModConfig/mod.json") 'ModConfig was not preserved'
    Assert-ProfileFixture (Test-Path "$destination/mods/config/mod.json") 'nested config was not preserved'
    Set-Content "$destination/default/1/settings.save" -Value 'private'
    Initialize-HeadlessProfile $source $destination
    Assert-ProfileFixture ((Get-Content "$destination/default/1/settings.save" -Raw).Trim() -eq 'private') 'reuse overwrote private settings'
    Assert-ProfileFixture ((Get-Content "$account/settings.save" -Raw).Trim() -eq '{"fixture":"steam"}') 'source settings changed'

    $source, $destination = New-ProfileFixture 'default'
    New-Item -ItemType Directory -Path "$source/default/1", "$source/steam/123" -Force | Out-Null
    Set-Content "$source/default/1/settings.save" -Value 'default'
    Set-Content "$source/steam/123/settings.save" -Value 'steam'
    Initialize-HeadlessProfile $source $destination
    Assert-ProfileFixture ((Get-Content "$destination/default/1/settings.save" -Raw).Trim() -eq 'default') 'default profile lost priority'

    foreach ($case in @('missing', 'ambiguous')) {
        $source, $destination = New-ProfileFixture $case
        if ($case -eq 'ambiguous') {
            foreach ($accountId in @('123', '456')) {
                New-Item -ItemType Directory -Path "$source/steam/$accountId" -Force | Out-Null
                Set-Content "$source/steam/$accountId/settings.save" -Value 'settings'
            }
        }
        $rejected = $false
        try { Initialize-HeadlessProfile $source $destination } catch {
            $expected = if ($case -eq 'missing') { 'No interactive settings.save*' } else { 'Multiple Steam profiles*' }
            if ($_.Exception.Message -notlike $expected) { throw }
            $rejected = $true
        }
        Assert-ProfileFixture $rejected "$case source was accepted"
        Assert-ProfileFixture (-not (Test-Path "$destination/default")) "$case source left a partial default profile"
    }
    Write-Output 'HEADLESS_PROFILE_SELECTION_PASS steam/default/reuse/source-preservation/config/missing/ambiguous'
} finally {
    $resolvedRoot = [IO.Path]::GetFullPath($testRoot)
    $tempParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $resolvedRoot.StartsWith($tempParent, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to clean a profile fixture outside temp: $resolvedRoot"
    }
    if (Test-Path -LiteralPath $resolvedRoot) { Remove-Item -LiteralPath $resolvedRoot -Recurse -Force }
}
