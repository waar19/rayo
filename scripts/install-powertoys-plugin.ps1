param(
    [string]$PluginZipPath = "",
    [bool]$AutoInstallDependencies = $true,
    [bool]$RestartPowerToys = $true,
    [string]$Repository = "waar19/rayo",
    [string]$ReleaseTag = ""
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

function Get-PowerToysExePath {
    $candidates = @(
        (Join-Path $env:LOCALAPPDATA "PowerToys/PowerToys.exe"),
        (Join-Path $env:ProgramFiles "PowerToys/PowerToys.exe")
    )
    $programFilesX86 = ${env:ProgramFiles(x86)}
    if ($env:ProgramFiles -ne $programFilesX86 -and $programFilesX86) {
        $candidates += (Join-Path $programFilesX86 "PowerToys/PowerToys.exe")
    }
    foreach ($candidate in $candidates) {
        if ($candidate -and (Test-Path $candidate)) {
            return $candidate
        }
    }
    return $null
}

function Test-DotNetDesktopRuntime {
    if (-not (Test-Command "dotnet")) {
        return $false
    }
    $runtimeLines = dotnet --list-runtimes
    return ($runtimeLines | Select-String "Microsoft.WindowsDesktop.App 9\.") -ne $null
}

function Stop-PowerToysIfRunning {
    param([bool]$ShouldStop)
    if (-not $ShouldStop) {
        return $false
    }
    $procs = Get-Process -Name "PowerToys" -ErrorAction SilentlyContinue
    if ($null -eq $procs) {
        return $false
    }
    $procs | Stop-Process -Force
    Start-Sleep -Milliseconds 700
    return $true
}

function Resolve-PluginZipFromRelease {
    param(
        [string]$Repo,
        [string]$Tag
    )

    $headers = @{ "User-Agent" = "rayo-powertoys-installer" }
    if ([string]::IsNullOrWhiteSpace($Tag)) {
        $url = "https://api.github.com/repos/$Repo/releases/latest"
    } else {
        $url = "https://api.github.com/repos/$Repo/releases/tags/$Tag"
    }

    Write-Host "Fetching release metadata from $url"
    $release = Invoke-RestMethod -Uri $url -Headers $headers
    $asset = $release.assets | Where-Object { $_.name -eq "RayoPlugin.zip" } | Select-Object -First 1
    if ($null -eq $asset) {
        throw "RayoPlugin.zip not found in release '$($release.tag_name)'."
    }

    $tmpDir = Join-Path $env:TEMP "rayo-plugin-install"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    $zipPath = Join-Path $tmpDir "RayoPlugin.zip"
    Write-Host "Downloading plugin zip: $($asset.browser_download_url)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $zipPath -Headers $headers
    return $zipPath
}

$defaultLocalZip = Join-Path (Get-Location) "dist/powertoys-run/RayoPlugin.zip"
if ([string]::IsNullOrWhiteSpace($PluginZipPath)) {
    if (Test-Path $defaultLocalZip) {
        $PluginZipPath = $defaultLocalZip
        Write-Host "Using local plugin zip: $PluginZipPath"
    } else {
        $PluginZipPath = Resolve-PluginZipFromRelease -Repo $Repository -Tag $ReleaseTag
    }
}

if (-not (Test-Path $PluginZipPath)) {
    throw "Plugin zip not found: $PluginZipPath"
}

$pluginRoot = Join-Path $env:LOCALAPPDATA "Microsoft/PowerToys/PowerToys Run/Plugins/Rayo"
$powerToysExe = Get-PowerToysExePath
$hasPowerToys = $null -ne $powerToysExe
$hasDesktopRuntime = Test-DotNetDesktopRuntime

if (-not $hasPowerToys) {
    Write-Warning "PowerToys not detected."
    if ($AutoInstallDependencies) {
        if (-not (Test-Command "winget")) {
            throw "winget not found. Install PowerToys manually: https://github.com/microsoft/PowerToys/releases"
        }
        Write-Host "Installing PowerToys..."
        Install-WithWinget -Id "Microsoft.PowerToys"
        $powerToysExe = Get-PowerToysExePath
        $hasPowerToys = $null -ne $powerToysExe
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
        $hasDesktopRuntime = Test-DotNetDesktopRuntime
    }
}

if (-not $hasPowerToys) {
    throw "PowerToys still not detected. Install it, then rerun installer."
}

if (-not $hasDesktopRuntime) {
    throw ".NET Desktop Runtime 9 still not detected. Install it, then rerun installer."
}

$powerToysWasRunning = Stop-PowerToysIfRunning -ShouldStop $RestartPowerToys

if (Test-Path $pluginRoot) {
    try {
        Remove-Item -Recurse -Force $pluginRoot
    } catch {
        throw "Plugin directory is locked. Close PowerToys and rerun installer, or use -RestartPowerToys `$true."
    }
}
New-Item -ItemType Directory -Path $pluginRoot -Force | Out-Null
Expand-Archive -Path $PluginZipPath -DestinationPath $pluginRoot -Force

Write-Host "Rayo plugin installed at: $pluginRoot"

if ($RestartPowerToys) {
    if ($powerToysExe -and (Test-Path $powerToysExe)) {
        Start-Process $powerToysExe | Out-Null
        if ($powerToysWasRunning) {
            Write-Host "PowerToys restarted."
        } else {
            Write-Host "PowerToys started."
        }
    }
}

Write-Host "Done. Open PowerToys Run and use: ry <query>"
