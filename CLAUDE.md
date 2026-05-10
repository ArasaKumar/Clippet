# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Clippet is a native Windows 11 clipboard manager: single `.exe`, no Electron / Tauri / Qt / web view, no installer, < 8 MB RAM at idle. Rust + `windows-rs` Win32 bindings only. The seven feature levels (clipboard listener → popup hotkey → persistence → rich content → tray → fuzzy search → pin/context-menu) are all complete; per-level design notes live in `docs/level-*.md` and the roadmap in `PLAN.md`.

The global hotkey is **Ctrl+Shift+V** (Win+V is held by the Win11 shell even with clipboard history disabled, so it was abandoned).

## Build & run

Requires the MSVC Rust toolchain (`x86_64-pc-windows-msvc`). MSRV is 1.73.

```powershell
cargo build --release           # produces target\release\clippet.exe
cargo build                     # debug build, faster iteration
cargo check                     # type-check without linking
.\target\release\clippet.exe    # run
```

There are no tests in the tree — `cargo test` is a no-op. The release profile is tuned for size (`opt-level = "z"`, LTO, `panic = "abort"`, strip).

`build.rs` procedurally rasterizes `assets/clippet.svg` into a multi-resolution `.ico` and embeds it as the `clippet` Win32 resource (matched at runtime by `LoadIconW(hinst, w!("clippet"))`). If you change the SVG, mirror the coordinate change in `build.rs::render_icon` — we don't parse the SVG to keep build deps lean.

## Architecture

**Single-threaded by design.** The Win32 message loop runs on the main thread; all state is `thread_local!` `Cell`/`RefCell` in [src/state.rs](src/state.rs). No `Mutex`, no async, no worker threads. The one atomic (`NEXT_ID`) only exists because it's initialized before the thread-locals are populated.

[src/main.rs](src/main.rs) is the orchestrator: it registers the window class, owns the `GetMessageW` loop, and dispatches each `WM_*` message to a focused module. Adding behavior usually means adding a `WM_*` arm in `wnd_proc` and the implementation in the matching module — don't grow `main.rs`.

Module layout:

| Module | Responsibility |
|---|---|
| [state.rs](src/state.rs) | All shared types, constants (palette, IDs, geometry, format codes), and the thread-local cells |
| [clipboard.rs](src/clipboard.rs) | `WM_CLIPBOARDUPDATE` capture pipeline. Picks the most informative format: Files > Spreadsheet > RTF > HTML > Image > UnicodeText > AnsiText. Includes the DIB ↔ PNG converter |
| [paste.rs](src/paste.rs) | Re-publish a stored item to the clipboard, then synthesize Ctrl+V via `SendInput` against the previously focused window (`PREV_FG`) |
| [storage.rs](src/storage.rs) | `%APPDATA%\Clippet\history.json` (atomic write via `.tmp` + rename), `settings.json`, and the HKCU `Run` autostart registry value |
| [search.rs](src/search.rs) | Fuzzy filter (`fuzzy-matcher` skim algorithm) + the `FILTERED` row→history index mapping that drives the listbox |
| [listbox.rs](src/listbox.rs) | Owner-draw listbox: per-row layout, bold matched-char runs, pin glyph hit-testing, listbox subclass for context menu |
| [titlebar.rs](src/titlebar.rs) | Custom title bar (Win11-style flat close button). The native caption is suppressed via `WM_NCCALCSIZE` returning 0 |
| [theme.rs](src/theme.rs) | Light/dark detection from `AppsUseLightTheme`, palette + DWM acrylic + rounded-corner setup |
| [tray.rs](src/tray.rs) | Tray icon, tray menu, popup show/hide/positioning, autostart prompt, popup-size persistence |
| [util.rs](src/util.rs) | Wide-string helpers, foreground-window IDE detection (drives the `[C]` code-language tag), relative-time formatting |

**Critical Win32 invariants** (from PLAN.md cross-cutting rules — preserve when editing):

- `OpenClipboard` is always paired with `CloseClipboard` on every exit path; same for `GlobalLock` / `GlobalUnlock`.
- File writes are atomic: write to `.tmp`, then rename.
- Every `unsafe` block carries a one-line `// SAFETY:` comment explaining why it's sound.
- No silent `unwrap()` on Win32 API calls — surface the error or use `let _ =` to mark deliberate ignores.
- Adding a top-level dependency requires justification in the relevant level doc.

**Re-entrancy gotcha:** when this app calls `SetClipboardData` itself (during a paste), the `WM_CLIPBOARDUPDATE` it triggers must be ignored or it would re-capture the just-pasted item. `paste.rs` sets `SUPPRESS_NEXT_UPDATE` before publishing; `main.rs::WM_CLIPBOARDUPDATE` reads-and-clears it.

**Window chrome:** `WS_POPUP | WS_THICKFRAME` with `WS_EX_TOOLWINDOW` (no taskbar / Alt+Tab). `WM_NCCALCSIZE` zeroes the non-client area, `WM_NCPAINT` swallows the gray frame, `WM_NCHITTEST` maps the outer 6 px (`RESIZE_MARGIN`) to resize codes and the top `TITLEBAR_HEIGHT` strip to `HTCAPTION`. DWM still paints the rounded corners + acrylic backdrop applied in `theme::apply_popup_style`.
