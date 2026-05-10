//! Custom title bar — replaces the native caption with a flat
//! Windows-11-style strip (semibold "Clippet" label + subtle close
//! button that turns red on hover), matching the chrome of Win11's
//! clipboard/"Emoji and more" panel.
//!
//! The title label is a STATIC and the close button is a BUTTON with
//! BS_OWNERDRAW; the parent's WM_DRAWITEM dispatches close-button
//! rendering to `draw_close_button`. A subclass on the close button
//! tracks WM_MOUSEMOVE / WM_MOUSELEAVE so we can re-render on hover.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, WM_MOUSELEAVE};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TRACKMOUSEEVENT, TME_LEAVE, TrackMouseEvent,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::state::{
    BG_BRUSH, CLOSE_BTN, CLOSE_BTN_H, CLOSE_BTN_HOT, CLOSE_BTN_ID, CLOSE_BTN_SUBCLASS_ID,
    CLOSE_BTN_W, CLOSE_HOT_BG, CLOSE_HOT_TEXT, ODS_SELECTED_BIT, PALETTE, SEARCH_BG_BRUSH,
    SEARCH_HEIGHT, SEARCH_ICON_LEFT_PAD, SEARCH_ICON_SIZE, SEARCH_INSET, SEARCH_RADIUS,
    SEARCH_TOP_GAP, TITLEBAR_HEIGHT, TITLE_LABEL, TITLE_LABEL_ID, TITLE_PAD_X,
};
use crate::util::to_wide;

// SS_LEFT | SS_CENTERIMAGE — left-aligned, vertically centered.
const SS_LEFT_BIT: u32 = 0x0000;
const SS_CENTERIMAGE_BIT: u32 = 0x0200;
const SS_NOPREFIX_BIT: u32 = 0x0080;
const BS_OWNERDRAW_BIT: u32 = 0x0000_000B;

/// Create the two title-bar children: a STATIC for the "Clippet" label
/// and a BUTTON (BS_OWNERDRAW) for the close glyph. Both stay parented
/// to the popup so they show up over the background brush automatically.
///
/// SAFETY: GetModuleHandleW returns a valid HMODULE; STATIC and BUTTON
/// are system classes always registered.
pub(crate) unsafe fn create_titlebar(parent: HWND) -> Result<()> {
    let hmod = GetModuleHandleW(None)?;
    let hinst = HINSTANCE(hmod.0);

    let label_text = to_wide("Clippet");
    let label_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | SS_LEFT_BIT | SS_CENTERIMAGE_BIT | SS_NOPREFIX_BIT,
    );
    let label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR(label_text.as_ptr()),
        label_style,
        TITLE_PAD_X,
        0,
        160,
        TITLEBAR_HEIGHT,
        parent,
        HMENU(TITLE_LABEL_ID as usize as *mut _),
        hinst,
        None,
    )?;
    TITLE_LABEL.with(|c| c.set(label));

    // BS_OWNERDRAW so we control the entire visual — flat fill at rest,
    // red fill on hover, white X on red.
    let btn_style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | BS_OWNERDRAW_BIT);
    let btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("BUTTON"),
        w!(""),
        btn_style,
        0,
        0,
        CLOSE_BTN_W,
        CLOSE_BTN_H,
        parent,
        HMENU(CLOSE_BTN_ID as usize as *mut _),
        hinst,
        None,
    )?;
    CLOSE_BTN.with(|c| c.set(btn));
    let _ = SetWindowSubclass(btn, Some(close_btn_subclass_proc), CLOSE_BTN_SUBCLASS_ID, 0);
    Ok(())
}

