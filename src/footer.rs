//! Bottom footer bar — 1px separator + a row of right-aligned, flat
//! icon buttons (Clear History, Settings, About, Quit). Painted into the
//! parent window's client area below the listbox. Mouse events are
//! dispatched from WM_LBUTTONDOWN / WM_MOUSEMOVE in `main.rs`.
//!
//! Icons are drawn with GDI primitives (same approach as the close
//! button) so we don't depend on any specific icon font being installed,
//! and each carries a per-action accent color for quick visual scanning.
//! Hover tooltips are provided by the standard "tooltips_class32"
//! control with TTF_SUBCLASS so it intercepts mouse events on the popup
//! and shows the tip automatically.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Controls::{
    TOOLTIPS_CLASSW, TOOLTIP_FLAGS, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW,
    TTM_UPDATETIPTEXTW, TTS_ALWAYSTIP, TTS_NOPREFIX, TTTOOLINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::state::{
    BG_BRUSH, FOOTER_BTN_H, FOOTER_BTN_W, FOOTER_HEIGHT, FOOTER_HOT_ITEM, FOOTER_ICON_SIZE,
    FOOTER_PAD_X, IS_DARK, PALETTE, Palette, RESIZE_MARGIN, SEL_BRUSH, TOOLTIP_BASE_ID,
    TOOLTIP_HWND,
};

pub(crate) const ITEM_COUNT: i32 = 5;

/// A footer button's semantic action. The index→action mapping lives
/// here, next to the index→icon (`icon_color`/`paint`) and index→tooltip
/// tables, so a button reorder only has to be made in one file. `main.rs`
/// dispatches on the enum rather than on a raw 0..4 index.
#[derive(Clone, Copy)]
pub(crate) enum FooterAction {
    ThemeToggle,
    ClearHistory,
    Settings,
    About,
    Quit,
}

/// Resolve a client-space click to the footer action under it, if any.
pub(crate) fn action_at(client_w: i32, client_h: i32, x: i32, y: i32) -> Option<FooterAction> {
    match hit_test(client_w, client_h, x, y) {
        0 => Some(FooterAction::ThemeToggle),
        1 => Some(FooterAction::ClearHistory),
        2 => Some(FooterAction::Settings),
        3 => Some(FooterAction::About),
        4 => Some(FooterAction::Quit),
        _ => None,
    }
}

/// X coordinate of the leftmost button (button index 0 = Clear History).
fn buttons_left(client_w: i32) -> i32 {
    client_w - FOOTER_PAD_X - ITEM_COUNT * FOOTER_BTN_W
}

/// Return the footer button index (0..ITEM_COUNT-1) for the given
/// client-space (x, y), or -1 if the point isn't on a button. Both
/// dimensions are checked because buttons are now laid out horizontally.
pub(crate) fn hit_test(client_w: i32, client_h: i32, x: i32, y: i32) -> i32 {
    let sep_top = client_h - FOOTER_HEIGHT;
    if y <= sep_top || y >= sep_top + 1 + FOOTER_BTN_H {
        return -1;
    }
    let left = buttons_left(client_w);
    if x < left || x >= left + ITEM_COUNT * FOOTER_BTN_W {
        return -1;
    }
    (x - left) / FOOTER_BTN_W
}

/// Per-action accent color. Trash is red so destructive actions read
/// at a glance; quit is amber as a softer "caution"; info is blue;
/// settings stays neutral in the secondary text color. The theme
/// toggle borrows the pinned-star accent so it reads as a primary,
/// non-destructive action.
fn icon_color(idx: i32, pal: &Palette) -> u32 {
    match idx {
        0 => pal.accent,    // Theme toggle — gold accent
        1 => pal.tag_rich,  // Clear History — red
        2 => pal.subtext,   // Settings — neutral
        3 => pal.tag_code,  // About — blue
        4 => pal.tag_file,  // Quit — amber
        _ => pal.text,
    }
}

