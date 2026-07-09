# Rayo

Ultra-fast file search engine for Windows, written in Rust and inspired by Everything.

English | [Español](README.es.md)

## What Rayo does today (MVP)

- Enumerates NTFS MFT using `FSCTL_ENUM_USN_DATA`.
- Builds and persists a file index keyed by FRN.
- Reconstructs full paths by walking parent FRNs.
- Searches by substring with filters:
  - `--ext`
  - `--under`
  - `--glob`
  - `--dirs-only`
  - `--files-only`
  - `--limit`
- Applies live updates from USN Journal (`FSCTL_READ_USN_JOURNAL`).

## Project structure

- `crates/rayo-core`: indexing, search, NTFS/USN integration, persistence.
- `crates/rayo-cli`: CLI interface (`index`, `search`, `content`, `watch`).
- `crates/rayo-service`: elevated background service with live in-memory index and named pipe API.
- `crates/rayo-gui`: native desktop GUI (`Slint`, Fluent style) with service/fallback search modes.

## Requirements

- Windows (NTFS volume).
- Rust toolchain (`cargo`).
- Administrator privileges for `index` and `watch` (needed to read MFT/USN).

## Quick start

```powershell
# Build
cargo build

# Create index (run terminal as Administrator)
# Single drive:
cargo run -p rayo-cli -- index --drive C --output .\c.rayo
# Multi-drive (generates c.rayo, d.rayo from base output path):
cargo run -p rayo-cli -- index --drive C,D --output .\c.rayo

# Search
cargo run -p rayo-cli -- search --index .\c.rayo --query report --ext pdf

# Content search (regex, ripgrep-style)
cargo run -p rayo-cli -- content --query "Rayo GUI search client" --under . --limit 20

# Keep index updated (run terminal as Administrator)
cargo run -p rayo-cli -- watch --drive C --index .\c.rayo

# Start background service (run terminal as Administrator)
# Single drive:
cargo run -p rayo-service -- --drive C --index .\c.rayo
# Multi-drive merge:
cargo run -p rayo-service -- --drives C,D --index .\c.rayo

# Open GUI (tries service first, falls back to local index file)
cargo run -p rayo-gui -- --index .\c.rayo

# Optional: install Explorer context menus for file/folder/background
cargo run -p rayo-cli -- shell install --gui-path .\target\debug\rayo-gui.exe

# Diagnose shell integration
cargo run -p rayo-cli -- shell doctor --gui-path .\target\debug\rayo-gui.exe
```

### GUI actions

- Select a row, then use action buttons: `Open`, `Open as admin`, `Open folder`, `Copy path`.
- Built-in Settings panel lets you tune scope, extension, mode, result limit, and debounce.
- Keyboard shortcuts: `Ctrl+,` opens Settings and `Esc` closes Settings.
- Empty or 1-character queries do not run full search unless `--under` is set.

### Contextual GUI launch flags

- `--under <path>`: open GUI scoped to a folder (used by Explorer directory actions).
- `--query <text>`: prefill the search box.
- `--open <path>`: derive context from a file/folder path for right-click workflows.

### Optional trigram mode

For long queries, trigram mode can reduce first-search latency dramatically:

```powershell
# CLI one-off
cargo run --release -p rayo-cli -- search --index .\c.rayo --query tickettrack --trigram

# Service-wide mode (for clients through named pipe, including multi-drive)
cargo run -p rayo-service -- --drives C,D --index .\c.rayo --trigram --metrics-interval-secs 30
```

Tradeoff: trigram index uses more memory, but improves long/rare query latency.

## Validation results (Windows 11, C:, Jul 2026)

Real-world validation on NTFS `C:` with elevated terminal:

- Index file size: ~`365 MB`.
- Entries loaded: ~`6.2M`.

Search latency samples on real index (release):

- `--query report --limit 20`: `20` results in `6.673 ms`.
- `--query report --limit 20 --trigram`: `20` results in `6.644 ms`.
- `--query tickettrack --limit 20`: `1` result in `7.685 ms`.
- `--query tickettrack --limit 20 --trigram`: `1` result in `0.502 ms`.
- `--query zzzqqxxnotfound --limit 20`: `0` results in `7.321 ms`.
- `--query zzzqqxxnotfound --limit 20 --trigram`: `0` results in `0.026 ms`.

Watch validation covered file create/rename/delete events.

Service + integration validation:

- `rayo-service` started elevated with existing index and exposed `\\.\pipe\rayo-query`.
- Non-elevated client query over named pipe returned JSON results successfully.
- `rayo-cli shell install`, `shell doctor`, and `shell uninstall` validated file/folder/background Explorer integration in `HKCU\Software\Classes`.

## Roadmap

### Next

