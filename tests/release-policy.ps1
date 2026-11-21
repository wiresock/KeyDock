$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot '../installer/release-policy.ps1')

# Exercise the real Git and manifest comparison without creating remote tags.
$script:tagCommit = $null
function Get-ReleaseTagCommit([string] $Tag) { $script:tagCommit }

function Assert-Failure([scriptblock] $Action, [string] $Expected) {
    try {
        & $Action
    } catch {
        if ($_.Exception.Message -like "*$Expected*") { return }
        throw
    }
    throw "Expected failure: $Expected"
}

$fixture = Join-Path ([IO.Path]::GetTempPath()) ('keydock-version-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path (Join-Path $fixture 'docs/releases') | Out-Null
Push-Location $fixture
try {
    git init --quiet
    git config user.name 'KeyDock Version Test'
    git config user.email 'keydock-version-test@example.invalid'

    function Write-Revision([string] $Version, [switch] $Notes) {
        "[package]`nname = `"keydock`"`nversion = `"$Version`"`n`n[dependencies]`n" | Set-Content -LiteralPath Cargo.toml
        if ($Notes) { 'Test release notes' | Set-Content -LiteralPath "docs/releases/$Version.md" }
        git add --all
        git -c commit.gpgsign=false commit --allow-empty --quiet -m "Fixture $Version"
        git rev-parse HEAD
    }

    $prior = Write-Revision '0.3.12'
    $ordinary = Write-Revision '0.3.12'
    $plan = Get-KeyDockReleasePlan $prior $ordinary
    if ($plan.Changed -or $plan.Version -ne '0.3.12') { throw 'Ordinary changes should leave bootstrap unpublished' }

    $bump = Write-Revision '0.3.13' -Notes
    $plan = Get-KeyDockReleasePlan $ordinary $bump
    if (!$plan.Changed -or $plan.Version -ne '0.3.13') { throw 'Expected a release plan for the bump' }

    $script:tagCommit = $bump
    if (!(Get-KeyDockReleasePlan $ordinary $bump).Changed) { throw 'A correct existing tag must allow a retry' }
    $script:tagCommit = $prior
    Assert-Failure { Get-KeyDockReleasePlan $ordinary $bump } 'already points to another commit'
    $script:tagCommit = $null

    $lower = Write-Revision '0.3.11'
    Assert-Failure { Get-KeyDockReleasePlan $bump $lower } 'Version decreased'
    Assert-Failure { Get-KeyDockReleasePlan $lower $ordinary } 'owner-approved bootstrap'
    $bad = Write-Revision '0.4.0-beta'
    Assert-Failure { Get-KeyDockReleasePlan $ordinary $bad } 'X.Y.Z format'
    $missing = Write-Revision '0.3.14'
    Assert-Failure { Get-KeyDockReleasePlan $bump $missing } 'Missing docs/releases/0.3.14.md'
    'Version detection checks passed.'
} finally {
    Pop-Location
}