/// Lay out the title-bar children inside the parent's client width.
/// Called from WM_SIZE before positioning the search box and listbox.
///
/// SAFETY: only invoked from the UI thread; child handles read from
/// thread-locals are valid for the session.
pub(crate) unsafe fn layout_titlebar(client_w: i32) {
    // Close button hugs the top-right with no margin so the hover
    // square reaches the rounded corner — same feel as Win11's chrome.
    let btn = CLOSE_BTN.with(|c| c.get());
    if !btn.0.is_null() {
        let _ = SetWindowPos(
            btn,
            None,
            client_w - CLOSE_BTN_W,
            0,
            CLOSE_BTN_W,
            CLOSE_BTN_H,
            SWP_NOZORDER,
        );
    }
    // Title label takes the remaining width minus the close button.
    let label = TITLE_LABEL.with(|c| c.get());
    if !label.0.is_null() {
        let label_w = (client_w - CLOSE_BTN_W - TITLE_PAD_X).max(0);
        let _ = SetWindowPos(
            label,
            None,
            TITLE_PAD_X,
            0,
            label_w,
            TITLEBAR_HEIGHT,
            SWP_NOZORDER,
        );
    }
}

/// Owner-draw the close button. At rest: same color as the popup
/// background with a thin gray X. On hover: Win11 red fill (#C42B1C)
/// with a white X. Strokes are drawn with GDI so they stay crisp at
/// any DPI without depending on a specific icon font.
///
/// SAFETY: dis is supplied by WM_DRAWITEM and outlives this call.
pub(crate) unsafe fn draw_close_button(dis: &DRAWITEMSTRUCT) {
    let pal = PALETTE.with(|c| c.get());
    let hot = CLOSE_BTN_HOT.with(|c| c.get());
    // For BS_OWNERDRAW buttons, ODS_SELECTED (0x0001) is set while the
    // mouse is pressed down on the button.
    let pressed = (dis.itemState.0 & ODS_SELECTED_BIT) != 0;

    let (fill, stroke) = if hot && pressed {
        // Slightly darker red while the mouse is held down — matches
        // Win11's pressed state without needing a separate constant.
        (darken(CLOSE_HOT_BG), CLOSE_HOT_TEXT)
    } else if hot {
        (CLOSE_HOT_BG, CLOSE_HOT_TEXT)
    } else {
        (pal.bg, pal.text)
    };

    let bg_brush = CreateSolidBrush(COLORREF(fill));
    if !bg_brush.0.is_null() {
        FillRect(dis.hDC, &dis.rcItem, bg_brush);
        let _ = DeleteObject(bg_brush);
    }

    // 10×10 X centered in the button, drawn with a 2-px-wide pen so the
    // diagonals carry the same visual weight as Win11's caption X
    // without depending on Segoe Fluent Icons being installed.
    let cx = (dis.rcItem.left + dis.rcItem.right) / 2;
    let cy = (dis.rcItem.top + dis.rcItem.bottom) / 2;
    let arm: i32 = 5;

    let pen = CreatePen(PS_SOLID, 2, COLORREF(stroke));
    if !pen.0.is_null() {
        let old_pen = SelectObject(dis.hDC, pen);
        let _ = MoveToEx(dis.hDC, cx - arm, cy - arm, None);
        let _ = LineTo(dis.hDC, cx + arm + 1, cy + arm + 1);
        let _ = MoveToEx(dis.hDC, cx + arm, cy - arm, None);
        let _ = LineTo(dis.hDC, cx - arm - 1, cy + arm + 1);
        SelectObject(dis.hDC, old_pen);
        let _ = DeleteObject(pen);
    }
}

/// COLORREF is 0x00BBGGRR. Darken each channel by ~12% for the
/// pressed state.
fn darken(c: u32) -> u32 {
    let r = (c & 0xFF) as f32 * 0.88;
    let g = ((c >> 8) & 0xFF) as f32 * 0.88;
    let b = ((c >> 16) & 0xFF) as f32 * 0.88;
    ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
}

