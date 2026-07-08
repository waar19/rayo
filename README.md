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

## PowerToys Run scaffold

- Scaffold location: [`integrations/powertoys-run`](integrations/powertoys-run)
- Project: `Community.PowerToys.Run.Plugin.Rayo`
- Current status: named pipe client + DTOs compiled in CI; PowerToys interface wiring is the next step.

## License

[MIT](LICENSE)
