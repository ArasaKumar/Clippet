<div align="center">

<img src="assets/clippet.svg" alt="Clippet" width="120" height="120" />

# Clippet

**The Windows 11 clipboard manager that respects your machine.**

A native, single-binary clipboard history utility for Windows 11. Built in Rust on the Win32 API directly — no Electron, no web view, no background services, no telemetry. Under 8 MB of RAM at idle. Your clipboard never leaves your computer.

[![Platform: Windows 11](https://img.shields.io/badge/platform-Windows%2011-0078D4)](#requirements)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-CE422B)](https://rust-lang.org)
[![Single binary](https://img.shields.io/badge/distribution-single%20.exe-success)](#install)
[![License: Unlicense](https://img.shields.io/badge/license-Unlicense-lightgrey)](LICENSE)
[![No telemetry](https://img.shields.io/badge/telemetry-none-brightgreen)](#privacy)

[Install](#install) · [Features](#features) · [Shortcuts](#shortcuts) · [How it works](#how-it-works) · [FAQ](#faq)

</div>

---

## Why Clippet

Windows 11's built-in clipboard history (`Win+V`) is fine — until you need it to remember more than a few items, survive a reboot, paste an image with the same fidelity Word saved it with, or stay out of the cloud. Clippet does all of that in a single executable you can drop anywhere.

| | Clippet | Win+V (built-in) | Electron-based managers |
|---|:---:|:---:|:---:|
| Native Win32 — no Chromium runtime | ✅ | ✅ | ❌ |
| Persistent history across reboots | ✅ | Partial | ✅ |
| Rich content (RTF, HTML, images, files) | ✅ | Partial | Varies |
| Pin items past the history cap | ✅ | ❌ | Varies |
| Fuzzy search across history | ✅ | ❌ | Varies |
| Idle memory footprint | < 8 MB | n/a | 100 – 400 MB |
| Distribution | One `.exe` | OS-bundled | Installer + updater |
| Network access | None | Cloud sync optional | Often required |

## Features

### 📋 Captures the format that matters

Clippet inspects every clipboard write and stores the most informative format available — not a flattened text version of it.

| Tag | Type | What's preserved |
|---|---|---|
| `[T]` | Plain text | UTF-8 / UTF-16 |
| `[R]` | Rich text | Original RTF — paste keeps fonts and styling |
| `[I]` | Image | Re-encoded as PNG; thumbnail shown inline in the popup; round-trips back to DIB on paste |
| `[F]` | Files / folders | Real `CF_HDROP` payload — paste into Explorer just works |
| `[H]` | HTML | Full `CF_HTML` fragment with source URL |
| `[X]` | Spreadsheet | Excel/Sheets table cells with structure intact |
| `[C:lang]` | Code | Auto-tagged with a language hint when copied from a known IDE |

### ⌨️ Lives where you do — `Ctrl + Shift + V`

A global hotkey summons the popup at your cursor. Pick an item with arrow keys + `Enter`, or click. Clippet restores focus to whatever window was active and synthesizes a real `Ctrl+V` — the paste lands exactly where it would have.

### 🔍 Fuzzy search built in

Just start typing. Clippet runs a Skim-style fuzzy match across every item's preview, highlights the matched characters in bold, and reorders the list by score. Pinned items still float to the top.

### 📌 Pin anything important

Star an item to pin it. Pinned entries sort above the rest, survive the 200-item history cap forever, and stay across restarts. Right-click any row for **Paste / Pin / Copy / Delete**, or use `Ctrl+P` from the keyboard.

### 🌓 Looks like Windows 11, because it is

- Acrylic backdrop and rounded corners via DWM (`DWMSBT_TRANSIENTWINDOW`, `DWMWCP_ROUND`)
- Custom flat title bar that matches the system clipboard panel
- Compact icon footer for quick actions (Clear History, Settings, About, Quit) with hover tooltips and per-action accent colours
- Per-monitor DPI v2 — sharp on mixed-DPI setups
- Light / dark palette tracked from `AppsUseLightTheme`
- Segoe UI Variable for body, semibold heading on the title bar

### 🔋 System tray + optional autostart

A tray icon keeps Clippet one click away. **Open / Clear history / Settings / Exit** from the right-click menu. On first launch you're asked once whether to start with Windows; the answer is remembered and writable through a single `HKCU\...\Run` value (no scheduled tasks, no service install).

### 🧱 Resilient by design

- Atomic disk writes (`history.json.tmp` → rename) — no half-written files if power dies mid-save
- Every Win32 call has paired cleanup (`OpenClipboard`/`CloseClipboard`, `GlobalLock`/`GlobalUnlock`)
- Pinned items are exempt from history overflow eviction
- Final flush on `WM_DESTROY` so in-session pins survive a force-close

## Install

### Build from source

Clippet builds with the standard Rust toolchain. No Visual Studio project files, no MSBuild — just `cargo`.

#### Requirements

- Windows 10 (build 1903+) or Windows 11
- Rust toolchain — install via [rustup](https://rustup.rs/)
- MSVC target: `x86_64-pc-windows-msvc` (rustup picks this by default on Windows)

#### Build

```powershell
git clone https://github.com/ArasaKumar/Clippet.git
cd Clippet
cargo build --release
```

The binary lands at `target\release\clippet.exe`. It's self-contained — copy it anywhere and run it.

#### Run

```powershell
.\target\release\clippet.exe
```

Clippet starts hidden in the system tray. Press `Ctrl + Shift + V` to summon the popup, or click the tray icon.

## Shortcuts

| Shortcut | Action |
|---|---|
| `Ctrl + Shift + V` | Show / hide the Clippet popup |
| `↑` / `↓` | Navigate items |
| `Tab` | Toggle focus between search box and list |
| `Enter` | Paste selected item into the previous window |
| `Ctrl + P` | Pin / unpin the selected row |
| `Esc` | Hide the popup |
| `Right-click row` | Context menu: Paste, Pin/Unpin, Copy, Delete |

## How it works

```
                  ┌─────────────────────────────────────────┐
                  │         clippet.exe (single binary)     │
                  ├─────────────────────────────────────────┤
   WM_HOTKEY  ◄───┤  Win32 message loop  (main thread)      │
   WM_CLIP-   ◄───┤                                         │
   BOARDUPDATE    │  ┌─────────────┐  ┌──────────────────┐  │
                  │  │ clipboard   │  │ paste            │  │
                  │  │  capture    │──▶ SetClipboardData │  │
                  │  │  pipeline   │  │ + SendInput Ctrl+V│ │
                  │  └─────────────┘  └──────────────────┘  │
                  │         │                  ▲            │
                  │         ▼                  │            │
                  │  ┌─────────────┐  ┌──────────────────┐  │
                  │  │ search +    │  │ owner-draw       │  │
                  │  │ fuzzy match │──▶ listbox + tray   │  │
                  │  └─────────────┘  └──────────────────┘  │
                  │         │                               │
                  │         ▼                               │
                  │  ┌────────────────────────────────────┐ │
                  │  │ atomic JSON @ %APPDATA%\Clippet\   │ │
                  │  └────────────────────────────────────┘ │
                  └─────────────────────────────────────────┘
```

- Single-threaded by design. The Win32 message loop owns everything; there is no async runtime, no worker pool, no `Mutex`.
- Capture priority on each `WM_CLIPBOARDUPDATE`: **Files → Spreadsheet → RTF → HTML → Image → Unicode text → ANSI text** — Clippet keeps the richest available format.
- DIB images are re-encoded to PNG for storage and converted back to DIB on paste, so applications receive exactly what they expect.
- The popup is a `WS_POPUP | WS_THICKFRAME` window with the native caption suppressed; `WM_NCHITTEST` maps the outer 6 px to resize cursors and the top strip to a draggable region.

## Configuration & data

Clippet stores everything under your roaming AppData folder:

| Path | Purpose |
|---|---|
| `%APPDATA%\Clippet\history.json` | Clipboard history (200-item cap; pins exempt) |
| `%APPDATA%\Clippet\settings.json` | Last-used popup size; autostart prompt state |
| `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` | Optional autostart entry (only written if you opt in) |

To wipe history, use **Clear history** in the tray menu, or delete `history.json` while Clippet isn't running.

## Privacy

Clippet has no network code. There is no analytics SDK, no auto-update check, no remote logging, no cloud sync. Every byte of your clipboard data lives on your disk and nowhere else. Read [src/storage.rs](src/storage.rs) — it's a few hundred lines of plain file I/O.

## Roadmap

All seven planned levels are implemented and shipping in the current build.

| # | Feature | Status |
|---|---|---|
| 1 | Clipboard viewer window + listener | ✅ Done |
| 2 | Global hotkey + popup window | ✅ Done |
| 3 | Persistent storage in AppData | ✅ Done |
| 4 | Rich content support (RTF / HTML / images / files / sheets) | ✅ Done |
| 5 | System tray icon + startup integration | ✅ Done |
| 6 | Fuzzy search | ✅ Done |
| 7 | Pin / unpin + per-row context menu | ✅ Done |

Per-level design notes are under [docs/](docs/); the master roadmap is in [PLAN.md](PLAN.md).

## FAQ

**Why Ctrl+Shift+V instead of Win+V?**
The Windows 11 shell holds `Win+V` even when clipboard history is disabled in Settings, so we can't reliably register it. `Ctrl+Shift+V` is unclaimed in stock Windows and free across most apps.

**Does Clippet conflict with Windows' own clipboard history?**
No. They run independently. If you don't want both, disable the built-in one under *Settings → System → Clipboard*.

**Where are images stored?**
Re-encoded to PNG and base64-embedded inside `history.json`. A thumbnail is rendered inline in the popup so you can identify the image before pasting. The 200-item cap keeps the file from growing unbounded; pinned images are exempt, so be mindful when pinning very large screenshots.

**Why Rust + raw Win32 instead of a framework?**
The whole point is the single sub-MB binary and the < 8 MB idle footprint. A framework would add tens of MB of runtime for features Clippet doesn't need.

**Can I trust this with sensitive data?**
The same trust you give any local app that reads your clipboard. Clippet writes to a per-user AppData file that other users on the machine can't read. Source is public domain — audit it.

## Contributing

Issues and pull requests are welcome. Before opening a PR:

1. The project must build cleanly with `cargo build --release` at every commit.
2. Match the existing style — pair every Win32 resource with its cleanup, justify every `unsafe` with a `// SAFETY:` comment, and keep `main.rs` focused on dispatch only (implementation belongs in the per-feature module).
3. Larger features should follow the level-doc pattern — a short design note in `docs/` paired with the change.

## License

Released into the public domain under the [Unlicense](LICENSE). Do whatever you want with it.