/// Paint the separator line, button hover backgrounds, and icon glyphs
/// into `hdc`. Called from the parent's WM_PAINT after BeginPaint.
///
/// SAFETY: `hwnd` is the popup window we own. BG_BRUSH / SEL_BRUSH are
/// live for the session. All other GDI handles are created and deleted
/// within this call.
pub(crate) unsafe fn paint(hwnd: HWND, hdc: HDC) {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return;
    }
    let pal = PALETTE.with(|c| c.get());
    let sep_top = rc.bottom - FOOTER_HEIGHT;

    // Wipe the footer first so partial repaints (hover changes) don't
    // leave stale highlight artefacts.
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

    let hot = FOOTER_HOT_ITEM.with(|c| c.get());
    let left = buttons_left(rc.right);
    let btn_top = sep_top + 1;

    for i in 0..ITEM_COUNT {
        let btn_x = left + i * FOOTER_BTN_W;
        let btn_rc = RECT {
            left: btn_x,
            top: btn_top,
            right: btn_x + FOOTER_BTN_W,
            bottom: btn_top + FOOTER_BTN_H,
        };
        if hot == i {
            let sel = SEL_BRUSH.with(|c| c.get());
            if !sel.0.is_null() {
                FillRect(hdc, &sel_rect_above_resize(&btn_rc), sel);
            }
        }
        let cx = btn_x + FOOTER_BTN_W / 2;
        // Center icons in the upper portion of the button so they stay
        // clear of the bottom resize edge that overlaps the row.
        let usable_h = FOOTER_BTN_H - RESIZE_MARGIN;
        let cy = btn_top + (usable_h - FOOTER_ICON_SIZE) / 2 + FOOTER_ICON_SIZE / 2;
        let color = icon_color(i, &pal);
        match i {
            0 => {
                // Show the *target* state: sun when currently dark (so a
                // click switches to light), moon when currently light.
                let dark = IS_DARK.with(|c| c.get());
                if dark {
                    draw_sun_icon(hdc, cx, cy, color);
                } else {
                    draw_moon_icon(hdc, cx, cy, color);
                }
            }
            1 => draw_trash_icon(hdc, cx, cy, color),
            2 => draw_cog_icon(hdc, cx, cy, color),
            3 => draw_info_icon(hdc, cx, cy, color),
            4 => draw_power_icon(hdc, cx, cy, color),
            _ => {}
        }
    }
}

/// Trim the highlight to the area above the bottom resize zone so the
/// hover fill doesn't bleed into a region that won't actually respond to
/// clicks (WM_NCHITTEST returns HTBOTTOM there).
fn sel_rect_above_resize(btn: &RECT) -> RECT {
    RECT {
        left: btn.left,
        top: btn.top,
        right: btn.right,
        bottom: btn.bottom - RESIZE_MARGIN,
    }
}

/// Make a 2-px solid pen in `color`, run `f` with it selected, then
/// restore the previous pen and delete ours. Keeps the per-icon paint
/// helpers focused on the geometry instead of GDI plumbing.
///
/// SAFETY: caller passes a valid HDC; the pen is created and deleted
/// in-call so no GDI handle escapes.
unsafe fn with_pen<F: FnOnce(HDC)>(hdc: HDC, color: u32, f: F) {
    let pen = CreatePen(PS_SOLID, 2, COLORREF(color));
    if pen.0.is_null() {
        return;
    }
    let old = SelectObject(hdc, pen);
    f(hdc);
    SelectObject(hdc, old);
    let _ = DeleteObject(pen);
}

/// Trash bin: small handle on top, lid line, and a slightly tapered
/// bin body. ~18×18 visual footprint centered on (cx, cy).
unsafe fn draw_trash_icon(hdc: HDC, cx: i32, cy: i32, color: u32) {
    with_pen(hdc, color, |hdc| {
        // Lid handle (small flat tab on top).
        let _ = MoveToEx(hdc, cx - 3, cy - 9, None);
        let _ = LineTo(hdc, cx + 4, cy - 9);
        // Lid line.
        let _ = MoveToEx(hdc, cx - 8, cy - 6, None);
        let _ = LineTo(hdc, cx + 9, cy - 6);
        // Bin body — left side, bottom, right side, slightly tapered.
        let _ = MoveToEx(hdc, cx - 7, cy - 6, None);
        let _ = LineTo(hdc, cx - 5, cy + 8);
        let _ = LineTo(hdc, cx + 6, cy + 8);
        let _ = LineTo(hdc, cx + 8, cy - 6);
    });
}

