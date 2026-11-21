<#
.SYNOPSIS
  Builds the KeyDock MSI packages (x64 and/or ARM64) with WiX Toolset 6.

.DESCRIPTION
  For each requested architecture: builds keydock.exe with cargo (unless
  -SkipCargo or -KeyDockExe is given), then runs `dotnet build` on
  KeyDock.wixproj. The MSI files are written to installer\out\ as
  KeyDock-<version>-x64.msi and KeyDock-<version>-arm64.msi.

  Requirements: Windows, the .NET SDK (6 or newer), and for the cargo step
  Rust with the x86_64-pc-windows-msvc / aarch64-pc-windows-msvc targets and
  the MSVC build tools. WiX itself is restored from NuGet by `dotnet build`.

.EXAMPLE
  .\installer\build.ps1                      # both architectures
  .\installer\build.ps1 -Architecture x64
  .\installer\build.ps1 -Architecture arm64 -KeyDockExe D:\out\keydock-aarch64.exe
#>
[CmdletBinding()]
param(
    [ValidateSet('x64', 'arm64', 'all')]
    [string] $Architecture = 'all',

    # Use this prebuilt keydock.exe instead of building it (single architecture only).
    [string] $KeyDockExe,

    # Use the existing cargo release build in target\<triple>\release.
    [switch] $SkipCargo,

    # Override the version (default: [package] version in Cargo.toml).
    [string] $Version,

    [string] $OutputDirectory
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$project = Join-Path $PSScriptRoot 'KeyDock.wixproj'
if (-not $OutputDirectory) { $OutputDirectory = Join-Path $PSScriptRoot 'out' }
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path $OutputDirectory).Path

if (-not $Version) {
    $cargoToml = Get-Content -Raw (Join-Path $repositoryRoot 'Cargo.toml')
    $match = [regex]::Match($cargoToml, '(?m)^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"')
    if (-not $match.Success) { throw 'Cannot read the version from Cargo.toml.' }
    $Version = $match.Groups[1].Value
}

# @(...) around the whole expression: an `if` that yields one element would
# otherwise be unwrapped to a string, and `.Count` fails under StrictMode.
$architectures = @(if ($Architecture -eq 'all') { 'x64', 'arm64' } else { $Architecture })
if ($KeyDockExe -and $architectures.Count -ne 1) {
    throw '-KeyDockExe needs a single -Architecture (x64 or arm64).'
}

$targets = @{
    x64   = @{ Triple = 'x86_64-pc-windows-msvc';  Platform = 'x64' }
    arm64 = @{ Triple = 'aarch64-pc-windows-msvc'; Platform = 'ARM64' }
}

foreach ($arch in $architectures) {
    $t = $targets[$arch]
    $exe = $KeyDockExe
    if (-not $exe) {
        $exe = Join-Path $repositoryRoot "target\$($t.Triple)\release\keydock.exe"
        if (-not $SkipCargo) {
            Write-Host "==> cargo build --release --target $($t.Triple)"
            Push-Location $repositoryRoot
            try {
                & cargo build --release --target $t.Triple
                if ($LASTEXITCODE -ne 0) { throw "cargo build failed for $($t.Triple) (exit code $LASTEXITCODE)." }
            }
            finally { Pop-Location }
        }
    }
    if (-not (Test-Path -LiteralPath $exe)) { throw "keydock.exe not found: $exe" }
    $exe = (Resolve-Path -LiteralPath $exe).Path

    $fileVersion = (Get-Item -LiteralPath $exe).VersionInfo.ProductVersion
    if ($fileVersion -and -not $fileVersion.StartsWith($Version)) {
        Write-Warning "keydock.exe reports version $fileVersion, the MSI will be $Version."
    }

    $name = "KeyDock-$Version-$arch"
    Write-Host "==> WiX: $name.msi"
    & dotnet build $project `
        --configuration Release `
        "--property:Platform=$($t.Platform)" `
        "--property:ProductVersion=$Version" `
        "--property:KeyDockExe=$exe" `
        "--property:OutputName=$name" `
        "--property:OutputPath=$OutputDirectory\" `
        "--property:IntermediateOutputPath=$(Join-Path $PSScriptRoot "obj\$arch")\"
    if ($LASTEXITCODE -ne 0) { throw "WiX build failed for $arch (exit code $LASTEXITCODE)." }

    $msi = Join-Path $OutputDirectory "$name.msi"
    if (-not (Test-Path -LiteralPath $msi)) { throw "WiX did not produce $msi" }
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $msi).Hash
    Write-Host "    $msi"
    Write-Host "    SHA256 $hash"
}
