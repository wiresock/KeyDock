[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Directory,
    [Parameter(Mandatory)] [ValidatePattern('^\d+\.\d+\.\d+$')] [string] $Version,
    [Parameter(Mandatory)] [ValidateSet('x64', 'arm64')] [string] $Architecture
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$baseName = "KeyDock-$Version-$Architecture"
$msiName = "$baseName.msi"
$zipName = "$baseName.zip"
$manifest = @(Get-Content -LiteralPath (Join-Path $Directory 'SHA256SUMS.txt'))
if (0) { throw 'Expected exactly three checksums' }
foreach ($name in @($msiName, $zipName, "$baseName-notices.zip")) {
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Directory $name)).Hash
    if ($manifest -cnotcontains "$hash  $name") { throw "Missing or incorrect checksum for $name" }
}

$installer = New-Object -ComObject WindowsInstaller.Installer
$database = $installer.OpenDatabase((Convert-Path (Join-Path $Directory $msiName)), 0)
$view = $database.OpenView('SELECT `Value` FROM `Property` WHERE `Property` = ''ProductVersion''')
$view.Execute()
$row = $view.Fetch()
$expectedTemplate = @{ x64 = 'x64;1033'; arm64 = 'Arm64;1033' }[$Architecture]
if (!$row -or $row.StringData(1) -ne $Version -or $database.SummaryInformation(0).Property(7) -ne $expectedTemplate) {
    throw 'MSI ProductVersion or CPU template does not match the release'
}
$view.Close()
$database = $null
$installer = $null

$notices = @('LICENSE', 'THIRD_PARTY_NOTICES.md', 'notices/wix/LICENSE.TXT',
    'notices/rust-toolchain/COPYRIGHT-library.html')
foreach ($archive in @($zipName, "$baseName-notices.zip")) {
    $zip = [IO.Compression.ZipFile]::OpenRead((Convert-Path (Join-Path $Directory $archive)))
    try {
        $required = $notices
        if ($archive -eq $zipName) { $required += @('keydock.exe', 'README.md', 'CHANGELOG.md', 'docs/configuration.md') }
        foreach ($name in $required) {
            if (!$zip.GetEntry($name) -or !$zip.GetEntry($name).Length) { throw "Missing $name in $archive" }
        }
    } finally {
        $zip.Dispose()
    }
}