/// Cog: outer hollow ring, four cardinal "teeth" sticking outward,
/// and a small inner ring to hint at the gear hub.
unsafe fn draw_cog_icon(hdc: HDC, cx: i32, cy: i32, color: u32) {
    let null_brush = GetStockObject(NULL_BRUSH);
    with_pen(hdc, color, |hdc| {
        let old_brush = SelectObject(hdc, null_brush);
        // Outer ring.
        let _ = Ellipse(hdc, cx - 7, cy - 7, cx + 7, cy + 7);
        // Inner ring (the hub).
        let _ = Ellipse(hdc, cx - 2, cy - 2, cx + 2, cy + 2);
        // 4 cardinal teeth.
        for &(dx, dy) in &[(0i32, -10i32), (0, 10), (-10, 0), (10, 0)] {
            let inner_x = cx + dx * 6 / 10;
            let inner_y = cy + dy * 6 / 10;
            let _ = MoveToEx(hdc, inner_x, inner_y, None);
            let _ = LineTo(hdc, cx + dx, cy + dy);
        }
        SelectObject(hdc, old_brush);
    });
}

/// Info: hollow circle with a stylized lower-case "i" inside. The dot
/// is a 2-px stub above a 6-px stem so it reads as an "i" rather than
/// a single tall stroke.
unsafe fn draw_info_icon(hdc: HDC, cx: i32, cy: i32, color: u32) {
    let null_brush = GetStockObject(NULL_BRUSH);
    with_pen(hdc, color, |hdc| {
        let old_brush = SelectObject(hdc, null_brush);
        let _ = Ellipse(hdc, cx - 9, cy - 9, cx + 9, cy + 9);
        SelectObject(hdc, old_brush);
        // Dot (just above center).
        let _ = MoveToEx(hdc, cx, cy - 5, None);
        let _ = LineTo(hdc, cx, cy - 4);
        // Stem.
        let _ = MoveToEx(hdc, cx, cy - 1, None);
        let _ = LineTo(hdc, cx, cy + 5);
    });
}

/// Sun: a small hollow circle with eight short rays radiating outward.
/// Drawn for the theme-toggle button when the popup is currently in
/// dark mode (clicking the button switches to light, so the icon shows
/// the destination state).
unsafe fn draw_sun_icon(hdc: HDC, cx: i32, cy: i32, color: u32) {
    let null_brush = GetStockObject(NULL_BRUSH);
    with_pen(hdc, color, |hdc| {
        let old_brush = SelectObject(hdc, null_brush);
        // Sun body.
        let _ = Ellipse(hdc, cx - 4, cy - 4, cx + 4, cy + 4);
        SelectObject(hdc, old_brush);
        // Eight rays at 45° intervals — inner endpoint just outside the
        // body, outer endpoint near the icon's nominal 9-px radius.
        for &(dx, dy, ex, ey) in &[
            (0i32, -6i32, 0i32, -9i32),
            (0, 6, 0, 9),
            (-6, 0, -9, 0),
            (6, 0, 9, 0),
            (-4, -4, -6, -6),
            (4, -4, 6, -6),
            (-4, 4, -6, 6),
            (4, 4, 6, 6),
        ] {
            let _ = MoveToEx(hdc, cx + dx, cy + dy, None);
            let _ = LineTo(hdc, cx + ex, cy + ey);
        }
    });
}

