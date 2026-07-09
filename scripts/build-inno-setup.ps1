param(
    [Parameter(Mandatory = $true)]
    [string]$Version
)

$ErrorActionPreference = "Stop"

$compiler = Get-Command iscc -ErrorAction SilentlyContinue
if ($null -eq $compiler) {
    throw "Inno Setup compiler 'iscc' was not found in PATH."
}

$issPath = Join-Path $PSScriptRoot "..\installer\RayoSetup.iss"
if (-not (Test-Path $issPath)) {
    throw "Installer definition not found: $issPath"
}

Write-Host "Building Inno Setup installer for Rayo v$Version"
& $compiler.Source "/DAppVersion=$Version" $issPath
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup build failed with exit code $LASTEXITCODE."
}

$output = Join-Path $PSScriptRoot "..\dist\RayoSetup.exe"
if (-not (Test-Path $output)) {
    throw "Expected installer output not found: $output"
}

Write-Host "Installer generated at: $output"
