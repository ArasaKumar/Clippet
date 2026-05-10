#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

//! Clippet — native Windows 11 clipboard manager.
//!
//! Hidden popup summoned with Ctrl+Shift+V. Listens for clipboard
//! changes, renders the captured history, and pastes the chosen item
//! into the previously focused window via SetClipboardData +
//! SendInput(Ctrl+V). History persists at `%APPDATA%\Clippet\history.json`
//! (atomic write, 200-item cap; pinned items kept past the cap).
//! Rich content: text, RTF, HTML, files, images (PNG-encoded),
//! spreadsheets (TSV), and code (text captured while an IDE has the
//! foreground).
//!
//! `main.rs` is the orchestrator: it registers the window class, owns
//! the message loop, and dispatches Win32 messages to the focused
//! modules below.

mod clipboard;
mod footer;
mod listbox;
mod paste;
mod search;
mod state;
mod storage;
mod theme;
mod titlebar;
mod tray;
mod util;

use std::sync::atomic::Ordering;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_SETCUEBANNER, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
    MEASUREITEMSTRUCT, WM_MOUSELEAVE,
};
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clipboard::{capture_clipboard, register_formats};
use crate::listbox::{
    create_listbox, create_search_box, draw_listbox_item, make_bold_font_from,
    measure_listbox_item, toggle_pin_at_row,
};
use crate::paste::activate_selected;
use crate::search::{push_item, refresh_listbox, update_filter};
use crate::state::{
    BG_BRUSH, BOLD_FONT, CLOSE_BTN_ID, EDIT_ID, EN_CHANGE_CODE, FOOTER_HEIGHT, FOOTER_HOT_ITEM,
    FOOTER_TRACKING, HISTORY, HOTKEY_ID, IDM_ABOUT, IDM_CLEAR, IDM_EXIT, IDM_OPEN, IDM_SETTINGS,
    IS_DARK, LISTBOX, LISTBOX_ID, NEXT_ID, PALETTE, POPUP_H, POPUP_MIN_H, POPUP_MIN_W, POPUP_SIZE,
    POPUP_W, PREV_FG, RESIZE_MARGIN, SEARCH, SEARCH_BG_BRUSH, SEARCH_BOTTOM_GAP,
    SEARCH_EDIT_RIGHT_PAD, SEARCH_EDIT_VERT_INSET, SEARCH_HEIGHT, SEARCH_ICON_LEFT_PAD,
    SEARCH_ICON_RIGHT_GAP, SEARCH_ICON_SIZE, SEARCH_INSET, SEARCH_TOP_GAP, SELF_HWND, SEL_BRUSH,
    SUPPRESS_NEXT_UPDATE, TITLEBAR_HEIGHT, TITLE_FONT, TITLE_LABEL, UI_FONT, WM_APP_TRAY,
};
use crate::storage::{load_history, load_settings};
use crate::theme::{
    apply_child_theme, apply_popup_style, create_title_font, create_ui_font, detect_palette,
};
use crate::titlebar::{
    create_titlebar, draw_close_button, layout_titlebar, paint_search_chrome, paint_titlebar_bg,
};
use crate::tray::{
    add_tray_icon, clear_history, load_app_icon, maybe_prompt_autostart, persist_popup_size,
    remove_tray_icon, show_about, show_popup, show_settings_stub, show_tray_menu, toggle_popup,
    update_tray_tooltip,
};
use crate::util::show_msgbox;

// =====================================================================
// Window procedure. Each arm dispatches to the focused module.
// =====================================================================

