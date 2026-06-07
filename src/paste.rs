//! Clipboard write + Ctrl+V synthesis.
//!
//! `set_clipboard_from_item` re-publishes a stored ClipItem in its
//! native format. `activate_selected` is the high-level "paste the
//! current row" entry point invoked from Enter/double-click/menu.

use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clipboard::{looks_like_png, png_to_dib};
use crate::state::{
    ClipItem, ItemType, CF_DIB, CF_HDROP, CF_TEXT, CF_UNICODETEXT, FILTERED, HISTORY, LISTBOX,
    PREV_FG, REG, SUPPRESS_NEXT_UPDATE,
};
use crate::storage::media_path;
use crate::util::debug_log;

/// Allocate a movable global block, copy `bytes` into it, and return
/// the handle. On success the caller hands ownership to the clipboard
/// via `SetClipboardData`.
///
/// SAFETY: GlobalAlloc/Lock/Unlock pair is honored on every exit path.
pub(crate) unsafe fn alloc_global(bytes: &[u8]) -> Option<HGLOBAL> {
    let hg = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let p = GlobalLock(hg);
    if p.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    let _ = GlobalUnlock(hg);
    Some(hg)
}

/// Build the CF_HDROP payload: DROPFILES header followed by
/// null-terminated UTF-16 file paths and a final extra null.
///
/// SAFETY: All pointer arithmetic stays within `buf`, which is sized
/// to fit the header + the wide-list bytes exactly.
pub(crate) unsafe fn build_hdrop_blob(paths: &[&str]) -> Vec<u8> {
    let header_size = std::mem::size_of::<DROPFILES>();
    let mut wide_list: Vec<u16> = Vec::new();
    for p in paths {
        wide_list.extend(p.encode_utf16());
        wide_list.push(0);
    }
    wide_list.push(0); // final terminator

    let total = header_size + wide_list.len() * 2;
    let mut buf = vec![0u8; total];
    let header = DROPFILES {
        pFiles: header_size as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: BOOL(0),
        fWide: BOOL(1),
    };
    std::ptr::copy_nonoverlapping(
        &header as *const _ as *const u8,
        buf.as_mut_ptr(),
        header_size,
    );
    let wide_bytes =
        std::slice::from_raw_parts(wide_list.as_ptr() as *const u8, wide_list.len() * 2);
    buf[header_size..].copy_from_slice(wide_bytes);
    buf
}

/// Wrapper around SetClipboardData that returns Some on success.
/// SetClipboardData takes ownership of the HGLOBAL on success; on
/// failure we'd leak it — acceptable here because the failure path is
/// rare and the process dies on it anyway.
///
/// SAFETY: caller must hold the clipboard open.
unsafe fn set_data(format: u32, hg: HGLOBAL) -> Option<()> {
    let handle = HANDLE(hg.0);
    SetClipboardData(format, handle).ok().map(|_| ())
}