/// Moon: filled crescent built as the region difference between an
/// outer disc and a smaller disc offset to the right. `RGN_DIFF` is
/// resolution-independent (no polyline ambiguity), and a single
/// `FillRgn` keeps the silhouette crisp regardless of the underlying
/// background — hover state vs. rest doesn't need a separate code path.
unsafe fn draw_moon_icon(hdc: HDC, cx: i32, cy: i32, color: u32) {
    let outer = CreateEllipticRgn(cx - 8, cy - 8, cx + 8, cy + 8);
    if outer.0.is_null() {
        return;
    }
    // Bite circle: same vertical bounds but shifted right by ~5px so
    // the carved edge sits inside the moon body, leaving a left-facing
    // crescent with a comfortable bulge.
    let bite = CreateEllipticRgn(cx - 3, cy - 8, cx + 13, cy + 8);
    if bite.0.is_null() {
        let _ = DeleteObject(outer);
        return;
    }
    let crescent = CreateRectRgn(0, 0, 0, 0);
    if crescent.0.is_null() {
        let _ = DeleteObject(outer);
        let _ = DeleteObject(bite);
        return;
    }
    let _ = CombineRgn(crescent, outer, bite, RGN_DIFF);

    let brush = CreateSolidBrush(COLORREF(color));
    if !brush.0.is_null() {
        let _ = FillRgn(hdc, crescent, brush);
        let _ = DeleteObject(brush);
    }
    let _ = DeleteObject(outer);
    let _ = DeleteObject(bite);
    let _ = DeleteObject(crescent);
}

/// Power: a near-full arc with a gap at the top and a vertical stem
/// passing through the gap. We trace the curve as a 36-segment polyline
/// instead of using GDI `Arc` because Arc with two endpoints near each
/// other at the top of the circle ambiguously picks the short arc
/// (across the top) rather than the long way around the bottom.
unsafe fn draw_power_icon(hdc: HDC, cx: i32, cy: i32, color: u32) {
    with_pen(hdc, color, |hdc| {
        let radius: f32 = 8.0;
        // Math angles, CCW from +x. With Win32's y-flipped screen coords,
        // 270° points up (12 o'clock). The arc skips ±20° around 270°,
        // leaving a 40°-wide gap at the top for the power-button stem.
        let start_deg: f32 = 290.0; // ~1 o'clock area, just right of the gap
        let sweep_deg: f32 = 320.0; // wraps through 0/90/180 to ~11 o'clock
        let segments: i32 = 36;
        let to_pt = |deg: f32| -> (i32, i32) {
            let r = deg.to_radians();
            let x = cx as f32 + radius * r.cos();
            let y = cy as f32 + radius * r.sin();
            (x.round() as i32, y.round() as i32)
        };
        let (sx, sy) = to_pt(start_deg);
        let _ = MoveToEx(hdc, sx, sy, None);
        for i in 1..=segments {
            let deg = start_deg + sweep_deg * i as f32 / segments as f32;
            let (x, y) = to_pt(deg);
            let _ = LineTo(hdc, x, y);
        }
        // Vertical stem through the gap at the top.
        let _ = MoveToEx(hdc, cx, cy - 9, None);
        let _ = LineTo(hdc, cx, cy);
    });
}

// =====================================================================
// Tooltips.
// =====================================================================

/// Per-tool labels. `w!()` wraps each as a static UTF-16 PCWSTR, so we
/// can hand the pointers to TOOLINFOW.lpszText without managing
/// lifetimes ourselves. The theme-toggle tooltip is set per-state in
/// `refresh_theme_tooltip` after the toggle flips IS_DARK.
fn tip_for(idx: i32) -> PCWSTR {
    match idx {
        0 => theme_toggle_tip(),
        1 => w!("Clear History"),
        2 => w!("Settings"),
        3 => w!("About"),
        4 => w!("Quit"),
        _ => w!(""),
    }
}

/// Tooltip text for the theme toggle. Mirrors `draw_sun_icon` /
/// `draw_moon_icon`: shows the *target* state so the user knows what
/// will happen when they click.
fn theme_toggle_tip() -> PCWSTR {
    let dark = IS_DARK.with(|c| c.get());
    if dark {
        w!("Switch to Light Mode")
    } else {
        w!("Switch to Dark Mode")
    }
}

