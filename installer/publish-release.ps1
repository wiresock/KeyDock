[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidatePattern('^[a-f0-9]{40}$')] [string] $Before,
    [Parameter(Mandatory)] [ValidatePattern('^\d+\.\d+\.\d+$')] [string] $Version,
    [Parameter(Mandatory)] [string] $Directory
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'release-policy.ps1')

if ($env:GITHUB_EVENT_NAME -cne 'push' -or $env:GITHUB_REF -cne 'refs/heads/main' -or
    $env:GITHUB_REF_PROTECTED -cne 'true' -or $env:GITHUB_SHA -cnotmatch '^[a-f0-9]{40}$' -or
    $env:GITHUB_REPOSITORY -cnotmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
    throw 'Automatic publishing requires a protected main push on GitHub Actions'
}
$repo = $env:GITHUB_REPOSITORY
$commit = $env:GITHUB_SHA
if ((git rev-parse HEAD) -cne $commit) { throw 'Checkout is not the validated commit' }
$plan = Get-KeyDockReleasePlan $Before $commit
if (!$plan.Changed -or $plan.Version -cne $Version -or $Version -eq '0.3.12') {
    throw 'The validated merge must intentionally increase the version after v0.3.12'
}

$main = gh api "repos/$repo/branches/main" | ConvertFrom-Json
if (!$main.protected) { throw 'The current main branch must be protected' }
git fetch --no-tags origin refs/heads/main
git merge-base --is-ancestor $commit FETCH_HEAD
$prs = @(gh api "repos/$repo/commits/$commit/pulls" | ConvertFrom-Json)
$merge = @($prs | Where-Object {
    $_.merged_at -and $_.base.ref -ceq 'main' -and $_.base.repo.full_name -ceq $repo -and $_.merge_commit_sha -ceq $commit
})
if (!$merge.Count) { throw 'The selected commit is not a merged pull request into this main branch' }

function Find-Release([string] $TagName) {
    $filter = '.[] | select(.tag_name == ' + ($TagName | ConvertTo-Json -Compress) + ')'
    $json = gh api --paginate "repos/$repo/releases?per_page=100" --jq $filter
    if ($json) { $json | ConvertFrom-Json }
}

$bootstrap = Find-Release 'v0.3.12'
if (!$bootstrap -or $bootstrap.draft) { throw 'The owner-approved v0.3.12 bootstrap release must be published first' }

& (Join-Path $PSScriptRoot 'verify-release.ps1') -Directory $Directory -Version $Version -Architecture x64
$provenance = Get-Content -Raw -LiteralPath (Join-Path $Directory 'provenance.json') | ConvertFrom-Json
if ($provenance.source -cne $commit -or $provenance.version -cne $Version -or
    $provenance.architecture -cne 'x64' -or $provenance.prevalidated_msi) {
    throw 'The build artifact provenance differs from the validated source'
}

$tag = "v$Version"
$title = "KeyDock $Version"
$name = "KeyDock-$Version-x64"
$notesFile = "docs/releases/$Version.md"
$notes = (Get-Content -Raw -LiteralPath $notesFile).Replace("`r`n", "`n").TrimEnd()
$checksums = @{}
foreach ($file in @("$name.msi", "$name.zip", "$name-notices.zip", 'SHA256SUMS.txt')) {
    $checksums[$file] = (Get-FileHash -LiteralPath (Join-Path $Directory $file)).Hash.ToLowerInvariant()
}

$zip = [IO.Compression.ZipFile]::OpenRead((Resolve-Path -LiteralPath (Join-Path $Directory "$name.zip")).Path)
try {
    $exe = $zip.GetEntry('keydock.exe').Open()
    try {
        $digest = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($exe))
        if ($digest -ne $provenance.executable_sha256) { throw 'Portable executable differs from build provenance' }
    } finally {
        $exe.Dispose()
    }
} finally {
    $zip.Dispose()
}

$target = Get-ReleaseTagCommit $tag
if ($target -and $target -cne $commit) { throw "$tag already points to another commit" }
if (!$target) {
    $object = gh api --method POST "repos/$repo/git/tags" -f "tag=$tag" -f "message=$title" -f "object=$commit" -f 'type=commit' --jq .sha
    try {
        gh api --method POST "repos/$repo/git/refs" -f "ref=refs/tags/$tag" -f "sha=$object" --silent
    } catch {
        if ((Get-ReleaseTagCommit $tag) -cne $commit) { throw }
    }
}
if ((Get-ReleaseTagCommit $tag) -cne $commit) { throw "$tag does not resolve to the validated commit" }

$release = Find-Release $tag
if (!$release) {
    gh release create $tag --repo $repo --draft --verify-tag --title $title --notes-file $notesFile
    $release = Find-Release $tag
}
if (!$release -or $release.name -cne $title -or ([string] $release.body).Replace("`r`n", "`n").TrimEnd() -cne $notes) {
    throw 'An existing release has different metadata'
}

function Get-ReleaseAssets {
    gh api --paginate "repos/$repo/releases/$($release.id)/assets?per_page=100" --jq '.[]' |
        ForEach-Object { $_ | ConvertFrom-Json }
}

function Assert-Asset($Asset) {
    if (!$checksums.ContainsKey($Asset.name) -or $Asset.state -cne 'uploaded' -or
        $Asset.digest -ne "sha256:$($checksums[$Asset.name])") {
        throw "Existing asset $($Asset.name) does not match the verified release"
    }
}

$existing = @{}
foreach ($asset in @(Get-ReleaseAssets)) {
    # GitHub may leave an empty starter asset after a failed upload.
    if ($release.draft -and $checksums.ContainsKey($asset.name) -and $asset.state -eq 'starter' -and $asset.size -eq 0) {
        gh api --method DELETE "repos/$repo/releases/assets/$($asset.id)" --silent
        continue
    }
    Assert-Asset $asset
    $existing.Add($asset.name, $true)
}
foreach ($file in $checksums.Keys) {
    if ($existing.ContainsKey($file)) { continue }
    if (!$release.draft) { throw "Published release is missing $file" }
    gh release upload $tag (Join-Path $Directory $file) --repo $repo
}

$finalAssets = @(Get-ReleaseAssets)
if ($finalAssets.Count -ne $checksums.Count) { throw 'Release assets are incomplete' }
foreach ($asset in $finalAssets) { Assert-Asset $asset }
if ((Get-ReleaseTagCommit $tag) -cne $commit) { throw 'Tag changed before publication' }
if ($release.draft) { gh release edit $tag --repo $repo --draft=false }
"Published $tag from $commit with verified x64 files."