/// Re-publish the item to the clipboard in its native format. Sets
/// `SUPPRESS_NEXT_UPDATE` so our own listener ignores the resulting
/// notification.
///
/// SAFETY: OpenClipboard is paired with CloseClipboard on every exit;
/// global-handle ownership is transferred to Win32 by SetClipboardData.
pub(crate) unsafe fn set_clipboard_from_item(hwnd: HWND, item: &ClipItem) -> bool {
    if OpenClipboard(hwnd).is_err() {
        return false;
    }
    let _ = EmptyClipboard();
    let reg = REG.with(|r| *r.borrow());

    let result = match item.kind {
        ItemType::Text | ItemType::Code => {
            // raw is UTF-8; clipboard CF_UNICODETEXT wants null-terminated UTF-16.
            let s = String::from_utf8_lossy(&item.raw);
            let wide: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes =
                std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
            alloc_global(bytes).and_then(|hg| set_data(CF_UNICODETEXT, hg))
        }
        ItemType::RichText => alloc_global(&item.raw).and_then(|hg| set_data(reg.rtf, hg)),
        ItemType::Html => alloc_global(&item.raw).and_then(|hg| set_data(reg.html, hg)),
        // Spreadsheet payload is TSV (or the raw XML fallback). Either
        // way, CF_TEXT is the broadly-pasteable format — Notepad,
        // Excel, web forms, terminals all take it.
        ItemType::Spreadsheet => alloc_global(&item.raw).and_then(|hg| set_data(CF_TEXT, hg)),
        ItemType::Image => {
            // Image bytes live on disk under media/{media_file}; read
            // them on demand. PNG is the common case; we still sniff
            // the signature so a raw-DIB fallback (rare-shape DIB the
            // capture path couldn't encode) routes correctly.
            let bytes = item
                .media_file
                .as_deref()
                .and_then(media_path)
                .and_then(|p| std::fs::read(&p).ok());
            match bytes {
                Some(b) => {
                    let dib = if looks_like_png(&b) {
                        png_to_dib(&b)
                    } else {
                        Some(b)
                    };
                    dib.and_then(|d| alloc_global(&d))
                        .and_then(|hg| set_data(CF_DIB, hg))
                }
                None => {
                    debug_log(&format!(
                        "Clippet: media missing for item id={} — paste skipped",
                        item.id
                    ));
                    None
                }
            }
        }
        ItemType::File => {
            let joined = String::from_utf8_lossy(&item.raw);
            let paths: Vec<&str> = joined.split('\n').filter(|s| !s.is_empty()).collect();
            if paths.is_empty() {
                None
            } else {
                let blob = build_hdrop_blob(&paths);
                alloc_global(&blob).and_then(|hg| set_data(CF_HDROP, hg))
            }
        }
    };

    if result.is_some() {
        SUPPRESS_NEXT_UPDATE.with(|f| f.set(true));
    }
    let _ = CloseClipboard();
    result.is_some()
}

/// Synthesize Ctrl+V into the foreground (now the previous app) input
/// queue.
///
/// SAFETY: SendInput is documented thread-safe; the input array lives
/// for the duration of the call.
pub(crate) unsafe fn send_paste() {
    let mut inputs: [INPUT; 4] = std::mem::zeroed();
    for i in inputs.iter_mut() {
        i.r#type = INPUT_KEYBOARD;
    }
    inputs[0].Anonymous.ki = KEYBDINPUT {
        wVk: VK_CONTROL,
        wScan: 0,
        dwFlags: KEYBD_EVENT_FLAGS(0),
        time: 0,
        dwExtraInfo: 0,
    };
    inputs[1].Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(b'V' as u16),
        wScan: 0,
        dwFlags: KEYBD_EVENT_FLAGS(0),
        time: 0,
        dwExtraInfo: 0,
    };
    inputs[2].Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(b'V' as u16),
        wScan: 0,
        dwFlags: KEYEVENTF_KEYUP,
        time: 0,
        dwExtraInfo: 0,
    };
    inputs[3].Anonymous.ki = KEYBDINPUT {
        wVk: VK_CONTROL,
        wScan: 0,
        dwFlags: KEYEVENTF_KEYUP,
        time: 0,
        dwExtraInfo: 0,
    };
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Activate the currently-selected listbox row (paste it into the
/// previous foreground window). Returns true if a row was activated.
///
/// SAFETY: the listbox handle is valid for the lifetime of the popup;
/// PREV_FG was tracked by WM_ACTIVATE.
pub(crate) unsafe fn activate_selected(hwnd: HWND) -> bool {
    let lb = LISTBOX.with(|l| *l.borrow());
    if lb.0.is_null() {
        return false;
    }
    let sel = crate::listbox::list_get_sel();
    if sel < 0 {
        return false;
    }
    // The listbox shows whatever update_filter() last published, so map
    // row index -> history index through FILTERED instead of assuming a
    // newest-first ordering.
    let hist_index = FILTERED
        .with(|f| f.borrow().get(sel as usize).map(|r| r.hist_index));
    let Some(hist_index) = hist_index else {
        return false;
    };
    let item_opt = HISTORY.with(|h| h.borrow().get(hist_index).cloned());
    let Some(item) = item_opt else {
        return false;
    };

    if !set_clipboard_from_item(hwnd, &item) {
        return false;
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
    let prev = PREV_FG.with(|c| c.get());
    if !prev.0.is_null() {
        let _ = SetForegroundWindow(prev);
    }
    send_paste();
    true
}
