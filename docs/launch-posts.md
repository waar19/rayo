# Rayo Launch Post Drafts

## Reddit draft (`r/rust`, `r/windows`)

Title:
- Show HN / Launch: Rayo, ultra-fast Windows file search in Rust (CLI + GUI + PowerToys)

Body:

Rayo is an open-source file search engine for Windows, inspired by Everything and built in Rust.

What ships today:
- Fast file/folder search with filters (`under`, `ext`, `exclude`, `glob`, `files-only`, `dirs-only`).
- Background service (`rayo-service`) with named-pipe API.
- Native GUI (`Slint`) with context actions and preview panel.
- PowerToys Run plugin integration.
- Winget package: `winget install waar19.Rayo`.

Why I built it:
- Wanted deterministic, scriptable search workflow.
- Wanted native Windows integration (service + PowerToys + installer).
- Wanted open-source base for future features (content/syntax workflows).

Repo:
- https://github.com/waar19/rayo

Feedback welcome:
- Performance edge cases.
- UX polish priorities for next release.
- Packaging/signing expectations for Windows users.

## Hacker News draft (`Show HN`)

Title:
- Show HN: Rayo — ultra-fast Windows file search engine in Rust

Body:

Hi HN,

I built Rayo, an open-source Windows search tool in Rust inspired by Everything.

Core pieces:
- `rayo-core`: index/search engine.
- `rayo-service`: background service with named-pipe API.
- `rayo-gui`: native Slint desktop client.
- PowerToys Run plugin for launcher workflow.

Install:
- `winget install waar19.Rayo`

Project:
- https://github.com/waar19/rayo

I would appreciate feedback on:
- Search latency behavior at large scale.
- Preferred update flow for Windows desktop tools.
- Tradeoffs users care about vs Everything/Windows Search.
