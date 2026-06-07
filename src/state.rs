//! Shared types, constants, and thread-local state.
//!
//! Every other module imports from here. The thread-locals are `pub(crate)`
//! so call sites can read/write them directly via `STATE.with(|c| c.get())`.
//! All Win32 message handling runs on the main thread, so single-threaded
//! `Cell`/`RefCell` storage is safe.

use std::cell::{Cell, RefCell};
use std::sync::atomic::AtomicU64;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------------
// Standard predefined clipboard format identifiers (Win32 CF_* constants).
// ---------------------------------------------------------------------

pub(crate) const CF_TEXT: u32 = 1;
pub(crate) const CF_BITMAP: u32 = 2;
pub(crate) const CF_DIB: u32 = 8;
pub(crate) const CF_UNICODETEXT: u32 = 13;
pub(crate) const CF_HDROP: u32 = 15;

// ---------------------------------------------------------------------
// Popup geometry and global hotkey id. POPUP_W/H are the *defaults* used
// on first launch; the user-resized size is persisted in settings.json
// (and held in POPUP_SIZE during the session).
// ---------------------------------------------------------------------

pub(crate) const POPUP_W: i32 = 400;
pub(crate) const POPUP_H: i32 = 500;
pub(crate) const POPUP_MIN_W: i32 = 280;
pub(crate) const POPUP_MIN_H: i32 = 280;
pub(crate) const HOTKEY_ID: i32 = 1;

// ---------------------------------------------------------------------
// DWM attribute values. windows-rs exposes the IDs as DWMWINDOWATTRIBUTE
// values; we keep numeric mirrors for the actual *values* fed to those
// attributes.
// ---------------------------------------------------------------------

pub(crate) const DWMSBT_TRANSIENTWINDOW: i32 = 3; // acrylic-style backdrop
pub(crate) const DWMWCP_ROUND: i32 = 2;

// ---------------------------------------------------------------------
// Tray (Level 5).
// ---------------------------------------------------------------------

pub(crate) const WM_APP_TRAY: u32 = WM_APP + 1;
pub(crate) const TRAY_ICON_ID: u32 = 1;
pub(crate) const IDM_OPEN: u32 = 100;
pub(crate) const IDM_CLEAR: u32 = 101;
pub(crate) const IDM_SETTINGS: u32 = 102;
pub(crate) const IDM_EXIT: u32 = 103;
pub(crate) const IDM_ABOUT: u32 = 104;

// ---------------------------------------------------------------------
// Search (Level 6).
// ---------------------------------------------------------------------

pub(crate) const ES_LEFT_BIT: u32 = 0x0000;
pub(crate) const ES_AUTOHSCROLL_BIT: u32 = 0x0080;
pub(crate) const EN_CHANGE_CODE: u32 = 0x0300;
pub(crate) const EDIT_ID: u16 = 1;
pub(crate) const LISTBOX_ID: u16 = 2;
pub(crate) const CLOSE_BTN_ID: u16 = 3;
pub(crate) const TITLE_LABEL_ID: u16 = 4;
pub(crate) const SEARCH_HEIGHT: i32 = 40;
// Geometry of the parent-painted search-box chrome (rounded surface +
// magnifying-glass icon). All values are inside the search wrapper rect,
// which itself sits at SEARCH_INSET from each side of the popup.
pub(crate) const SEARCH_INSET: i32 = 12;
pub(crate) const SEARCH_TOP_GAP: i32 = 6;
pub(crate) const SEARCH_BOTTOM_GAP: i32 = 8;
pub(crate) const SEARCH_RADIUS: i32 = 8;
pub(crate) const SEARCH_ICON_LEFT_PAD: i32 = 14;
pub(crate) const SEARCH_ICON_SIZE: i32 = 14;
pub(crate) const SEARCH_ICON_RIGHT_GAP: i32 = 8;
pub(crate) const SEARCH_EDIT_VERT_INSET: i32 = 6;
pub(crate) const SEARCH_EDIT_RIGHT_PAD: i32 = 14;
// DRAWITEMSTRUCT.itemState bits (DRAWITEMSTRUCT_FLAGS values). Still used
// by the title-bar close button's owner-draw (pressed state).
pub(crate) const ODS_SELECTED_BIT: u32 = 0x0001;

