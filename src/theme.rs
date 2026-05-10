//! Theme detection (AppsUseLightTheme) + Win11 control styling
//! (DwmSetWindowAttribute, SetWindowTheme on the listbox/edit).
//!
//! The palette is read once at startup; theme changes during a session
//! don't auto-apply — the user would need to reopen Clippet, which is
//! fine for the kind of app this is.

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Controls::SetWindowTheme;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::state::{
    Palette, DARK_PALETTE, DWMSBT_TRANSIENTWINDOW, DWMWCP_ROUND, LIGHT_PALETTE,
};
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

pub(crate) fn detect_palette() -> (Palette, bool) {
    // Default to light when the value is missing — that's Win11's factory state.
    // SAFETY: registry_apps_use_light_theme cleans up its own handles.
    let light = unsafe { registry_apps_use_light_theme().unwrap_or(true) };
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