/// Subclass on the close BUTTON: track WM_MOUSEMOVE / WM_MOUSELEAVE so
/// we can repaint with the hover fill. WM_NCCREATE arms the leave
/// tracking lazily on the first mouse-move (TrackMouseEvent re-arms
/// once per leave event).
///
/// SAFETY: signature matches SUBCLASSPROC; we always either return
/// LRESULT(0) for handled messages or fall through to DefSubclassProc.
unsafe extern "system" fn close_btn_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let was_hot = CLOSE_BTN_HOT.with(|c| c.get());
            if !was_hot {
                CLOSE_BTN_HOT.with(|c| c.set(true));
                // Arm leave-tracking so we get WM_MOUSELEAVE when the
                // pointer exits the button rect.
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = TrackMouseEvent(&mut tme);
                let _ = InvalidateRect(hwnd, None, true);
            }
        }
        WM_MOUSELEAVE => {
            CLOSE_BTN_HOT.with(|c| c.set(false));
            let _ = InvalidateRect(hwnd, None, true);
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(
                hwnd,
                Some(close_btn_subclass_proc),
                CLOSE_BTN_SUBCLASS_ID,
            );
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

/// Erase the title-bar strip with the popup background brush. Called
/// from WM_PAINT in the parent so the area stays clean when child
/// controls don't fully cover it.
///
/// SAFETY: hwnd is the popup; BG_BRUSH outlives the call.
pub(crate) unsafe fn paint_titlebar_bg(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return;
    }
    let bar = RECT {
        left: rc.left,
        top: rc.top,
        right: rc.right,
        bottom: rc.top + TITLEBAR_HEIGHT,
    };
    let brush = BG_BRUSH.with(|c| c.get());
    if !brush.0.is_null() {
        FillRect(hdc, &bar, brush);
    }
}

/// Paint the rounded surface + magnifying-glass icon behind the search
/// EDIT. Called from the parent's WM_PAINT before the EDIT's own paint
/// runs. The EDIT's WM_CTLCOLOREDIT brush matches search_bg so the
/// EDIT's rect blends seamlessly into this surface; only the rounded
/// corners and the icon area are visible outside the EDIT's rect.
///
/// SAFETY: hwnd is the popup; SEARCH_BG_BRUSH outlives the call. All
/// GDI handles created here are deleted before returning.
pub(crate) unsafe fn paint_search_chrome(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return;
    }
    let pal = PALETTE.with(|c| c.get());
    let left = rc.left + SEARCH_INSET;
    let right = rc.right - SEARCH_INSET;
    let top = rc.top + TITLEBAR_HEIGHT + SEARCH_TOP_GAP;
    let bottom = top + SEARCH_HEIGHT;
    if right <= left + 2 * SEARCH_RADIUS {
        // Window resized too narrow to draw a meaningful pill — skip,
        // the EDIT's own bg will still render.
        return;
    }

    // Filled rounded rectangle. Pen color matches the fill so RoundRect's
    // outline is invisible.
    let pen = CreatePen(PS_SOLID, 1, COLORREF(pal.search_bg));
    let brush = SEARCH_BG_BRUSH.with(|c| c.get());
    let old_pen = SelectObject(hdc, pen);
    let old_brush = SelectObject(hdc, brush);
    let _ = RoundRect(
        hdc,
        left,
        top,
        right,
        bottom,
        SEARCH_RADIUS * 2,
        SEARCH_RADIUS * 2,
    );
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(pen);

    // Magnifying-glass icon: a small circle plus a diagonal handle, drawn
    // with a 2-px pen in the secondary text color. Aligned to the left of
    // the rounded surface so the EDIT (offset right of the icon) starts
    // right after the gap.
    let icon_cx = left + SEARCH_ICON_LEFT_PAD + SEARCH_ICON_SIZE / 2;
    let icon_cy = top + SEARCH_HEIGHT / 2;
    let r = SEARCH_ICON_SIZE / 2;
    let icon_pen = CreatePen(PS_SOLID, 2, COLORREF(pal.subtext));
    let null_brush = GetStockObject(NULL_BRUSH);
    let old_pen = SelectObject(hdc, icon_pen);
    let old_brush = SelectObject(hdc, null_brush);
    let _ = Ellipse(hdc, icon_cx - r, icon_cy - r, icon_cx + r, icon_cy + r);
    let h_start_x = icon_cx + (r * 7) / 10;
    let h_start_y = icon_cy + (r * 7) / 10;
    let _ = MoveToEx(hdc, h_start_x, h_start_y, None);
    let _ = LineTo(hdc, h_start_x + r - 1, h_start_y + r - 1);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(icon_pen);
}
