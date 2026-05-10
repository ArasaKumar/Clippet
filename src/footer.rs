//! Bottom footer bar — separator + four action rows (Clear History,
//! Settings..., About, Quit). Painted into the parent window's client
//! area below the listbox. Mouse events in that area are dispatched from
//! WM_LBUTTONDOWN / WM_MOUSEMOVE in `main.rs`.

use windows::core::PCWSTR;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::state::{
    BG_BRUSH, FOOTER_HEIGHT, FOOTER_HOT_ITEM, FOOTER_ITEM_H, FOOTER_PAD_X, PALETTE, SEL_BRUSH,
    UI_FONT,
};

const ITEMS: &[&str] = &["Clear History", "Settings...", "About", "Quit"];
pub(crate) const ITEM_COUNT: i32 = ITEMS.len() as i32;

/// Return the footer row index (0..ITEM_COUNT-1) for the given client-space
/// y-coordinate, or -1 if y does not fall within a footer action row.
pub(crate) fn hit_test(client_h: i32, y: i32) -> i32 {
    let sep_top = client_h - FOOTER_HEIGHT;
    if y <= sep_top {
        return -1;
    }
    let item = (y - sep_top - 1) / FOOTER_ITEM_H;
    if item >= 0 && item < ITEM_COUNT { item } else { -1 }
}

/// Paint the separator line, row backgrounds, and label text into `hdc`.
/// Called from the parent window's WM_PAINT handler after BeginPaint.
///
/// SAFETY: `hwnd` is the popup window we own. BG_BRUSH / SEL_BRUSH /
/// UI_FONT are live for the session. All other GDI handles are created
/// and deleted within this call.
pub(crate) unsafe fn paint(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return;
    }
    let pal = PALETTE.with(|c| c.get());
    let sep_top = rc.bottom - FOOTER_HEIGHT;

    // Fill the entire footer background first so partial repaints
    // (hover changes) don't leave stale highlight artefacts.
    let footer_rc = RECT { left: rc.left, top: sep_top, right: rc.right, bottom: rc.bottom };
    let bg = BG_BRUSH.with(|c| c.get());
    if !bg.0.is_null() {
        FillRect(hdc, &footer_rc, bg);
    }

    // Separator line.
    let sep_pen = CreatePen(PS_SOLID, 1, COLORREF(pal.subtext));
    if !sep_pen.0.is_null() {
        let old = SelectObject(hdc, sep_pen);
        let _ = MoveToEx(hdc, rc.left, sep_top, None);
        let _ = LineTo(hdc, rc.right, sep_top);
        SelectObject(hdc, old);
        let _ = DeleteObject(sep_pen);
    }

    let font = UI_FONT.with(|c| c.get());
    let old_font = if !font.0.is_null() {
        SelectObject(hdc, font)
    } else {
        HGDIOBJ(std::ptr::null_mut())
    };
    SetBkMode(hdc, TRANSPARENT);

    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let hot = FOOTER_HOT_ITEM.with(|c| c.get());

    for (i, label) in ITEMS.iter().enumerate() {
        let i = i as i32;
        let item_top = sep_top + 1 + i * FOOTER_ITEM_H;
        let item_rc = RECT {
            left: rc.left,
            top: item_top,
            right: rc.right,
            bottom: item_top + FOOTER_ITEM_H,
        };

        if hot == i {
            let sel = SEL_BRUSH.with(|c| c.get());
            if !sel.0.is_null() {
                FillRect(hdc, &item_rc, sel);
            }
        }

        SetTextColor(hdc, COLORREF(pal.text));
        let text_x = rc.left + FOOTER_PAD_X;
        let text_y = item_top + (FOOTER_ITEM_H - tm.tmHeight) / 2;
        let clip = RECT {
            left: text_x,
            top: item_top,
            right: rc.right - FOOTER_PAD_X,
            bottom: item_top + FOOTER_ITEM_H,
        };
        let wide: Vec<u16> = label.encode_utf16().collect();
        let _ = ExtTextOutW(
            hdc,
            text_x,
            text_y,
            ETO_CLIPPED,
            Some(&clip as *const _),
            PCWSTR(wide.as_ptr()),
            wide.len() as u32,
            None,
        );
    }

    if !old_font.0.is_null() {
        SelectObject(hdc, old_font);
    }
}
