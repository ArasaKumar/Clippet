//! Theme detection (AppsUseLightTheme) + Win11 control styling
//! (DwmSetWindowAttribute, SetWindowTheme on the listbox/edit).
//!
//! The system palette is read at startup, but the theme is also live:
//! `apply_theme` performs a full runtime swap (brushes, class brush, DWM
//! attributes, child control theme, repaint) so the footer toggle takes
//! effect immediately without relaunching.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::state::{
    BG_BRUSH, IS_DARK, LISTBOX, PALETTE, Palette, SEARCH, SEARCH_BG_BRUSH, SEL_BRUSH,
    THEME_OVERRIDE, DARK_PALETTE, DWMSBT_TRANSIENTWINDOW, DWMWCP_ROUND, LIGHT_PALETTE,
};
use crate::storage::{load_settings, save_settings};
use crate::util::to_wide;

/// Read HKCU\...\Personalize\AppsUseLightTheme. Returns None on any
/// failure; callers fall back to light (Win11's factory state).
///
/// SAFETY: Every registry handle opened here is closed before return.
pub(crate) unsafe fn registry_apps_use_light_theme() -> Option<bool> {
    let key_name = to_wide(
        "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize",
    );
    let mut hkey: HKEY = HKEY::default();
    if RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR(key_name.as_ptr()),
        0,
        KEY_READ,
        &mut hkey,
    )
    .is_err()
    {
        return None;
    }
    let value_name = to_wide("AppsUseLightTheme");
    let mut value: u32 = 0;
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let mut kind = REG_VALUE_TYPE::default();
    let r = RegQueryValueExW(
        hkey,
        PCWSTR(value_name.as_ptr()),
        None,
        Some(&mut kind),
        Some(&mut value as *mut _ as *mut u8),
        Some(&mut size),
    );
    let _ = RegCloseKey(hkey);
    if r != ERROR_SUCCESS {
        return None;
    }
    Some(value != 0)
}

/// Resolve the active palette from the user's override (set by the
/// footer theme-toggle button, persisted in settings.json) falling
/// back to the system `AppsUseLightTheme` value. Returns
/// `(palette, is_dark)`.
pub(crate) fn detect_palette(override_light: Option<bool>) -> (Palette, bool) {
    let light = match override_light {
        Some(v) => v,
        // Default to light when the system value is missing — that's
        // Win11's factory state. SAFETY: registry_apps_use_light_theme
        // cleans up its own handles.
        None => unsafe { registry_apps_use_light_theme().unwrap_or(true) },
    };
    if light {
        (LIGHT_PALETTE, false)
    } else {
        (DARK_PALETTE, true)
    }
}

/// Pull the system's preferred UI font (Segoe UI Variable on Win11).
/// Falls back to whatever CreateFontIndirectW returns for a zeroed
/// LOGFONTW — good enough that we never need to ship a font ourselves.
///
/// SAFETY: NONCLIENTMETRICSW is a stack-allocated POD; its address is
/// passed to SystemParametersInfoW and only read until that call returns.
pub(crate) unsafe fn create_ui_font() -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    if SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut _),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_err()
    {
        let lf = LOGFONTW::default();
        return CreateFontIndirectW(&lf);
    }
    CreateFontIndirectW(&ncm.lfMessageFont)
}

/// Heading-style font for the custom title bar — same family as the
/// system message font (Segoe UI Variable on Win11) but ~30% larger
/// and semibold, matching the heading text Windows 11's clipboard /
/// "Emoji and more" panel uses for its panel title.
///
/// SAFETY: same contract as `create_ui_font` — NONCLIENTMETRICSW is a
/// stack POD only read for the duration of SystemParametersInfoW.
pub(crate) unsafe fn create_title_font() -> HFONT {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    if SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut _),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_err()
    {
        let lf = LOGFONTW::default();
        return CreateFontIndirectW(&lf);
    }
    let mut lf = ncm.lfMessageFont;
    // lfHeight is negative for character height in logical units; scale
    // up ~45% so the title reads as a prominent heading, matching the
    // landing-page mockup's brand chip.
    lf.lfHeight = ((lf.lfHeight as f32) * 1.45).round() as i32;
    // FW_BOLD = 700. The mockup's brand uses font-weight: 700.
    lf.lfWeight = 700;
    CreateFontIndirectW(&lf)
}

/// Apply dark common-control theming to the listbox + edit so their
/// scrollbars, edit border, and (in Win11) selection rendering switch
/// to dark variants. No-op when the system is in light mode.
///
/// SAFETY: SetWindowTheme accepts null-terminated wide-string class
/// names; the literals are static.
pub(crate) unsafe fn apply_child_theme(listbox: HWND, edit: HWND, is_dark: bool) {
    if is_dark {
        let _ = SetWindowTheme(listbox, w!("DarkMode_Explorer"), PCWSTR::null());
        let _ = SetWindowTheme(edit, w!("DarkMode_CFD"), PCWSTR::null());
    } else {
        // Light mode uses the controls' default theme; passing empty
        // strings resets any prior dark override.
        let _ = SetWindowTheme(listbox, w!("Explorer"), PCWSTR::null());
        let _ = SetWindowTheme(edit, w!("CFD"), PCWSTR::null());
    }
}

