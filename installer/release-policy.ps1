# Shared version policy for read-only PR and merged-commit checks.

function Get-KeyDockVersion([string] $Commit) {
    if ($Commit -cnotmatch '^[0-9a-f]{40}$') { throw 'Expected a Git commit SHA' }
    $toml = (git show "$($Commit):Cargo.toml") -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "Cannot read Cargo.toml at $Commit" }
    $package = [regex]::Match($toml, '(?ms)^\[package\][ \t]*\r?\n(.*?)(?=^\[|\z)')
    $versions = [regex]::Matches($package.Groups[1].Value, '(?m)^version[ \t]*=[ \t]*"((?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*))"[ \t]*(?:#.*)?$')
    if ($versions.Count -ne 1) { throw "Expected a package version in X.Y.Z format at $Commit" }
    return $versions[0].Groups[1].Value
}

function Get-ReleaseTagCommit([string] $Tag) {
    $ref = "refs/tags/$Tag"
    $rows = @(git ls-remote origin $ref "$ref^{}")
    if ($LASTEXITCODE -ne 0) { throw "Could not inspect $Tag" }
    $direct = $null
    foreach ($row in $rows) {
        $hash, $name = $row -split "`t", 2
        if ($name -ceq "$ref^{}") { return $hash }
        if ($name -ceq $ref) { $direct = $hash }
    }
    return $direct
}

function Get-KeyDockReleasePlan([string] $Before, [string] $Commit) {
    $previous = Get-KeyDockVersion $Before
    $next = Get-KeyDockVersion $Commit
    if ([version] $next -lt [version] $previous) { throw "Version decreased from $previous to $next" }

    $changed = $next -cne $previous
    if ($changed) {
        if ($next -eq '0.3.12') { throw 'v0.3.12 uses the owner-approved bootstrap release path' }
        $notesEntry = git ls-tree $Commit -- "docs/releases/$next.md"
        if ($LASTEXITCODE -ne 0 -or !$notesEntry) { throw "Missing docs/releases/$next.md" }
        $notes = (git show "$($Commit):docs/releases/$next.md") -join "`n"
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($notes)) { throw "Empty or unreadable release notes for $next" }
        $tagCommit = Get-ReleaseTagCommit "v$next"
        if ($tagCommit -and $tagCommit -cne $Commit) { throw "v$next already points to another commit" }
    }
    return [pscustomobject]@{ Version = $next; Changed = $changed }
}
