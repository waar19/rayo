param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [string]$PackageIdentifier = "waar19.Rayo",
    [string]$Repository = "waar19/rayo",
    [string]$InstallerAssetName = "RayoSetup.exe",
    [string]$Publisher = "waar19",
    [string]$PackageName = "Rayo",
    [string]$OutputRoot = "dist/winget",
    [string]$Token = $env:WINGET_GITHUB_TOKEN
)

$ErrorActionPreference = "Stop"

if ([string]::IsNullOrWhiteSpace($Token)) {
    throw "Missing token. Set -Token or WINGET_GITHUB_TOKEN with repo scope to submit PR."
}

$normalizedVersion = $Version.Trim().TrimStart("v")
if ([string]::IsNullOrWhiteSpace($normalizedVersion)) {
    throw "Invalid version value: '$Version'"
}

$installerUrl = "https://github.com/$Repository/releases/download/v$normalizedVersion/$InstallerAssetName"
$tempRoot = Join-Path $env:TEMP "rayo-winget-submit-$normalizedVersion"
$toolPath = Join-Path $tempRoot "wingetcreate.exe"
$installerPath = Join-Path $tempRoot $InstallerAssetName
$manifestScript = Join-Path $PSScriptRoot "generate-winget-manifest.ps1"

if (Test-Path $tempRoot) {
    Remove-Item -Path $tempRoot -Recurse -Force
}
New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null

Write-Host "Downloading wingetcreate..."
Invoke-WebRequest -Uri "https://aka.ms/wingetcreate/latest" -OutFile $toolPath

Write-Host "Downloading installer asset: $installerUrl"
Invoke-WebRequest -Uri $installerUrl -OutFile $installerPath

Write-Host "Generating Winget manifests for $PackageIdentifier v$normalizedVersion"
& $manifestScript `
    -Version $normalizedVersion `
    -InstallerUrl $installerUrl `
    -InstallerPath $installerPath `
    -Publisher $Publisher `
    -PackageIdentifier $PackageIdentifier `
    -PackageName $PackageName `
    -OutputRoot $OutputRoot `
    -InstallerType "inno"

$manifestDir = Join-Path $OutputRoot $normalizedVersion
if (-not (Test-Path $manifestDir)) {
    throw "Generated manifest directory not found: $manifestDir"
}
$manifestDir = (Resolve-Path -Path $manifestDir).Path

Write-Host "Submitting winget package for $PackageIdentifier v$normalizedVersion"
& $toolPath submit `
    --prtitle "New package: $PackageIdentifier $normalizedVersion" `
    --token $Token `
    --no-open `
    $manifestDir

if ($LASTEXITCODE -ne 0) {
    throw "wingetcreate submit failed with exit code $LASTEXITCODE."
}

Write-Host "winget submission command completed."

if (Test-Path $tempRoot) {
    Remove-Item -Path $tempRoot -Recurse -Force
}
