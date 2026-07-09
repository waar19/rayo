param(
    [bool]$RemoveData = $false,
    [bool]$RestartPowerToys = $true
)

$ErrorActionPreference = "Stop"

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

function Remove-PathWithRetry {
    param(
        [string]$Path,
        [int]$MaxAttempts = 8,
        [int]$DelayMs = 900
    )

    if (-not (Test-Path $Path)) {
        return $false
    }

    for ($attempt = 1; $attempt -le $MaxAttempts; $attempt++) {
        try {
            Remove-Item -Path $Path -Recurse -Force
            return $true
        } catch {
            if ($attempt -eq $MaxAttempts) {
                throw
            }
            Write-Warning "Path is busy while removing '$Path' (attempt $attempt/$MaxAttempts). Retrying..."
            Start-Sleep -Milliseconds $DelayMs
        }
    }
}

function Invoke-ElevatedPowerShell {
    param([string]$Command)
    $proc = Start-Process -FilePath "powershell" -ArgumentList @(
        "-NoProfile",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        $Command
    ) -Verb RunAs -Wait -PassThru
    return $proc.ExitCode
}

function Remove-ProgramDataRayo {
    $programDataBase = $env:ProgramData
    if ([string]::IsNullOrWhiteSpace($programDataBase)) {
        $programDataBase = "C:\ProgramData"
    }
    $programDataRoot = Join-Path $programDataBase "Rayo"
    if (-not (Test-Path $programDataRoot)) {
        return $false
    }

    $escapedPath = $programDataRoot.Replace("'", "''")
    $command = "if (Test-Path '$escapedPath') { Remove-Item -Path '$escapedPath' -Recurse -Force }"
    $exitCode = Invoke-ElevatedPowerShell -Command $command
    if ($exitCode -ne 0) {
        throw "Failed to remove ProgramData path '$programDataRoot' (exit code $exitCode)."
    }
    return $true
}

$pluginRoot = Join-Path $env:LOCALAPPDATA "Microsoft/PowerToys/PowerToys Run/Plugins/Rayo"
$serviceRoot = Join-Path $env:LOCALAPPDATA "Rayo"
$startMenuShortcut = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Rayo.lnk"
$guiSettingsPath = Join-Path $env:APPDATA "rayo/settings.json"
$guiSettingsDir = Split-Path -Parent $guiSettingsPath
$powerToysExe = Get-PowerToysExePath

$taskRemoved = $false
$shellRemoved = $false
$pluginRemoved = $false
$binariesRemoved = $false
$shortcutRemoved = $false
$dataRemoved = $false

$cliExe = Join-Path $serviceRoot "rayo-cli.exe"
if (Test-Path $cliExe) {
    Write-Host "Removing background task: Rayo Service"
    try {
        $taskProc = Start-Process -FilePath $cliExe -ArgumentList @("service", "uninstall") -Verb RunAs -Wait -PassThru
        if ($taskProc.ExitCode -eq 0) {
            $taskRemoved = $true
        } else {
            Write-Warning "rayo-cli service uninstall returned exit code $($taskProc.ExitCode)."
        }
    } catch {
        Write-Warning "Could not run rayo-cli service uninstall: $($_.Exception.Message)"
    }

    Write-Host "Removing Explorer shell integration for current user"
    try {
        & $cliExe shell uninstall | Out-Null
        if ($LASTEXITCODE -eq 0) {
            $shellRemoved = $true
        } else {
            Write-Warning "rayo-cli shell uninstall returned exit code $LASTEXITCODE."
        }
    } catch {
        Write-Warning "Could not run rayo-cli shell uninstall: $($_.Exception.Message)"
    }
} else {
    Write-Warning "rayo-cli.exe not found at '$cliExe'. Service/shell uninstall commands skipped."
}

if ($null -ne (Get-Process -Name "rayo-service" -ErrorAction SilentlyContinue)) {
    Write-Host "Stopping any remaining rayo-service process"
    $stopExitCode = Invoke-ElevatedPowerShell -Command "Get-Process -Name 'rayo-service' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue"
    if ($stopExitCode -ne 0) {
        Write-Warning "Could not force-stop rayo-service (exit code $stopExitCode)."
    }
    Start-Sleep -Milliseconds 800
}

$powerToysWasRunning = Stop-PowerToysIfRunning -ShouldStop $RestartPowerToys

$pluginRemoved = Remove-PathWithRetry -Path $pluginRoot
$binariesRemoved = Remove-PathWithRetry -Path $serviceRoot
$shortcutRemoved = Remove-PathWithRetry -Path $startMenuShortcut

if ($RemoveData) {
    Write-Host "Removing optional data files (ProgramData + GUI settings)"
    $dataRemoved = Remove-ProgramDataRayo
    if (Test-Path $guiSettingsPath) {
        Remove-PathWithRetry -Path $guiSettingsPath | Out-Null
    }
    if (Test-Path $guiSettingsDir) {
        try {
            $remaining = Get-ChildItem -Path $guiSettingsDir -Force -ErrorAction Stop
            if ($remaining.Count -eq 0) {
                Remove-Item -Path $guiSettingsDir -Force
            }
        } catch {
            Write-Warning "Could not clean settings directory '$guiSettingsDir': $($_.Exception.Message)"
        }
    }
}

if ($RestartPowerToys -and $powerToysWasRunning -and $powerToysExe -and (Test-Path $powerToysExe)) {
    Start-Process $powerToysExe | Out-Null
    Write-Host "PowerToys restarted."
}

if ($RemoveData) {
    Write-Host "Done. Plugin and service were removed. Data files were also removed."
} else {
    Write-Host "Done. Plugin and service were removed. Data files were kept."
    Write-Host "Use -RemoveData `$true if you also want to delete indices and logs."
}

Write-Host "Summary:"
Write-Host " - Scheduled task removed: $taskRemoved"
Write-Host " - Explorer shell integration removed: $shellRemoved"
Write-Host " - Plugin directory removed: $pluginRemoved"
Write-Host " - Local Rayo binaries removed: $binariesRemoved"
Write-Host " - Start menu shortcut removed: $shortcutRemoved"
if ($RemoveData) {
    Write-Host " - ProgramData removed: $dataRemoved"
}
