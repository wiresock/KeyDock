[CmdletBinding()]
param(
    [Parameter(Mandatory)] [ValidateSet('x64', 'arm64')] [string] $Architecture,
    [Parameter(Mandatory)] [string] $KeyDockExe,
    [Parameter(Mandatory)] [string] $OutputDirectory,
    # Use a previously validated MSI built from this executable.
    [string] $ValidatedMsi
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$manifest = Get-Content -Raw -LiteralPath (Join-Path $root 'Cargo.toml')
$version = [regex]::Match($manifest, '(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"').Groups[1].Value
if (!$version) { throw 'Missing Cargo package version' }

$exe = Get-Item -LiteralPath $KeyDockExe
$expectedMachine = @{ x64 = 0x8664; arm64 = 0xAA64 }[$Architecture]
$bytes = [IO.File]::ReadAllBytes($exe.FullName)
$pe = [BitConverter]::ToInt32($bytes, 0x3C)
if ($exe.VersionInfo.ProductVersion -ne $version -or [BitConverter]::ToUInt16($bytes, $pe + 4) -ne $expectedMachine) {
    throw 'Executable ProductVersion or CPU architecture does not match the release'
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$destination = (Resolve-Path -LiteralPath $OutputDirectory).Path
if (Get-ChildItem -LiteralPath $destination -Force | Select-Object -First 1) { throw 'Use an empty output directory' }
$name = "KeyDock-$version-$Architecture"
$msi = Join-Path $destination "$name.msi"
if ($ValidatedMsi) {
    Copy-Item -LiteralPath $ValidatedMsi -Destination $msi
} else {
    & (Join-Path $PSScriptRoot 'build.ps1') -Architecture $Architecture -KeyDockExe $exe.FullName -OutputDirectory $destination
}

$noticeFiles = [ordered]@{
    'LICENSE' = (Join-Path $root 'LICENSE')
    'THIRD_PARTY_NOTICES.md' = (Join-Path $root 'THIRD_PARTY_NOTICES.md')
}
foreach ($file in Get-ChildItem -LiteralPath (Join-Path $root 'notices') -Recurse -File) {
    $entry = [IO.Path]::GetRelativePath($root, $file.FullName).Replace('\', '/')
    $noticeFiles[$entry] = $file.FullName
}
$sysroot = rustc --print sysroot
if ($LASTEXITCODE -ne 0) { throw 'Cannot find Rust toolchain notices' }
$rustDocs = Join-Path $sysroot 'share/doc/rust'
$noticeFiles['notices/rust-toolchain/COPYRIGHT-library.html'] = Join-Path $rustDocs 'COPYRIGHT-library.html'
foreach ($file in Get-ChildItem -LiteralPath (Join-Path $rustDocs 'licenses') -File) {
    $noticeFiles["notices/rust-toolchain/licenses/$($file.Name)"] = $file.FullName
}

function Write-Archive([string] $path, [System.Collections.IDictionary] $files) {
    $zip = [IO.Compression.ZipFile]::Open($path, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($entry in $files.Keys) {
            [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $files[$entry], $entry) | Out-Null
        }
    } finally {
        $zip.Dispose()
    }
}

Write-Archive (Join-Path $destination "$name-notices.zip") $noticeFiles
$docs = @('README.md', 'CHANGELOG.md', 'CONTRIBUTING.md', 'SECURITY.md', 'docs/configuration.md', 'docs/releasing.md')
$releaseNotes = "docs/releases/$version.md"
if (Test-Path -LiteralPath (Join-Path $root $releaseNotes) -PathType Leaf) { $docs += $releaseNotes }
foreach ($doc in $docs) {
    $noticeFiles[$doc] = Join-Path $root $doc
}
$noticeFiles['keydock.exe'] = $exe.FullName
Write-Archive (Join-Path $destination "$name.zip") $noticeFiles

$sums = foreach ($file in @("$name.msi", "$name.zip", "$name-notices.zip")) {
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $destination $file)).Hash
    "$hash  $file"
}
Set-Content -LiteralPath (Join-Path $destination 'SHA256SUMS.txt') -Value $sums -Encoding ascii
& (Join-Path $PSScriptRoot 'verify-release.ps1') -Directory $destination -Version $version -Architecture $Architecture

$signatures = @(Get-AuthenticodeSignature -FilePath $exe.FullName,$msi)
foreach ($signature in $signatures) {
    if ($signature.Status -notin @('Valid', 'NotSigned')) { throw "Signature validation failed: $($signature.Status)" }
}
[ordered]@{
    source = (git -C $root rev-parse HEAD)
    version = $version
    architecture = $Architecture
    executable_sha256 = (Get-FileHash -LiteralPath $exe.FullName).Hash
    executable_authenticode = $signatures[0].Status.ToString()
    msi_authenticode = $signatures[1].Status.ToString()
    prevalidated_msi = [bool] $ValidatedMsi
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $destination 'provenance.json')
Get-Content -LiteralPath (Join-Path $destination 'SHA256SUMS.txt')
