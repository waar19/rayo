param(
    [string]$OutputDir = "dist"
)

$ErrorActionPreference = "Stop"

Write-Host "Building release binaries..."
cargo build --release -p rayo-cli
cargo build --release -p rayo-service
cargo build --release -p rayo-gui

$releaseDir = Join-Path $PSScriptRoot "..\target\release"
$outRoot = Join-Path $PSScriptRoot "..\$OutputDir"
$packageDir = Join-Path $outRoot "rayo-windows"
$zipPath = Join-Path $outRoot "rayo-windows.zip"

if (Test-Path $packageDir) {
    Remove-Item -Recurse -Force $packageDir
}
if (Test-Path $zipPath) {
    Remove-Item -Force $zipPath
}

New-Item -ItemType Directory -Path $packageDir -Force | Out-Null

$binaries = @("rayo-cli.exe", "rayo-service.exe", "rayo-gui.exe")
foreach ($bin in $binaries) {
    $source = Join-Path $releaseDir $bin
    if (-not (Test-Path $source)) {
        throw "Missing expected binary: $source"
    }
    Copy-Item $source -Destination $packageDir
}

Copy-Item (Join-Path $PSScriptRoot "..\README.md") -Destination $packageDir
Copy-Item (Join-Path $PSScriptRoot "..\README.es.md") -Destination $packageDir
Copy-Item (Join-Path $PSScriptRoot "..\LICENSE") -Destination $packageDir

New-Item -ItemType Directory -Path $outRoot -Force | Out-Null
Compress-Archive -Path (Join-Path $packageDir "*") -DestinationPath $zipPath

Write-Host "Release package ready at: $zipPath"
