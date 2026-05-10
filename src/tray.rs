//! Tray icon, tray menu, tooltip, popup show/hide, autostart prompt,
//! and the small dialog helpers (clear/settings stub) that live around
//! the popup chrome.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::listbox::clear_thumb_cache;
use crate::search::{pinned_count, refresh_listbox};
use crate::state::{
    HISTORY, IDM_CLEAR, IDM_EXIT, IDM_OPEN, IDM_SETTINGS, LISTBOX, POPUP_SIZE, PREV_FG, SEARCH,
    TRAY_ICON_ID, WM_APP_TRAY,
};
use crate::storage::{
    current_exe_quoted, load_settings, registry_run_set, save_history, save_settings,
};
use crate::util::{show_msgbox, to_wide};

// =====================================================================
// Icon + tray.
// =====================================================================

/// Load the embedded "clippet" icon resource at the requested size.
/// Falls back to the system IDI_APPLICATION icon if the resource is
/// missing (build.rs failed) or LoadImageW errors.
///
/// SAFETY: returned HICON is owned by the system (LR_SHARED) — callers
/// must not free it.
pub(crate) unsafe fn load_app_icon(width: i32, height: i32) -> HICON {
    let hinst = match GetModuleHandleW(None) {
        Ok(m) => HINSTANCE(m.0),
        Err(_) => return LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
    };
    if let Ok(handle) = LoadImageW(
        hinst,
        w!("clippet"),
        IMAGE_ICON,
        width,
        height,
        LR_DEFAULTCOLOR | LR_SHARED,
    ) {
        return HICON(handle.0);
    }
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

/// Add the tray icon. Tray prefers SM_CXSMICON-sized icons; LoadImageW
/// with that size picks the best-fitting variant out of the multi-res
/// .ico embedded by build.rs.
///
/// SAFETY: NOTIFYICONDATAW is a stack-allocated POD; szTip is
/// zero-initialised so the trailing null is in place.
pub(crate) unsafe fn add_tray_icon(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ICON_ID;
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_APP_TRAY;
    let small = GetSystemMetrics(SM_CXSMICON);
    nid.hIcon = load_app_icon(small, small);

    let tip: Vec<u16> = "Clippet".encode_utf16().collect();
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);

    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
}

/// SAFETY: same NOTIFYICONDATAW contract as `add_tray_icon`.
pub(crate) unsafe fn remove_tray_icon(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ICON_ID;
    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
}

/// Refresh the tray tooltip with the current pinned count. Called
/// after any pin toggle or history mutation that changes the count.
///
/// SAFETY: see `add_tray_icon`.
pub(crate) unsafe fn update_tray_tooltip(hwnd: HWND) {
    let count = pinned_count();
    let label = if count > 0 {
        format!("Clippet \u{2014} {} pinned", count)
    } else {
        "Clippet".to_string()
    };
    let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
    nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_ICON_ID;
    nid.uFlags = NIF_TIP;
    let tip: Vec<u16> = label.encode_utf16().collect();
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

// =====================================================================
// Popup show/hide and positioning.
// =====================================================================

/// SAFETY: hwnd is the popup window we own.
pub(crate) unsafe fn toggle_popup(hwnd: HWND) {
    if IsWindowVisible(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_HIDE);
    } else {
        show_popup(hwnd);
    }
}

/// Place the popup near the cursor, clamped to the work area of the
/// nearest monitor. Uses the user-resized size (or the default) so each
/// summon preserves whatever dimensions the window was last left at.
///
/// SAFETY: GetCursorPos / MonitorFromPoint / GetMonitorInfoW /
/// SetWindowPos are documented thread-safe Win32 APIs.
unsafe fn position_popup_at_cursor(hwnd: HWND) {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        mi.rcWork
    } else {
        RECT { left: 0, top: 0, right: 1920, bottom: 1080 }
    };

    let (w, h) = POPUP_SIZE.with(|c| c.get());
    let mut x = pt.x;
    let mut y = pt.y;
    if x + w > work.right {
        x = work.right - w;
    }
    if y + h > work.bottom {
        y = work.bottom - h;
    }
    if x < work.left {
        x = work.left;
    }
    if y < work.top {
        y = work.top;
    }
    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
}

/// Push the in-memory popup size back to settings.json so it survives
/// a restart. Called when the window is hidden — that's the natural
/// save point and avoids hammering the disk on every WM_SIZE.
pub(crate) fn persist_popup_size() {
    let (w, h) = POPUP_SIZE.with(|c| c.get());
    let mut s = load_settings();
    s.popup_w = Some(w);
    s.popup_h = Some(h);
    save_settings(&s);
}