pub(crate) const TEXT_ITEM_HEIGHT: u32 = 34;

// ---------------------------------------------------------------------
// Custom title bar — replaces the native caption to match Windows 11
// Clipboard's flat chrome (small subtle close button, semibold heading).
// ---------------------------------------------------------------------
pub(crate) const TITLEBAR_HEIGHT: i32 = 48;
pub(crate) const CLOSE_BTN_W: i32 = 46;
pub(crate) const CLOSE_BTN_H: i32 = 40;
pub(crate) const TITLE_PAD_X: i32 = 12;
pub(crate) const CLOSE_BTN_SUBCLASS_ID: usize = 0xC2;
pub(crate) const RESIZE_MARGIN: i32 = 6;

// ---------------------------------------------------------------------
// Bottom footer bar — 1px separator + a compact horizontal row of
// flat icon buttons (clear, settings, about, quit), right-aligned.
// ---------------------------------------------------------------------

pub(crate) const FOOTER_HEIGHT: i32 = 44; // 1px separator + 43px button row
pub(crate) const FOOTER_BTN_W: i32 = 40;
pub(crate) const FOOTER_BTN_H: i32 = 43;
pub(crate) const FOOTER_ICON_SIZE: i32 = 20;
pub(crate) const FOOTER_PAD_X: i32 = 6; // right margin from window edge
/// Base id for the four rect-based tooltip tools (clear/settings/about/quit).
/// Each tool is registered with `TOOLTIP_BASE_ID + index`.
pub(crate) const TOOLTIP_BASE_ID: usize = 1;
// Win11 close-button hover red ≈ #C42B1C (BGR for COLORREF).
pub(crate) const CLOSE_HOT_BG: u32 = 0x001C2BC4;
pub(crate) const CLOSE_HOT_TEXT: u32 = 0x00FFFFFF;

// ---------------------------------------------------------------------
// Pin / context menu (Level 7).
// ---------------------------------------------------------------------

pub(crate) const PIN_GLYPH_PINNED: &str = "\u{2605}"; // ★
pub(crate) const PIN_GLYPH_UNPINNED: &str = "\u{2606}"; // ☆
pub(crate) const PIN_AREA_W: i32 = 22;
pub(crate) const IDM_ROW_PASTE: u32 = 200;
pub(crate) const IDM_ROW_PIN: u32 = 201;
pub(crate) const IDM_ROW_COPY: u32 = 202;
pub(crate) const IDM_ROW_DELETE: u32 = 203;

// ---------------------------------------------------------------------
// Storage cap.
// ---------------------------------------------------------------------

pub(crate) const MAX_ITEMS: usize = 200;
pub(crate) const STORAGE_VERSION: u32 = 2;
/// Longest edge of the cached thumbnail PNG written next to each image
/// in `%APPDATA%\Clippet\media\`. Sized to fit the 2*TEXT_ITEM_HEIGHT
/// thumb area at 1.5x DPI; the listbox StretchBlts to row size.
pub(crate) const THUMB_MAX_SIZE: u32 = 96;

// =====================================================================
// Win11 palette — colors are COLORREF (0x00BBGGRR), so written in BGR
// order. Selected per-system theme at startup, then everything (window
// class brush, WM_CTLCOLOR* handlers, owner-draw) reads from the cached
// PALETTE so light/dark stays consistent across the popup.
// =====================================================================

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub bg: u32,         // window + control surface
    pub row_sel: u32,    // selected listbox row
    pub text: u32,       // primary text
    pub subtext: u32,    // secondary text (timestamp)
    pub accent: u32,     // pinned star (gold)
    pub pin_dim: u32,    // unpinned star
    pub search_bg: u32,  // search-box surface tint (slightly off the window bg)
    // Per-format tag colors. COLORREF is 0x00BBGGRR so values are written
    // in BGR. Picked from the landing-page mockup for visual continuity.
    pub tag_text: u32,
    pub tag_rich: u32,
    pub tag_image: u32,
    pub tag_file: u32,
    pub tag_html: u32,
    pub tag_sheet: u32,
    pub tag_code: u32,
}

