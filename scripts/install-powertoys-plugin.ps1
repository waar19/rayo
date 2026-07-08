param(
    [string]$PluginZipPath = "dist/powertoys-run/RayoPlugin.zip",
    [switch]$AutoInstallDependencies,
    [switch]$RestartPowerToys
)

$ErrorActionPreference = "Stop"

function Test-Command {
    param([string]$Name)
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

function Install-WithWinget {
    param([string]$Id)
    winget install --id $Id -e --accept-package-agreements --accept-source-agreements
}

if (-not (Test-Path $PluginZipPath)) {
    throw "Plugin zip not found: $PluginZipPath. Build/publish first or download CI artifact."
}

$pluginRoot = Join-Path $env:LOCALAPPDATA "Microsoft/PowerToys/PowerToys Run/Plugins/Rayo"
$powerToysExe = Join-Path $env:LOCALAPPDATA "PowerToys/PowerToys.exe"

$hasPowerToys = Test-Path $powerToysExe
$hasDotnet = Test-Command "dotnet"
$hasDesktopRuntime = $false
if ($hasDotnet) {
    $runtimeLines = dotnet --list-runtimes
    $hasDesktopRuntime = ($runtimeLines | Select-String "Microsoft.WindowsDesktop.App 9\.") -ne $null
}

if (-not $hasPowerToys) {
    Write-Warning "PowerToys not detected."
    if ($AutoInstallDependencies) {
        if (-not (Test-Command "winget")) {
            throw "winget not found. Install PowerToys manually: https://github.com/microsoft/PowerToys/releases"
        }
        Write-Host "Installing PowerToys..."
        Install-WithWinget -Id "Microsoft.PowerToys"
        $hasPowerToys = Test-Path $powerToysExe
    }
}

if (-not $hasDesktopRuntime) {
    Write-Warning ".NET Desktop Runtime 9 not detected."
    if ($AutoInstallDependencies) {
        if (-not (Test-Command "winget")) {
            throw "winget not found. Install .NET Desktop Runtime manually: https://dotnet.microsoft.com/download/dotnet/9.0"
        }
        Write-Host "Installing .NET Desktop Runtime 9..."
        Install-WithWinget -Id "Microsoft.DotNet.DesktopRuntime.9"
    }
}

if (-not $hasPowerToys) {
    throw "PowerToys still not detected. Install it, then rerun installer."
}

if (Test-Path $pluginRoot) {
    Remove-Item -Recurse -Force $pluginRoot
}
New-Item -ItemType Directory -Path $pluginRoot -Force | Out-Null
Expand-Archive -Path $PluginZipPath -DestinationPath $pluginRoot -Force

Write-Host "Rayo plugin installed at: $pluginRoot"

if ($RestartPowerToys) {
    Get-Process -Name "PowerToys" -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    if (Test-Path $powerToysExe) {
        Start-Process $powerToysExe | Out-Null
        Write-Host "PowerToys restarted."
    }
}

Write-Host "Done. Open PowerToys Run and use: ry <query>"