/// Create the tooltip control once at WM_CREATE time and register one
/// rect-based tool per footer button. TTF_SUBCLASS makes the tooltip
/// install its own subclass on the parent so it can intercept
/// WM_MOUSEMOVE itself — we don't have to relay events manually.
///
/// The tool rects start zero-sized; `update_tooltip_rects` fills them
/// in on every WM_SIZE so the tooltips track resizes.
///
/// SAFETY: GetModuleHandleW returns a valid HMODULE; CreateWindowExW
/// uses the system tooltips class. The lpszText pointers come from
/// `w!()` static strings that live for the program's lifetime.
pub(crate) unsafe fn create_tooltip(parent: HWND) -> Result<()> {
    let hmod = GetModuleHandleW(None)?;
    let hinst = HINSTANCE(hmod.0);
    let style = WINDOW_STYLE(WS_POPUP.0 | TTS_NOPREFIX | TTS_ALWAYSTIP);
    let tt = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        TOOLTIPS_CLASSW,
        w!(""),
        style,
        0,
        0,
        0,
        0,
        parent,
        None,
        hinst,
        None,
    )?;
    let _ = SetWindowPos(
        tt,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    TOOLTIP_HWND.with(|c| c.set(tt));

    for i in 0..ITEM_COUNT {
        let tip = tip_for(i);
        let mut ti: TTTOOLINFOW = std::mem::zeroed();
        ti.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        ti.uFlags = TOOLTIP_FLAGS(TTF_SUBCLASS.0);
        ti.hwnd = parent;
        ti.uId = TOOLTIP_BASE_ID + i as usize;
        // Cast PCWSTR (const) to PWSTR (mut) — tooltip control reads
        // from this pointer but never writes to it for fixed text.
        ti.lpszText = PWSTR(tip.0 as *mut u16);
        ti.hinst = hinst;
        SendMessageW(
            tt,
            TTM_ADDTOOLW,
            WPARAM(0),
            LPARAM(&ti as *const _ as isize),
        );
    }
    Ok(())
}

/// Refresh the theme-toggle tooltip text after the popup flips between
/// light and dark. Re-reads `IS_DARK` via `theme_toggle_tip` and sends
/// TTM_UPDATETIPTEXTW so a hover after the toggle reads the new label.
///
/// SAFETY: TOOLTIP_HWND is populated by `create_tooltip`; SendMessageW
/// on a null handle is a no-op (we early-exit).
pub(crate) unsafe fn refresh_theme_tooltip(parent: HWND) {
    let tt = TOOLTIP_HWND.with(|c| c.get());
    if tt.0.is_null() {
        return;
    }
    let hmod = match GetModuleHandleW(None) {
        Ok(m) => HINSTANCE(m.0),
        Err(_) => HINSTANCE(std::ptr::null_mut()),
    };
    let tip = theme_toggle_tip();
    let mut ti: TTTOOLINFOW = std::mem::zeroed();
    ti.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
    ti.hwnd = parent;
    ti.uId = TOOLTIP_BASE_ID; // theme toggle is button index 0
    ti.lpszText = PWSTR(tip.0 as *mut u16);
    ti.hinst = hmod;
    SendMessageW(
        tt,
        TTM_UPDATETIPTEXTW,
        WPARAM(0),
        LPARAM(&ti as *const _ as isize),
    );
}

/// Push the current button rects into the tooltip control so it knows
/// where each tool lives after a resize. Call from WM_SIZE.
///
/// SAFETY: TOOLTIP_HWND is set by `create_tooltip`; SendMessageW on a
/// null handle is a no-op (we early-exit).
pub(crate) unsafe fn update_tooltip_rects(parent: HWND, client_w: i32, client_h: i32) {
    let tt = TOOLTIP_HWND.with(|c| c.get());
    if tt.0.is_null() {
        return;
    }
    let sep_top = client_h - FOOTER_HEIGHT;
    let left = buttons_left(client_w);
    let btn_top = sep_top + 1;
    for i in 0..ITEM_COUNT {
        let btn_x = left + i * FOOTER_BTN_W;
        let mut ti: TTTOOLINFOW = std::mem::zeroed();
        ti.cbSize = std::mem::size_of::<TTTOOLINFOW>() as u32;
        ti.hwnd = parent;
        ti.uId = TOOLTIP_BASE_ID + i as usize;
        ti.rect = RECT {
            left: btn_x,
            top: btn_top,
            right: btn_x + FOOTER_BTN_W,
            // Trim to the same area we accept clicks in; the resize edge
            // shouldn't be marketed as a button.
            bottom: btn_top + FOOTER_BTN_H - RESIZE_MARGIN,
        };
        SendMessageW(
            tt,
            TTM_NEWTOOLRECTW,
            WPARAM(0),
            LPARAM(&ti as *const _ as isize),
        );
    }
}
