# Clippet — Implementation Plan

A native Windows 11 clipboard manager built incrementally in Rust + `windows-rs`,
with zero runtime dependencies. Single `.exe`, no installer.

## Targets

- Language: Rust (latest stable)
- Win32 bindings: `windows` crate (microsoft/windows-rs), feature-gated
- Subsystem: `#![windows_subsystem = "windows"]` (no console)
- Memory: < 8 MB RAM at idle
- Storage: `%APPDATA%\Clippet\history.json`
- Hotkey: `Win + V`
- Output: single `.exe`, no Electron / Tauri / Qt / WPF / web view

## Levels

Each level lands as one cohesive change set. Build with `cargo build --release`
between levels — the project must compile cleanly at every checkpoint.

| #  | Title                                                          | Status      |
|----|----------------------------------------------------------------|-------------|
| 1  | [Clipboard viewer window + listener](docs/level-1-clipboard-listener.md) | Done        |
| 2  | [Global hotkey + popup window](docs/level-2-popup-hotkey.md)   | Done        |
| 3  | [Persistent storage in AppData](docs/level-3-persistent-storage.md) | Done        |
| 4  | [Rich content support](docs/level-4-rich-content.md)           | Done        |
| 5  | [System tray icon + startup](docs/level-5-tray-icon.md)        | Done        |
| 6  | [Fuzzy search](docs/level-6-search.md)                         | Done        |
| 7  | [Pin / unpin + context menu](docs/level-7-pin-context-menu.md) | Done        |

## Cross-cutting rules

- All Win32 errors handled explicitly — no silent `unwrap()` on API calls.
- `OpenClipboard` always paired with `CloseClipboard`; `GlobalLock` with `GlobalUnlock`.
- File writes are atomic: write to `.tmp`, then rename.
- Win32 message loop runs on the main thread.
- Every `unsafe` block carries a one-line comment explaining why it is safe.
- No new top-level dependency added without justification in the level doc.