/// Apply Windows 11 popup styling: rounded corners, acrylic transient
/// backdrop, and a dark caption bar in dark mode. Failures are
/// non-fatal — older Windows builds simply ignore unknown DWM
/// attributes and we keep the solid brush.
///
/// SAFETY: Each DwmSetWindowAttribute receives a pointer-and-length
/// pair that points at a stack value of the matching size.
pub(crate) unsafe fn apply_popup_style(hwnd: HWND, is_dark: bool) {
    let pref: i32 = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const _ as *const _,
        std::mem::size_of::<i32>() as u32,
    );
    let backdrop: i32 = DWMSBT_TRANSIENTWINDOW;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_SYSTEMBACKDROP_TYPE,
        &backdrop as *const _ as *const _,
        std::mem::size_of::<i32>() as u32,
    );
    let dark: BOOL = BOOL(if is_dark { 1 } else { 0 });
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const _ as *const _,
        std::mem::size_of::<BOOL>() as u32,
    );
    // Win11 paints a 1-px accent border around every top-level window with
    // WS_THICKFRAME, on top of DWM's rounded-corner clip. That shows up as
    // a thin white line wrapping the popup. DWMWA_COLOR_NONE removes it
    // entirely so the only chrome is the rounded corner + acrylic shadow.
    let border: u32 = DWMWA_COLOR_NONE;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &border as *const _ as *const _,
        std::mem::size_of::<u32>() as u32,
    );
}

/// Switch the popup between light and dark at runtime: rebuild the
/// solid brushes, update the window class background (so WM_ERASEBKGND
/// stops painting with the old color), reapply DWM + common-control
/// theming, persist the choice to settings.json, then invalidate so
/// every child + parent-painted surface repaints with the new palette.
///
/// Safe to call repeatedly — old brushes are released after the swap so
/// repeated toggles don't accumulate GDI handles.
///
/// SAFETY: `hwnd` is the popup window owned by this process; all the
/// thread-locals touched here are populated for the session.
pub(crate) unsafe fn apply_theme(hwnd: HWND, light: bool) {
    let (palette, is_dark) = if light {
        (LIGHT_PALETTE, false)
    } else {
        (DARK_PALETTE, true)
    };
    PALETTE.with(|c| c.set(palette));
    IS_DARK.with(|c| c.set(is_dark));
    THEME_OVERRIDE.with(|c| c.set(Some(light)));

    // Create the replacement brushes first; swap them in atomically per
    // thread-local; then release the previous handles. Holding both for
    // a moment keeps the WM_CTLCOLOR* handlers from ever returning a
    // null brush during the swap.
    let new_bg = CreateSolidBrush(COLORREF(palette.bg));
    let new_sel = CreateSolidBrush(COLORREF(palette.row_sel));
    let new_search_bg = CreateSolidBrush(COLORREF(palette.search_bg));

    let old_bg = BG_BRUSH.with(|c| {
        let prev = c.get();
        c.set(new_bg);
        prev
    });
    let old_sel = SEL_BRUSH.with(|c| {
        let prev = c.get();
        c.set(new_sel);
        prev
    });
    let old_search_bg = SEARCH_BG_BRUSH.with(|c| {
        let prev = c.get();
        c.set(new_search_bg);
        prev
    });

    // The window class still references the original brush for the
    // default WM_ERASEBKGND path; swap it too so any erase between the
    // toggle and the WM_PAINT below uses the new color.
    SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, new_bg.0 as isize);

    if !old_bg.0.is_null() {
        let _ = DeleteObject(old_bg);
    }
    if !old_sel.0.is_null() {
        let _ = DeleteObject(old_sel);
    }
    if !old_search_bg.0.is_null() {
        let _ = DeleteObject(old_search_bg);
    }

    // Reapply DWM caption + scrollbar/edit theming so listbox scrollbars
    // and the edit's caret render in the matching variant.
    apply_popup_style(hwnd, is_dark);
    let lb = LISTBOX.with(|l| *l.borrow());
    let edit = SEARCH.with(|c| c.get());
    if !lb.0.is_null() && !edit.0.is_null() {
        apply_child_theme(lb, edit, is_dark);
    }

    // Repaint the footer's tooltip with the new "Switch to ..." label.
    crate::footer::refresh_theme_tooltip(hwnd);

    let _ = InvalidateRect(hwnd, None, true);

    // Persist so the choice survives a restart. Read-modify-write the
    // existing settings.json — autostart / popup-size fields are left
    // alone.
    let mut s = load_settings();
    s.theme_override = Some(light);
    save_settings(&s);
}