/// Show the popup at the cursor, capture the window we should paste
/// back into, clear the search box, and focus the edit control.
///
/// SAFETY: every Win32 call is invoked on the UI thread with handles
/// owned by this process.
pub(crate) unsafe fn show_popup(hwnd: HWND) {
    let fg = GetForegroundWindow();
    if fg.0 != hwnd.0 {
        PREV_FG.with(|c| c.set(fg));
    }
    position_popup_at_cursor(hwnd);

    // Clear the search box on every show so the user starts with the
    // full list. SetWindowTextW will fire EN_CHANGE which calls
    // update_filter; we then call it explicitly to cover the "already
    // empty" case.
    let edit = SEARCH.with(|c| c.get());
    if !edit.0.is_null() {
        let _ = SetWindowTextW(edit, w!(""));
    }
    crate::search::update_filter();

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);

    // Focus the search box — typing should filter immediately. The
    // listbox already has its first row selected from update_filter,
    // so Enter pastes without an extra arrow keypress.
    if !edit.0.is_null() {
        let _ = SetFocus(edit);
    } else {
        let lb = LISTBOX.with(|l| *l.borrow());
        if !lb.0.is_null() {
            let _ = SetFocus(lb);
        }
    }
}

// =====================================================================
// Tray menu commands.
// =====================================================================

/// SAFETY: see `show_row_context_menu` for the same TrackPopupMenu
/// pattern. `SetForegroundWindow` precedes the menu so the menu
/// dismisses correctly on first-click-elsewhere.
pub(crate) unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let s_open = to_wide("Open Clippet");
    let s_clear = to_wide("Clear History");
    let s_settings = to_wide("Settings...");
    let s_exit = to_wide("Exit");
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN as usize, PCWSTR(s_open.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_CLEAR as usize, PCWSTR(s_clear.as_ptr()));
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        IDM_SETTINGS as usize,
        PCWSTR(s_settings.as_ptr()),
    );
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, PCWSTR(s_exit.as_ptr()));

    // Per Microsoft's TrackPopupMenu docs: the owner window must be the
    // foreground window or the menu won't dismiss properly when the
    // user clicks elsewhere; the trailing PostMessage(WM_NULL) forces
    // the first-click-after-dismiss behavior to feel right.
    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, p.x, p.y, 0, hwnd, None);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

/// SAFETY: invokes show_msgbox (own MessageBoxW wrapper) and reads/
/// writes HISTORY on the UI thread.
pub(crate) unsafe fn clear_history(hwnd: HWND) {
    let r = show_msgbox(
        hwnd,
        "Clippet",
        "Clear all clipboard history?\n\nThis cannot be undone.",
        MB_YESNO | MB_ICONQUESTION,
    );
    if r == IDYES {
        HISTORY.with(|h| h.borrow_mut().clear());
        clear_thumb_cache();
        save_history(&[]);
        refresh_listbox();
        update_tray_tooltip(hwnd);
    }
}

/// SAFETY: shows a MessageBoxW; no other state is touched.
pub(crate) unsafe fn show_settings_stub(hwnd: HWND) {
    show_msgbox(
        hwnd,
        "Clippet",
        "Settings UI coming in a later level.",
        MB_OK | MB_ICONINFORMATION,
    );
}

/// SAFETY: shows a MessageBoxW; no other state is touched.
pub(crate) unsafe fn show_about(hwnd: HWND) {
    show_msgbox(
        hwnd,
        "About Clippet",
        "Clippet v0.1.0\n\n\
         Native Windows 11 clipboard manager.\n\
         Built with Rust + windows-rs.\n\n\
         Global hotkey: Ctrl+Shift+V",
        MB_OK | MB_ICONINFORMATION,
    );
}

// =====================================================================
// Autostart prompt (first launch only).
// =====================================================================

/// On first launch, ask the user whether to autostart Clippet with
/// Windows. The choice (and the prompted flag) is persisted in
/// settings.json so we never ask twice.
///
/// SAFETY: shows a MessageBoxW and writes one HKCU registry value.
pub(crate) unsafe fn maybe_prompt_autostart(hwnd: HWND) {
    let mut s = load_settings();
    if s.autostart_prompted {
        return;
    }
    let r = show_msgbox(
        hwnd,
        "Clippet",
        "Start Clippet automatically with Windows?",
        MB_YESNO | MB_ICONQUESTION,
    );
    s.autostart_prompted = true;
    if r == IDYES {
        if let Some(exe) = current_exe_quoted() {
            if registry_run_set(&exe).is_ok() {
                s.autostart_enabled = true;
            } else {
                crate::util::debug_log("Clippet: failed to write autostart registry value");
            }
        }
    }
    save_settings(&s);
}
