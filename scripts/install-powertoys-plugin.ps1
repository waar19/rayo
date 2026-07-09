param(
    [string]$PluginZipPath = "",
    [string]$WindowsBundleZipPath = "",
    [bool]$AutoInstallDependencies = $true,
    [bool]$RestartPowerToys = $true,
    [string]$Repository = "waar19/rayo",
    [string]$ReleaseTag = "",
    [string]$ServiceDrives = "C",
    [bool]$InstallBackgroundTask = $true
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

function Stop-RayoServiceIfRunning {
    param(
        [string]$ServiceRootPath
    )

    $existingCli = Join-Path $ServiceRootPath "rayo-cli.exe"
    if (-not (Test-Path $existingCli)) {
        return
    }

    Write-Host "Stopping existing Rayo background task before updating binaries..."
    try {
        $stopProc = Start-Process -FilePath $existingCli -ArgumentList @("service", "uninstall") -Verb RunAs -Wait -PassThru
        if ($stopProc.ExitCode -ne 0) {
            Write-Warning "Could not uninstall existing Rayo background task automatically (exit code $($stopProc.ExitCode))."
        }
    } catch {
        Write-Warning "Could not stop existing Rayo background task automatically: $($_.Exception.Message)"
    }

    Start-Sleep -Milliseconds 1200
}

function Copy-WithRetry {
    param(
        [string]$Source,
        [string]$Destination,
        [int]$MaxAttempts = 8,
        [int]$DelayMs = 900
    )

    for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
        try {
            Copy-Item $Source -Destination $Destination -Force
            return
        } catch {
            if ($attempt -eq $MaxAttempts) {
                throw
            }
            Write-Warning "File is busy while updating '$Destination' (attempt $attempt/$MaxAttempts). Retrying..."
            Start-Sleep -Milliseconds $DelayMs
        }
    }
}

function Get-ReleaseMetadata {
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
    return $release
}

function Download-ReleaseAsset {
    param(
        $Release,
        [string]$AssetName
    )

    $asset = $Release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
    if ($null -eq $asset) {
        throw "$AssetName not found in release '$($Release.tag_name)'."
    }

    $headers = @{ "User-Agent" = "rayo-powertoys-installer" }
    $tmpDir = Join-Path $env:TEMP "rayo-plugin-install"
    New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
    $assetPath = Join-Path $tmpDir $AssetName
    Write-Host "Downloading ${AssetName}: $($asset.browser_download_url)"
    Invoke-WebRequest -Uri $asset.browser_download_url -OutFile $assetPath -Headers $headers
    return $assetPath
}

$defaultLocalZip = Join-Path (Get-Location) "dist/powertoys-run/RayoPlugin.zip"
$defaultWindowsBundleZip = Join-Path (Get-Location) "dist/rayo-windows.zip"
$release = $null

if ([string]::IsNullOrWhiteSpace($PluginZipPath)) {
    if (Test-Path $defaultLocalZip) {
        $PluginZipPath = $defaultLocalZip
        Write-Host "Using local plugin zip: $PluginZipPath"
    } else {
        $release = Get-ReleaseMetadata -Repo $Repository -Tag $ReleaseTag
        $PluginZipPath = Download-ReleaseAsset -Release $release -AssetName "RayoPlugin.zip"
    }
}

if (-not (Test-Path $PluginZipPath)) {
    throw "Plugin zip not found: $PluginZipPath"
}

if ([string]::IsNullOrWhiteSpace($WindowsBundleZipPath)) {
    if (Test-Path $defaultWindowsBundleZip) {
        $WindowsBundleZipPath = $defaultWindowsBundleZip
        Write-Host "Using local Windows bundle zip: $WindowsBundleZipPath"
    } else {
        if ($null -eq $release) {
            $release = Get-ReleaseMetadata -Repo $Repository -Tag $ReleaseTag
        }
        $WindowsBundleZipPath = Download-ReleaseAsset -Release $release -AssetName "rayo-windows.zip"
    }
}

if (-not (Test-Path $WindowsBundleZipPath)) {
    throw "Windows bundle zip not found: $WindowsBundleZipPath"
}

$pluginRoot = Join-Path $env:LOCALAPPDATA "Microsoft/PowerToys/PowerToys Run/Plugins/Rayo"
$serviceRoot = Join-Path $env:LOCALAPPDATA "Rayo"
$powerToysExe = Get-PowerToysExePath
$hasPowerToys = $null -ne $powerToysExe

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

if (-not $hasPowerToys) {
    throw "PowerToys still not detected. Install it, then rerun installer."
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

$serviceTempDir = Join-Path $env:TEMP "rayo-service-install"
if (Test-Path $serviceTempDir) {
    Remove-Item -Recurse -Force $serviceTempDir
}
New-Item -ItemType Directory -Path $serviceTempDir -Force | Out-Null
Expand-Archive -Path $WindowsBundleZipPath -DestinationPath $serviceTempDir -Force

$serviceSource = Join-Path $serviceTempDir "rayo-service.exe"
$cliSource = Join-Path $serviceTempDir "rayo-cli.exe"
if (-not (Test-Path $serviceSource)) {
    throw "rayo-service.exe not found in bundle: $WindowsBundleZipPath"
}
if (-not (Test-Path $cliSource)) {
    throw "rayo-cli.exe not found in bundle: $WindowsBundleZipPath"
}

New-Item -ItemType Directory -Path $serviceRoot -Force | Out-Null
Stop-RayoServiceIfRunning -ServiceRootPath $serviceRoot
Copy-WithRetry -Source $serviceSource -Destination (Join-Path $serviceRoot "rayo-service.exe")
Copy-WithRetry -Source $cliSource -Destination (Join-Path $serviceRoot "rayo-cli.exe")
Remove-Item -Recurse -Force $serviceTempDir
Write-Host "Rayo binaries installed at: $serviceRoot"

if ($InstallBackgroundTask) {
    $cliExe = Join-Path $serviceRoot "rayo-cli.exe"
    $serviceExe = Join-Path $serviceRoot "rayo-service.exe"
    Write-Host "Registering and starting background task: Rayo Service"
    $taskProc = Start-Process -FilePath $cliExe -ArgumentList @("service", "install", "--service-exe", $serviceExe, "--drives", $ServiceDrives) -Verb RunAs -Wait -PassThru
    if ($taskProc.ExitCode -ne 0) {
        throw "Failed to install background task using rayo-cli (exit code $($taskProc.ExitCode))."
    }
}

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
Write-Host "If needed, set RAYO_SERVICE_PATH to override service binary location."
