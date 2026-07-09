param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [string]$PackageIdentifier = "waar19.Rayo",
    [string]$Repository = "waar19/rayo",
    [string]$InstallerAssetName = "RayoSetup.exe",
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
$toolPath = Join-Path $env:TEMP "wingetcreate.exe"

Write-Host "Downloading wingetcreate..."
Invoke-WebRequest -Uri "https://aka.ms/wingetcreate/latest" -OutFile $toolPath

Write-Host "Submitting winget update for $PackageIdentifier v$normalizedVersion"
& $toolPath update $PackageIdentifier `
    --version $normalizedVersion `
    --urls $installerUrl `
    --prtitle "Update $PackageIdentifier to $normalizedVersion" `
    --submit `
    --token $Token

if ($LASTEXITCODE -ne 0) {
    throw "wingetcreate update failed with exit code $LASTEXITCODE."
}

Write-Host "winget submission command completed."