/// SAFETY: signature matches the WNDPROC contract; every match arm
/// either returns LRESULT(0) or falls through to DefWindowProcW.
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            SELF_HWND.with(|c| c.set(hwnd));
            let is_dark = IS_DARK.with(|c| c.get());
            apply_popup_style(hwnd, is_dark);
            if create_listbox(hwnd).is_err() {
                return LRESULT(-1);
            }
            if create_search_box(hwnd).is_err() {
                return LRESULT(-1);
            }
            if create_titlebar(hwnd).is_err() {
                return LRESULT(-1);
            }
            // Push the system UI font (Segoe UI Variable on Win11) onto
            // the controls before deriving the bold variant — that way
            // the bold matched-text runs use the same family + size as
            // the rest.
            let font = create_ui_font();
            UI_FONT.with(|c| c.set(font));
            // Heading-style font for the custom title-bar label —
            // matches the semibold heading Win11's clipboard panel uses.
            let title_font = create_title_font();
            TITLE_FONT.with(|c| c.set(title_font));
            let lb = LISTBOX.with(|l| *l.borrow());
            let edit = SEARCH.with(|c| c.get());
            let title_label = TITLE_LABEL.with(|c| c.get());
            if !font.0.is_null() {
                if !lb.0.is_null() {
                    SendMessageW(lb, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                }
                if !edit.0.is_null() {
                    SendMessageW(edit, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
                }
            }
            if !title_font.0.is_null() && !title_label.0.is_null() {
                SendMessageW(
                    title_label,
                    WM_SETFONT,
                    WPARAM(title_font.0 as usize),
                    LPARAM(1),
                );
            }
            // Dark common-control theming for the listbox + edit when
            // the system is in dark mode (no-op in light mode).
            if !lb.0.is_null() && !edit.0.is_null() {
                apply_child_theme(lb, edit, is_dark);
            }
            // EM_SETCUEBANNER lays a "Search clipboard..." placeholder
            // over the empty edit. wParam=1 keeps the cue visible while
            // the edit has focus (matches Win11 search-box behavior).
            if !edit.0.is_null() {
                let cue: Vec<u16> = "Search clipboard..."
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                SendMessageW(
                    edit,
                    EM_SETCUEBANNER,
                    WPARAM(1),
                    LPARAM(cue.as_ptr() as isize),
                );
            }
            BOLD_FONT.with(|c| c.set(make_bold_font_from(lb)));
            // Footer button tooltips. Creates the tooltips_class32
            // control once; per-button rects are pushed in WM_SIZE.
            let _ = footer::create_tooltip(hwnd);
            register_formats();
            let _ = AddClipboardFormatListener(hwnd);
            // Ctrl+Shift+V global hotkey. If another app already holds
            // it, surface the failure instead of silently dropping it.
            if RegisterHotKey(hwnd, HOTKEY_ID, MOD_CONTROL | MOD_SHIFT, b'V' as u32).is_err() {
                show_msgbox(
                    hwnd,
                    "Clippet",
                    "Could not register Ctrl+Shift+V.\n\n\
                     Another running app is already using this shortcut.\n\
                     Close the conflicting app and restart Clippet.",
                    MB_OK | MB_ICONWARNING,
                );
            }
            // Capture whatever is currently on the clipboard at startup.
            if let Some(item) = capture_clipboard(hwnd) {
                push_item(item);
            }
            // Show whatever was loaded from disk (and any startup capture).
            refresh_listbox();
            // Tray icon. The popup itself receives the callback messages
            // so we don't need a separate message-only window.
            add_tray_icon(hwnd);
            // Reflect the loaded pinned count in the tooltip immediately.
            update_tray_tooltip(hwnd);
            LRESULT(0)
        }
        WM_APP_TRAY => {
            match lp.0 as u32 {
                WM_LBUTTONUP => toggle_popup(hwnd),
                WM_RBUTTONUP => show_tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            if wp.0 as i32 == HOTKEY_ID {
                show_popup(hwnd);
            }
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            let suppressed = SUPPRESS_NEXT_UPDATE.with(|f| {
                let was = f.get();
                f.set(false);
                was
            });
            if !suppressed {
                if let Some(item) = capture_clipboard(hwnd) {
                    if push_item(item) {
                        refresh_listbox();
                        update_tray_tooltip(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // LOWORD(wParam) == WA_INACTIVE: we just lost focus to
            // another window. The window is movable now, so we DON'T
            // auto-hide on blur — the user dismisses with X / Esc /
            // Enter. But we still need to track whatever window the
            // user clicked into so that a later activate_selected()
            // pastes there. lParam in WA_INACTIVE is the handle of the
            // window being activated.
            if (wp.0 as u32) & 0xFFFF == WA_INACTIVE {
                let activating = HWND(lp.0 as *mut _);
                if !activating.0.is_null() && activating.0 != hwnd.0 {
                    // Skip windows owned by us (MessageBoxes from the
                    // tray menu, the autostart prompt, etc.) —
                    // otherwise PREV_FG ends up pointing at a transient
                    // dialog handle and the next paste lands nowhere
                    // useful.
                    let owner = GetWindow(activating, GW_OWNER).unwrap_or_default();
                    if owner.0 != hwnd.0 {
                        PREV_FG.with(|c| c.set(activating));
                    }
                }
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Clicking the X in the caption hides the window instead of
            // destroying it — the process keeps running and the hotkey
            // re-summons it. Flush any user-applied resize to disk first.
            persist_popup_size();
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_NCCALCSIZE => {
            // wp == TRUE: rgrc[0] is the proposed client rect. By
            // returning 0 without calling DefWindowProc we keep client
            // area == window rect (no system caption / border chrome).
            // DWM still draws the rounded corners + acrylic backdrop
            // applied via apply_popup_style, and WS_THICKFRAME keeps
            // the resize edges responsive (handled in WM_NCHITTEST).
            if wp.0 != 0 {
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_NCPAINT => {
            // WS_THICKFRAME (kept for resize-edge hit testing) otherwise
            // draws a thin gray frame around the window even after
            // WM_NCCALCSIZE has zeroed the non-client area. Swallow the
            // message so DWM's rounded corners + shadow are the only
            // chrome the user sees.
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // The default proc would return HTCLIENT for our entire
            // window now that the caption is gone, so nothing would
            // be draggable or resizable. Map the outer edges to the
            // resize hit-test codes and the top strip (excluding the
            // close button — child windows get their own clicks
            // first) to HTCAPTION so the system handles dragging.
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
            let mut wr = RECT::default();
            if GetWindowRect(hwnd, &mut wr).is_err() {
                return DefWindowProcW(hwnd, msg, wp, lp);
            }
            let m = RESIZE_MARGIN;
            let in_left = x >= wr.left && x < wr.left + m;
            let in_right = x >= wr.right - m && x < wr.right;
            let in_top = y >= wr.top && y < wr.top + m;
            let in_bottom = y >= wr.bottom - m && y < wr.bottom;
            let hit = match (in_top, in_bottom, in_left, in_right) {
                (true, _, true, _) => HTTOPLEFT,
                (true, _, _, true) => HTTOPRIGHT,
                (_, true, true, _) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, _, _, _) => HTTOP,
                (_, true, _, _) => HTBOTTOM,
                (_, _, true, _) => HTLEFT,
                (_, _, _, true) => HTRIGHT,
                _ => {
                    // Above the search box → drag region. The close
                    // button is a child window and intercepts its own
                    // clicks before this hit-test runs.
                    if y - wr.top < TITLEBAR_HEIGHT {
                        HTCAPTION
                    } else {
                        HTCLIENT
                    }
                }
            };
            LRESULT(hit as isize)
        }
        WM_PAINT => {
            // Children paint themselves; the parent owns the title-bar
            // strip, the rounded search-chrome, and the bottom footer bar.
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            paint_titlebar_bg(hwnd, hdc);
            paint_search_chrome(hwnd, hdc);
            footer::paint(hwnd, hdc);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // Floor the resize to a usable size — below ~280x200 the
            // row text starts colliding with the pin column and the
            // search box.
            let info = lp.0 as *mut MINMAXINFO;
            if !info.is_null() {
                (*info).ptMinTrackSize.x = POPUP_MIN_W;
                (*info).ptMinTrackSize.y = POPUP_MIN_H;
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            // HIWORD(wParam) is the notification code (LBN_DBLCLK = 2
            // for the listbox, EN_CHANGE = 0x300 for the edit, BN_CLICKED
            // = 0 for buttons), or 0 for menu commands. LOWORD is the
            // command id.
            let code = (wp.0 as u32 >> 16) & 0xFFFF;
            let id = (wp.0 as u32) & 0xFFFF;
            if code == 2 && id == LISTBOX_ID as u32 {
                activate_selected(hwnd);
            } else if code == EN_CHANGE_CODE && id == EDIT_ID as u32 {
                update_filter();
            } else if id == CLOSE_BTN_ID as u32 {
                // Custom title-bar close button — same hide path as
                // WM_CLOSE so the process keeps running and the hotkey
                // re-summons it.
                let _ = SendMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            } else if code == 0 {
                match id {
                    IDM_OPEN => show_popup(hwnd),
                    IDM_CLEAR => clear_history(hwnd),
                    IDM_SETTINGS => show_settings_stub(hwnd),
                    IDM_ABOUT => show_about(hwnd),
                    IDM_EXIT => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // Forward clicks inside the footer area to the appropriate command.
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                match footer::hit_test(rc.right, rc.bottom, x, y) {
                    0 => clear_history(hwnd),
                    1 => show_settings_stub(hwnd),
                    2 => show_about(hwnd),
                    3 => { let _ = DestroyWindow(hwnd); }
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // Update footer hover state and arm leave-tracking if needed.
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                let new_hot = footer::hit_test(rc.right, rc.bottom, x, y);
                let old_hot = FOOTER_HOT_ITEM.with(|c| c.get());
                if new_hot != old_hot {
                    FOOTER_HOT_ITEM.with(|c| c.set(new_hot));
                    let footer_top = rc.bottom - FOOTER_HEIGHT;
                    let dirty = RECT {
                        left: rc.left,
                        top: footer_top,
                        right: rc.right,
                        bottom: rc.bottom,
                    };
                    let _ = InvalidateRect(hwnd, Some(&dirty), false);
                }
            }
            // Arm TME_LEAVE once per entry so WM_MOUSELEAVE fires when
            // the cursor exits the popup window rectangle.
            if !FOOTER_TRACKING.with(|c| c.get()) {
                FOOTER_TRACKING.with(|c| c.set(true));
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_MOUSELEAVE => {
            FOOTER_TRACKING.with(|c| c.set(false));
            if FOOTER_HOT_ITEM.with(|c| c.get()) >= 0 {
                FOOTER_HOT_ITEM.with(|c| c.set(-1));
                let mut rc = RECT::default();
                if GetClientRect(hwnd, &mut rc).is_ok() {
                    let footer_top = rc.bottom - FOOTER_HEIGHT;
                    let dirty = RECT {
                        left: rc.left,
                        top: footer_top,
                        right: rc.right,
                        bottom: rc.bottom,
                    };
                    let _ = InvalidateRect(hwnd, Some(&dirty), false);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            // The search box gets its own tinted brush so it reads as an
            // inset surface against the popup background — matching the
            // pill look in the landing-page mockup.
            let hdc = HDC(wp.0 as *mut _);
            let pal = PALETTE.with(|c| c.get());
            SetTextColor(hdc, COLORREF(pal.text));
            SetBkColor(hdc, COLORREF(pal.search_bg));
            let brush = SEARCH_BG_BRUSH.with(|c| c.get());
            LRESULT(brush.0 as isize)
        }
        WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            // Listbox dead space, the title-bar STATIC, and any button
            // child track the popup background. The returned brush is
            // reused on every repaint so we don't leak.
            let hdc = HDC(wp.0 as *mut _);
            let pal = PALETTE.with(|c| c.get());
            SetTextColor(hdc, COLORREF(pal.text));
            SetBkColor(hdc, COLORREF(pal.bg));
            let brush = BG_BRUSH.with(|c| c.get());
            LRESULT(brush.0 as isize)
        }
        WM_DRAWITEM => {
            let dis = lp.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                if (*dis).CtlID == CLOSE_BTN_ID as u32 {
                    draw_close_button(&*dis);
                } else {
                    draw_listbox_item(&*dis);
                }
            }
            LRESULT(1)
        }
        WM_MEASUREITEM => {
            measure_listbox_item(lp.0 as *mut MEASUREITEMSTRUCT);
            LRESULT(1)
        }
        WM_SIZE => {
            let w = (lp.0 as u32 & 0xFFFF) as i32;
            let h = ((lp.0 as u32 >> 16) & 0xFFFF) as i32;
            // Capture the outer window size so position_popup_at_cursor()
            // and persist_popup_size() see the user-applied dimensions
            // on the next summon. WM_SIZE delivers client size; pull the
            // outer rect explicitly so we round-trip the same width/
            // height back into SetWindowPos.
            let mut wr = RECT::default();
            if GetWindowRect(hwnd, &mut wr).is_ok() {
                POPUP_SIZE.with(|c| c.set((wr.right - wr.left, wr.bottom - wr.top)));
            }
            // Title bar runs across the top of the client area; the
            // search EDIT sits inside a parent-painted rounded surface
            // (paint_search_chrome) — its rect is inset for the icon
            // and vertical padding so the cue text reads as centered
            // inside the pill. Listbox takes everything below.
            layout_titlebar(w);
            let wrap_top = TITLEBAR_HEIGHT + SEARCH_TOP_GAP;
            let edit_left = SEARCH_INSET
                + SEARCH_ICON_LEFT_PAD
                + SEARCH_ICON_SIZE
                + SEARCH_ICON_RIGHT_GAP;
            let edit_right_bound = (w - SEARCH_INSET - SEARCH_EDIT_RIGHT_PAD).max(edit_left);
            let edit_w = (edit_right_bound - edit_left).max(0);
            let edit_y = wrap_top + SEARCH_EDIT_VERT_INSET;
            let edit_h = (SEARCH_HEIGHT - 2 * SEARCH_EDIT_VERT_INSET).max(0);
            SEARCH.with(|c| {
                let edit = c.get();
                if !edit.0.is_null() {
                    let _ = SetWindowPos(
                        edit, None, edit_left, edit_y, edit_w, edit_h, SWP_NOZORDER,
                    );
                }
            });
            LISTBOX.with(|lb| {
                let lb = *lb.borrow();
                if !lb.0.is_null() {
                    let top = wrap_top + SEARCH_HEIGHT + SEARCH_BOTTOM_GAP;
                    let lb_h = (h - top - FOOTER_HEIGHT).max(0);
                    let _ = SetWindowPos(lb, None, 0, top, w, lb_h, SWP_NOZORDER);
                }
            });
            // Refresh tooltip rectangles so the per-button hover targets
            // track the new layout after a resize.
            footer::update_tooltip_rects(hwnd, w, h);
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            let _ = UnregisterHotKey(hwnd, HOTKEY_ID);
            let _ = RemoveClipboardFormatListener(hwnd);
            // Free the GDI objects we own. Win32 cleans these up on
            // process exit anyway, but we own them explicitly so we
            // delete them explicitly.
            let bold = BOLD_FONT.with(|c| c.get());
            if !bold.0.is_null() {
                let _ = DeleteObject(bold);
                BOLD_FONT.with(|c| c.set(HFONT(std::ptr::null_mut())));
            }
            let ui_font = UI_FONT.with(|c| c.get());
            if !ui_font.0.is_null() {
                let _ = DeleteObject(ui_font);
                UI_FONT.with(|c| c.set(HFONT(std::ptr::null_mut())));
            }
            let title_font = TITLE_FONT.with(|c| c.get());
            if !title_font.0.is_null() {
                let _ = DeleteObject(title_font);
                TITLE_FONT.with(|c| c.set(HFONT(std::ptr::null_mut())));
            }
            let bg = BG_BRUSH.with(|c| c.get());
            if !bg.0.is_null() {
                let _ = DeleteObject(bg);
                BG_BRUSH.with(|c| c.set(HBRUSH(std::ptr::null_mut())));
            }
            let sel = SEL_BRUSH.with(|c| c.get());
            if !sel.0.is_null() {
                let _ = DeleteObject(sel);
                SEL_BRUSH.with(|c| c.set(HBRUSH(std::ptr::null_mut())));
            }
            let sbg = SEARCH_BG_BRUSH.with(|c| c.get());
            if !sbg.0.is_null() {
                let _ = DeleteObject(sbg);
                SEARCH_BG_BRUSH.with(|c| c.set(HBRUSH(std::ptr::null_mut())));
            }
            // Final history flush — ensures any in-session pins are
            // saved even if the app is force-closed before WM_DESTROY's
            // natural flush points.
            HISTORY.with(|h| crate::storage::save_history(&h.borrow()));
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn main() -> Result<()> {
    // Load persisted history before the window exists so WM_CREATE's
    // refresh_listbox() call sees the restored items.
    let (loaded, next_id) = load_history();
    NEXT_ID.store(next_id.max(1), Ordering::Relaxed);
    HISTORY.with(|h| *h.borrow_mut() = loaded);

    // SAFETY: every call inside this block is a documented Win32 API.
    // We follow Win32's threading rules: the message loop runs on the
    // thread that registered the window class and created the window.
    unsafe {
        // Per-monitor DPI v2 keeps cursor coords and window placement
        // in the same coordinate space across mixed-DPI displays.
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Load comctl32 and register the tooltip / toolbar / status-bar
        // window classes. The footer's hover tooltips need this — without
        // it, the "tooltips_class32" CreateWindowExW would fail. ICC_BAR_CLASSES
        // is the umbrella flag that covers the tooltip control.
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        // Detect light vs dark from AppsUseLightTheme and cache the
        // palette before we register the window class — the class's
        // hbrBackground needs the matching solid brush to avoid a
        // light flash on first paint.
        let (palette, is_dark) = detect_palette();
        PALETTE.with(|c| c.set(palette));
        IS_DARK.with(|c| c.set(is_dark));
        let bg_brush = CreateSolidBrush(COLORREF(palette.bg));
        let sel_brush = CreateSolidBrush(COLORREF(palette.row_sel));
        let search_bg_brush = CreateSolidBrush(COLORREF(palette.search_bg));
        BG_BRUSH.with(|c| c.set(bg_brush));
        SEL_BRUSH.with(|c| c.set(sel_brush));
        SEARCH_BG_BRUSH.with(|c| c.set(search_bg_brush));

        let hmod = GetModuleHandleW(None)?;
        let hinst = HINSTANCE(hmod.0);

        let class_name = w!("ClippetMainWindow");
        let hcursor = LoadCursorW(None, IDC_ARROW)?;
        // Big icon for Alt+Tab / task switcher; small icon for the
        // caption bar. Both come out of the multi-resolution
        // clippet.ico embedded by build.rs.
        let icon_big = load_app_icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON));
        let icon_small =
            load_app_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            hCursor: hcursor,
            hbrBackground: bg_brush,
            hIcon: icon_big,
            hIconSm: icon_small,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(Error::from_win32());
        }

        // Restore any persisted popup size; fall back to the defaults
        // on first launch (or after a settings.json reset).
        let saved = load_settings();
        let init_w = saved
            .popup_w
            .filter(|w| *w >= POPUP_MIN_W)
            .unwrap_or(POPUP_W);
        let init_h = saved
            .popup_h
            .filter(|h| *h >= POPUP_MIN_H)
            .unwrap_or(POPUP_H);
        POPUP_SIZE.with(|c| c.set((init_w, init_h)));

        // WS_EX_TOOLWINDOW keeps us out of the taskbar and Alt+Tab.
        // We drop WS_CAPTION (and WS_SYSMENU) to suppress the native
        // caption — the popup paints its own title bar in the client
        // area to match Windows 11's clipboard chrome (semibold heading,
        // flat close button that turns red on hover). WS_THICKFRAME
        // stays so WM_NCHITTEST can map the outer 6px to resize cursors.
        // WS_CLIPCHILDREN excludes child rects from the parent's paint,
        // so the rounded search-chrome painted in WM_PAINT doesn't
        // flicker over the EDIT / listbox / title-bar children.
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("Clippet"),
            WS_POPUP | WS_THICKFRAME | WS_CLIPCHILDREN,
            0,
            0,
            init_w,
            init_h,
            None,
            None,
            hinst,
            None,
        )?;

        // Start hidden — only Ctrl+Shift+V brings it up.
        let _ = UpdateWindow(hwnd);

        // First-launch autostart prompt. Runs after CreateWindowExW so
        // the MessageBox can be parented to our (hidden) popup, and
        // after add_tray_icon (called from WM_CREATE) so the tray icon
        // is visible by the time the user answers.
        maybe_prompt_autostart(hwnd);

        let mut msg = MSG::default();
        // GetMessageW returns 0 on WM_QUIT and -1 on error; either ends
        // the loop.
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            // Intercept navigation keys while the popup is visible. We
            // do this BEFORE TranslateMessage/DispatchMessage so the
            // EDIT control never sees Tab/Up/Down/Esc/Enter — otherwise
            // the single-line edit would beep on Esc/Enter and swallow
            // Tab as a control char, and arrow keys wouldn't drive the
            // listbox when focus is on the search box.
            if msg.message == WM_KEYDOWN {
                let popup = SELF_HWND.with(|c| c.get());
                let visible = !popup.0.is_null() && IsWindowVisible(popup).as_bool();
                if visible && handle_popup_key(popup, msg.wParam.0 as u16) {
                    continue;
                }
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}

/// Dispatch a keydown to popup-specific behavior. Returns true when
/// the key was handled (so the message loop should `continue` past
/// `TranslateMessage`).
///
/// SAFETY: caller has confirmed the popup is visible; all Win32 calls
/// operate on the popup's owned controls.
unsafe fn handle_popup_key(popup: HWND, vk: u16) -> bool {
    if vk == VK_ESCAPE.0 {
        let _ = ShowWindow(popup, SW_HIDE);
        true
    } else if vk == VK_RETURN.0 {
        activate_selected(popup);
        true
    } else if vk == VK_TAB.0 {
        // Cycle focus between search box and listbox.
        let lb = LISTBOX.with(|l| *l.borrow());
        let edit = SEARCH.with(|c| c.get());
        let cur = GetFocus();
        if !lb.0.is_null() && !edit.0.is_null() {
            if cur.0 == lb.0 {
                let _ = SetFocus(edit);
            } else {
                let _ = SetFocus(lb);
            }
        }
        true
    } else if vk == b'P' as u16 {
        // Ctrl+P toggles pin on the selected row. Bare P falls
        // through so the user can still type "p" into the search box.
        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
        if ctrl {
            let lb = LISTBOX.with(|l| *l.borrow());
            if !lb.0.is_null() {
                let sel = SendMessageW(lb, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                if sel >= 0 {
                    toggle_pin_at_row(lb, sel);
                }
            }
            return true;
        }
        false
    } else if vk == VK_UP.0 || vk == VK_DOWN.0 {
        let lb = LISTBOX.with(|l| *l.borrow());
        if !lb.0.is_null() {
            let count = SendMessageW(lb, LB_GETCOUNT, WPARAM(0), LPARAM(0)).0 as i32;
            if count > 0 {
                let cur = SendMessageW(lb, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32;
                let next = if vk == VK_DOWN.0 {
                    (cur + 1).min(count - 1)
                } else {
                    (cur - 1).max(0)
                };
                SendMessageW(lb, LB_SETCURSEL, WPARAM(next as usize), LPARAM(0));
            }
        }
        true
    } else {
        false
    }
}
