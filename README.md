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
- `crates/rayo-cli`: CLI interface (`index`, `search`, `watch`).
- `crates/rayo-service`: elevated background service with live in-memory index and named pipe API.
- `crates/rayo-gui`: native desktop GUI (`egui`) with service/fallback search modes.

## Requirements

- Windows (NTFS volume).
- Rust toolchain (`cargo`).
- Administrator privileges for `index` and `watch` (needed to read MFT/USN).

## Quick start

```powershell
# Build
cargo build

# Create index (run terminal as Administrator)
cargo run -p rayo-cli -- index --drive C --output .\c.rayo

# Search
cargo run -p rayo-cli -- search --index .\c.rayo --query report --ext pdf

# Keep index updated (run terminal as Administrator)
cargo run -p rayo-cli -- watch --drive C --index .\c.rayo

# Start background service (run terminal as Administrator)
cargo run -p rayo-service -- --drive C --index .\c.rayo

# Open GUI (tries service first, falls back to local index file)
cargo run -p rayo-gui -- --index .\c.rayo

# Optional: install Explorer context menu for current user
cargo run -p rayo-cli -- shell install --gui-path .\target\debug\rayo-gui.exe
```

### GUI shortcuts

- `Enter`: open selected result.
- `Ctrl+Enter`: open selected result as Administrator (UAC prompt).
- Context menu on each row: open, open as admin, open containing folder, copy path.

## Validation results (Windows 11, C:, Jul 2026)

Real-world validation on NTFS `C:` with elevated terminal:

- Initial indexing to `c-base.rayo`: `INDEX_WALL_MS=133246` (~2m13s).
- Index file size: `364427087` bytes (~347.5 MiB).
- Entries loaded when watch started: `6192118`.

Search latency samples on real index:

- `--query report --limit 20`: `20` results in `2.4751112s` (wall-clock `15315 ms`).
- `--query report --ext pdf --limit 20`: `20` results in `1.9989417s` (wall-clock `17261 ms`).
- `--query system --under C:\Windows --limit 20`: `20` results in `2.7214587s` (wall-clock `18455 ms`).
- `--query kernel --glob "**/*.dll" --limit 20`: `20` results in `2.2629864s` (wall-clock `16657 ms`).

Watch validation covered file create/rename/delete events.

Service + integration validation:

- `rayo-service` started elevated with existing index and exposed `\\.\pipe\rayo-query`.
- Non-elevated client query over named pipe returned JSON results successfully.
- `rayo-cli shell install` and `shell uninstall` wrote and removed Explorer context menu entries in `HKCU\Software\Classes`.

## Roadmap

### Phase 2

- Content search (ripgrep-style) using `grep`/`ignore`.
- Syntax-aware queries using `tree-sitter`.

### Phase 3

- Native GUI (`egui` or `Slint`).
- Service-first architecture:
  - background index/watch service,
  - IPC for query clients (named pipes),
  - GUI and Windows integrations as thin clients.
- Potential integrations:
  - PowerToys Run plugin,
  - Explorer context action ("Search with Rayo here").

## License

[MIT](LICENSE)