// Win11 dark surface ≈ #202020. Selection ≈ #3C3C3C. Pin gold ≈ #FACC15.
// Tag colors mirror site/styles.css (tag-text/-rich/-image/-file/-html/-sheet/-code).
pub(crate) const DARK_PALETTE: Palette = Palette {
    bg: 0x00202020,
    row_sel: 0x003C3C3C,
    text: 0x00F0F0F0,
    subtext: 0x00A0A0A0,
    accent: 0x0015CCFA,
    pin_dim: 0x00606060,
    search_bg: 0x00333333,
    tag_text:  0x00E1D5CB, // #CBD5E1
    tag_rich:  0x00A5A5FC, // #FCA5A5
    tag_image: 0x00ACEF86, // #86EFAC
    tag_file:  0x004DD3FC, // #FCD34D
    tag_html:  0x00FDB5C4, // #C4B5FD
    tag_sheet: 0x00D4EA5E, // #5EEAD4
    tag_code:  0x00FDC593, // #93C5FD
};

// Win11 light surface ≈ #FAFAFA. Selection ≈ #E5E5E5. Light-mode tag colors
// are the saturated variants of the dark-mode palette so they read on white.
pub(crate) const LIGHT_PALETTE: Palette = Palette {
    bg: 0x00FAFAFA,
    row_sel: 0x00E5E5E5,
    text: 0x00111111,
    subtext: 0x00606060,
    accent: 0x000DA1CA, // #CAA10D — amber, matches gold-on-white
    pin_dim: 0x00B0B0B0,
    search_bg: 0x00F1F1F1,
    tag_text:  0x00695447, // #475569
    tag_rich:  0x002626DC, // #DC2626
    tag_image: 0x004AA316, // #16A34A
    tag_file:  0x00048ACA, // #CA8A04
    tag_html:  0x00ED3A7C, // #7C3AED
    tag_sheet: 0x0088940D, // #0D9488
    tag_code:  0x00EB6325, // #2563EB
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ItemType {
    Text,
    RichText,
    Image,
    File,
    Html,
    Spreadsheet,
    Code,
}

impl ItemType {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            ItemType::Text => "[T]",
            ItemType::RichText => "[R]",
            ItemType::Image => "[I]",
            ItemType::File => "[F]",
            ItemType::Html => "[H]",
            ItemType::Spreadsheet => "[X]",
            ItemType::Code => "[C]",
        }
    }

    pub(crate) fn tag_color(&self, pal: &Palette) -> u32 {
        match self {
            ItemType::Text => pal.tag_text,
            ItemType::RichText => pal.tag_rich,
            ItemType::Image => pal.tag_image,
            ItemType::File => pal.tag_file,
            ItemType::Html => pal.tag_html,
            ItemType::Spreadsheet => pal.tag_sheet,
            ItemType::Code => pal.tag_code,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ClipItem {
    pub id: u64,
    pub kind: ItemType,
    /// Inline payload bytes. Empty for `Image` items (PNG bytes live on
    /// disk under `media/{id}.png` instead — keeps history.json tiny and
    /// peak RAM bounded). Populated for every other kind.
    pub raw: Vec<u8>,
    pub preview: String,
    pub timestamp: u64,
    pub pinned: bool,
    /// Optional language hint (file extension, lower-cased) for Code items.
    pub lang: Option<String>,
    /// Stable FNV-1a/64 of the source bytes (PNG bytes for images, raw
    /// bytes otherwise). Drives consecutive-duplicate suppression in
    /// `push_item` so we don't need to keep the bytes resident just to
    /// compare against the next capture.
    pub content_hash: u64,
    /// Filename within `media/` for the full-resolution PNG. Some only
    /// for `Image` items.
    pub media_file: Option<String>,
    /// Filename within `media/` for the listbox thumbnail PNG.
    pub thumb_file: Option<String>,
    pub media_w: Option<u32>,
    pub media_h: Option<u32>,
}

#[derive(Default, Clone, Copy)]
pub(crate) struct RegFormats {
    pub rtf: u32,
    pub html: u32,
    pub sheet: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct FilterRow {
    pub hist_index: usize,
    /// Byte indices into the item's `preview` that the fuzzy matcher
    /// flagged as matched chars. Empty when the search box is empty.
    pub indices: Vec<usize>,
}

// =====================================================================
// Thread-local state. Single-threaded by design — the Win32 message loop
// runs on the main thread, and these are read/written exclusively from
// message handlers and helpers called by them.
// =====================================================================

// Const-initialised thread-locals avoid the per-access lazy-init check.
// Every constructor here is `const fn` (Cell::new, RefCell::new,
// Vec::new, HWND/HFONT/HBRUSH tuple-struct constructors).
thread_local! {
    pub(crate) static HISTORY: RefCell<Vec<ClipItem>> = const { RefCell::new(Vec::new()) };
    pub(crate) static LISTBOX: RefCell<HWND> = const { RefCell::new(HWND(std::ptr::null_mut())) };
    pub(crate) static REG: RefCell<RegFormats> = const { RefCell::new(RegFormats { rtf: 0, html: 0, sheet: 0 }) };
    /// Window we should restore focus to after pasting.
    pub(crate) static PREV_FG: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// The popup itself; needed by message-loop key intercepts and the
    /// listbox subclass.
    pub(crate) static SELF_HWND: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// Set right before SetClipboardData so the next WM_CLIPBOARDUPDATE is ignored.
    pub(crate) static SUPPRESS_NEXT_UPDATE: Cell<bool> = const { Cell::new(false) };
    /// Search box and the cached bold font used for matched-char highlights.
    pub(crate) static SEARCH: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    pub(crate) static BOLD_FONT: Cell<HFONT> = const { Cell::new(HFONT(std::ptr::null_mut())) };
    /// What's currently rendered in the listbox: a row->history mapping
    /// plus the byte indices of the matched chars within each item's preview.
    pub(crate) static FILTERED: RefCell<Vec<FilterRow>> = const { RefCell::new(Vec::new()) };
    /// Theme + cached GDI objects derived from it.
    pub(crate) static PALETTE: Cell<Palette> = const { Cell::new(DARK_PALETTE) };
    pub(crate) static IS_DARK: Cell<bool> = const { Cell::new(true) };
    /// User-applied theme override: `None` follows the system
    /// `AppsUseLightTheme` value, `Some(true)` forces light, `Some(false)`
    /// forces dark. Hydrated from settings.json at startup; mutated by
    /// the footer theme-toggle button.
    pub(crate) static THEME_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
    pub(crate) static BG_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
    pub(crate) static SEL_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
    /// Brush used for the search-box background — slightly tinted off the
    /// window bg so the field reads as an inset surface, like the mockup.
    pub(crate) static SEARCH_BG_BRUSH: Cell<HBRUSH> = const { Cell::new(HBRUSH(std::ptr::null_mut())) };
    pub(crate) static UI_FONT: Cell<HFONT> = const { Cell::new(HFONT(std::ptr::null_mut())) };
    /// Heading font used for the custom title-bar label.
    pub(crate) static TITLE_FONT: Cell<HFONT> = const { Cell::new(HFONT(std::ptr::null_mut())) };
    /// Custom title-bar children.
    pub(crate) static TITLE_LABEL: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    pub(crate) static CLOSE_BTN: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
    /// Hover state for the close button — set by the close-button subclass
    /// from WM_MOUSEMOVE/WM_MOUSELEAVE so WM_DRAWITEM picks the right fill.
    pub(crate) static CLOSE_BTN_HOT: Cell<bool> = const { Cell::new(false) };
    /// User-resized popup size — kept in memory during the session;
    /// flushed to settings.json on hide so it survives restarts.
    pub(crate) static POPUP_SIZE: Cell<(i32, i32)> = const { Cell::new((POPUP_W, POPUP_H)) };
    /// Which footer row (0‥3) the mouse is currently hovering over; -1
    /// means no row is highlighted.
    pub(crate) static FOOTER_HOT_ITEM: Cell<i32> = const { Cell::new(-1) };
    /// True while TrackMouseEvent(TME_LEAVE) is armed for the popup so we
    /// don't re-arm it on every WM_MOUSEMOVE.
    pub(crate) static FOOTER_TRACKING: Cell<bool> = const { Cell::new(false) };
    /// The standard "tooltips_class32" control. Owns the per-button hover
    /// tooltips and (via TTF_SUBCLASS) intercepts mouse events on the
    /// popup itself to show/hide the tip automatically.
    pub(crate) static TOOLTIP_HWND: Cell<HWND> = const { Cell::new(HWND(std::ptr::null_mut())) };
}

pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);