- Syntax-aware queries using `tree-sitter`.
- Bring content search into service and GUI workflows.

### Phase 3

- Keep polishing the native Fluent GUI (context menu, keyboard shortcuts, shell actions).
- Service-first architecture:
  - background index/watch service,
  - IPC for query clients (named pipes),
  - GUI and Windows integrations as thin clients.
- Potential integrations:
  - PowerToys Run plugin,
  - Explorer context action ("Search with Rayo here").

## CI and release packaging

- CI pipeline: [`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs `fmt`, `test`, Windows release builds, and a non-blocking .NET build for the PowerToys plugin scaffold.
- Windows release helper: [`scripts/release-windows.ps1`](scripts/release-windows.ps1)

```powershell
pwsh .\scripts\release-windows.ps1
```

This generates `dist/rayo-windows.zip` with `rayo-cli.exe`, `rayo-service.exe`, `rayo-gui.exe`, and docs.

## PowerToys Run plugin

- Plugin project: [`integrations/powertoys-run`](integrations/powertoys-run)
- Action keyword: `ry`
- Runtime dependency: `rayo-service` running as Administrator (`\\.\pipe\rayo-query`)

### Build and install manually

```powershell
dotnet build .\integrations\powertoys-run\Community.PowerToys.Run.Plugin.Rayo.csproj -c Release
dotnet publish .\integrations\powertoys-run\Community.PowerToys.Run.Plugin.Rayo.csproj -c Release -o .\dist\powertoys-run\RayoPlugin
```

Copy plugin output to:

`%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Plugins\Rayo\`

Then restart PowerToys and search with:

`ry <query>`

### Run as background service (recommended)

Use the new scheduled-task mode so Rayo runs without a visible console window:

```powershell
rayo-cli service install --service-exe "$env:LOCALAPPDATA\Rayo\rayo-service.exe" --drives C
rayo-cli service status
rayo-cli service uninstall
```

Defaults used by background mode:

- Index files: `%ProgramData%\Rayo\<drive>.rayo`
- Service log: `%ProgramData%\Rayo\service.log`

### Dependency-aware installer

One-command install from latest GitHub Release:

```powershell
irm https://raw.githubusercontent.com/waar19/rayo/main/scripts/install-powertoys-plugin.ps1 | iex
```

Local install with explicit zip:

```powershell
pwsh .\scripts\install-powertoys-plugin.ps1 -PluginZipPath .\dist\powertoys-run\RayoPlugin.zip -AutoInstallDependencies -RestartPowerToys
```

What it does:
- Detects PowerToys.
- Installs plugin to `%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Plugins\Rayo\`.
- Installs `rayo-service.exe`, `rayo-cli.exe`, and `rayo-gui.exe` to `%LOCALAPPDATA%\Rayo\`.
- Creates Start menu shortcut `Rayo` for launching the GUI.
- Registers/starts scheduled task `Rayo Service` for true background startup.
- Supports `RAYO_SERVICE_PATH` as override for custom service location.

### View indexing status

- Service log (live): `%ProgramData%\Rayo\service.log`
- PowerToys plugin shows startup/indexing progress while service warms up.
- GUI status bar shows source and indexed entries (for example: `service | indexed=...`).

```powershell
Get-Content C:\ProgramData\Rayo\service.log -Tail 20 -Wait
```

### Uninstall plugin and service

One-command uninstall:

```powershell
irm https://raw.githubusercontent.com/waar19/rayo/main/scripts/uninstall-powertoys-plugin.ps1 | iex
```

By default, uninstall keeps existing indexes/logs in `%ProgramData%\Rayo`.
To remove all data too:

```powershell
pwsh .\scripts\uninstall-powertoys-plugin.ps1 -RemoveData $true
```

### Release assets

- Tag-based release workflow publishes:
  - `rayo-windows.zip`
  - `RayoPlugin.zip`
- Installer downloads `RayoPlugin.zip` from latest release automatically when `-PluginZipPath` is omitted.

### Troubleshooting PowerToys plugin init errors

If PowerToys shows plugin initialization errors for Rayo:

1. Make sure you are on the latest release (`v0.1.7` or newer).
2. Reinstall plugin:
   ```powershell
   irm https://raw.githubusercontent.com/waar19/rayo/main/scripts/install-powertoys-plugin.ps1 | iex
   ```
3. If it still fails, check logs:
   `%LOCALAPPDATA%\Microsoft\PowerToys\PowerToys Run\Logs\<version>\<date>.txt`
4. Search for:
   `Can't find class implement IPlugin` or `System.Runtime` load errors.

5. If logs mention `IPlugin` type mismatch, your plugin package likely bundled host DLLs (`Wox.Plugin.dll` / `PowerToys.*.dll`). Reinstall from latest release.

## License

[MIT](LICENSE)
