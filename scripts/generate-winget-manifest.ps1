param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$InstallerUrl,
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [string]$Publisher = "waar19",
    [string]$PackageIdentifier = "waar19.Rayo",
    [string]$PackageName = "Rayo",
    [string]$OutputRoot = "dist/winget",
    [ValidateSet("inno", "zip")]
    [string]$InstallerType = "inno"
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $InstallerPath)) {
    throw "Installer not found: $InstallerPath"
}

$hash = (Get-FileHash -Path $InstallerPath -Algorithm SHA256).Hash.ToUpperInvariant()
$versionDir = Join-Path $OutputRoot $Version
New-Item -ItemType Directory -Path $versionDir -Force | Out-Null

$defaultLocalePath = Join-Path $versionDir "$PackageIdentifier.locale.en-US.yaml"
$versionPath = Join-Path $versionDir "$PackageIdentifier.yaml"
$installerPathOut = Join-Path $versionDir "$PackageIdentifier.installer.yaml"

$defaultLocale = @"
PackageIdentifier: $PackageIdentifier
PackageVersion: $Version
PackageLocale: en-US
Publisher: $Publisher
PackageName: $PackageName
License: MIT
ShortDescription: Ultra-fast NTFS file search for Windows.
Homepage: https://github.com/waar19/rayo
ManifestType: defaultLocale
ManifestVersion: 1.9.0
"@

$versionManifest = @"
PackageIdentifier: $PackageIdentifier
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.9.0
"@

if ($InstallerType -eq "zip") {
    $installerManifest = @"
PackageIdentifier: $PackageIdentifier
PackageVersion: $Version
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
  - RelativeFilePath: rayo-gui.exe
    PortableCommandAlias: rayo-gui
Installers:
  - Architecture: x64
    InstallerUrl: $InstallerUrl
    InstallerSha256: $hash
ManifestType: installer
ManifestVersion: 1.9.0
"@
} else {
    $installerManifest = @"
PackageIdentifier: $PackageIdentifier
PackageVersion: $Version
InstallerType: inno
Installers:
  - Architecture: x64
    InstallerUrl: $InstallerUrl
    InstallerSha256: $hash
ManifestType: installer
ManifestVersion: 1.9.0
"@
}

Set-Content -Path $defaultLocalePath -Value $defaultLocale -Encoding UTF8
Set-Content -Path $versionPath -Value $versionManifest -Encoding UTF8
Set-Content -Path $installerPathOut -Value $installerManifest -Encoding UTF8

$zipPath = Join-Path $OutputRoot "rayo-winget-manifest-$Version.zip"
if (Test-Path $zipPath) {
    Remove-Item -Force $zipPath
}
Compress-Archive -Path (Join-Path $versionDir "*") -DestinationPath $zipPath -Force

Write-Host "Winget manifest generated:"
Write-Host " - $defaultLocalePath"
Write-Host " - $versionPath"
Write-Host " - $installerPathOut"
Write-Host "Manifest zip: $zipPath"
