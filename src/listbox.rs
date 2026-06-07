//! Custom owner-drawn list control + search-edit control.
//!
//! The history list is a bespoke `WS_CHILD` window class ("ClippetList")
//! rather than the Win32 `LISTBOX`. The stock listbox can only scroll in
//! whole-item steps (it always snaps a row to the top edge), which reads
//! as choppy with the variable row heights here (image rows are 3× tall).
//! This control tracks a *pixel* scroll offset, double-buffers every
//! paint, and eases the offset toward a target on a frame timer so the
//! wheel produces smooth, continuous motion.
//!
//! The module also owns the owner-draw row rendering (fuzzy-match bold
//! runs + image thumbnails), the thumbnail bitmap cache, and the per-row
//! operations (pin / copy / delete / context menu).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Controls::{SetScrollInfo, SetScrollPos};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clipboard::{decode_dib_to_bgra, looks_like_png, png_to_dib};
use crate::paste::{activate_selected, set_clipboard_from_item};
use crate::search::{refresh_listbox, row_to_hist};
use crate::state::{
    BG_BRUSH, BOLD_FONT, EDIT_ID, ES_AUTOHSCROLL_BIT, ES_LEFT_BIT, FILTERED, HISTORY,
    IDM_ROW_COPY, IDM_ROW_DELETE, IDM_ROW_PASTE, IDM_ROW_PIN, ItemType, LISTBOX, LISTBOX_ID,
    PALETTE, PIN_AREA_W, PIN_GLYPH_PINNED, PIN_GLYPH_UNPINNED, SEARCH, SEARCH_HEIGHT, SELF_HWND,
    SEL_BRUSH, TEXT_ITEM_HEIGHT, UI_FONT,
};
use crate::storage::media_path;
use crate::tray::update_tray_tooltip;
use crate::util::{relative_time, to_wide};

// =====================================================================
// Control constants + custom scroll state.
// =====================================================================

/// Class name for the custom list window.
const LIST_CLASS: PCWSTR = w!("ClippetList");
/// Timer id for the scroll-easing animation.
const SCROLL_TIMER_ID: usize = 1;
/// Animation tick interval (~60 fps).
const SCROLL_TIMER_MS: u32 = 16;
/// Pixels the scroll target advances per mouse-wheel notch (WHEEL_DELTA).
const WHEEL_NOTCH_PX: i32 = 60;
/// Easing divisor: each tick closes 1/N of the remaining distance.
const SCROLL_EASE_DIV: i32 = 4;

thread_local! {
    /// Current (animated) vertical scroll offset in pixels, >= 0.
    static LIST_SCROLL_Y: Cell<i32> = const { Cell::new(0) };
    /// Target the animation eases `LIST_SCROLL_Y` toward.
    static LIST_SCROLL_TARGET: Cell<i32> = const { Cell::new(0) };
    /// Selected row index (FILTERED order); -1 when nothing is selected.
    static LIST_SEL: Cell<i32> = const { Cell::new(-1) };
    /// Whether the scroll-easing timer is currently armed.
    static LIST_ANIM: Cell<bool> = const { Cell::new(false) };
    /// Cumulative pixel tops of each row: `len() == rows + 1`, with the
    /// last entry equal to the total content height. Rebuilt whenever the
    /// filtered view changes.
    static LIST_ROW_TOPS: RefCell<Vec<i32>> = const { RefCell::new(Vec::new()) };
}

#[inline]
fn scroll_y() -> i32 {
    LIST_SCROLL_Y.with(|c| c.get())
}
#[inline]
fn set_scroll_y(v: i32) {
    LIST_SCROLL_Y.with(|c| c.set(v));
}
#[inline]
fn scroll_target() -> i32 {
    LIST_SCROLL_TARGET.with(|c| c.get())
}
#[inline]
fn set_scroll_target(v: i32) {
    LIST_SCROLL_TARGET.with(|c| c.set(v));
}
#[inline]
fn sel() -> i32 {
    LIST_SEL.with(|c| c.get())
}

// =====================================================================
// Control creation.
// =====================================================================

/// Register the custom list window class (idempotent) and create the
/// history-list child window.
///
/// SAFETY: GetModuleHandleW returns a valid HMODULE; the class is
/// registered before first use and CreateWindowExW is given valid
/// parameters. The resulting HWND is stored in LISTBOX for the session.
pub(crate) unsafe fn create_listbox(parent: HWND) -> Result<()> {
    let hmod = GetModuleHandleW(None)?;
    let hinst = HINSTANCE(hmod.0);

    // CS_DBLCLKS so we receive WM_LBUTTONDBLCLK (paste on double-click).
    // Null background brush: every pixel is painted in WM_PAINT.
    let wc = WNDCLASSW {
        style: CS_DBLCLKS,
        lpfnWndProc: Some(list_wnd_proc),
        hInstance: hinst,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszClassName: LIST_CLASS,
        ..Default::default()
    };
    // A zero atom means the class is already registered (harmless) or the
    // call failed; CreateWindowExW below surfaces a real failure either way.
    let _ = RegisterClassW(&wc);

    let style = WS_CHILD | WS_VISIBLE | WS_VSCROLL;
    let lb = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        LIST_CLASS,
        w!(""),
        style,
        0,
        0,
        100,
        100,
        parent,
        HMENU(LISTBOX_ID as usize as *mut _),
        hinst,
        None,
    )?;
    LISTBOX.with(|s| *s.borrow_mut() = lb);
    Ok(())
}

/// SAFETY: see `create_listbox`. The EDIT system class is always
/// registered.
pub(crate) unsafe fn create_search_box(parent: HWND) -> Result<HWND> {
    let hmod = GetModuleHandleW(None)?;
    let hinst = HINSTANCE(hmod.0);
    // No WS_BORDER — the popup paints a tinted surface behind the field
    // via WM_CTLCOLOREDIT, matching the inset-pill look of the mockup.
    let style = WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | ES_LEFT_BIT | ES_AUTOHSCROLL_BIT);
    let edit = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("EDIT"),
        w!(""),
        style,
        0,
        0,
        100,
        SEARCH_HEIGHT,
        parent,
        HMENU(EDIT_ID as usize as *mut _),
        hinst,
        None,
    )?;
    SEARCH.with(|c| c.set(edit));
    Ok(edit)
}

/// Derive a bold version of the listbox's default font so owner-draw can
/// switch fonts for matched-character runs without disturbing the global
/// system font. Returns null on any GDI failure — the draw path handles
/// that by falling back to the regular font (no highlight).
///
/// SAFETY: SendMessageW(WM_GETFONT) returns either an HFONT or null;
/// LOGFONTW is a stack-allocated POD passed to GetObjectW.
pub(crate) unsafe fn make_bold_font_from(owner: HWND) -> HFONT {
    let lr = SendMessageW(owner, WM_GETFONT, WPARAM(0), LPARAM(0));
    let regular = HFONT(lr.0 as *mut _);
    if regular.0.is_null() {
        return HFONT(std::ptr::null_mut());
    }
    let mut lf: LOGFONTW = std::mem::zeroed();
    let n = GetObjectW(
        regular,
        std::mem::size_of::<LOGFONTW>() as i32,
        Some(&mut lf as *mut _ as *mut _),
    );
    if n == 0 {
        return HFONT(std::ptr::null_mut());
    }
    lf.lfWeight = FW_BOLD.0 as i32;
    CreateFontIndirectW(&lf)
}

// =====================================================================
// Owner-draw text helpers.
// =====================================================================

thread_local! {
    /// Reusable UTF-16 buffer for the per-paint text helpers below.
    /// Owner-draw fires once per visible row per scroll tick; allocating
    /// a fresh `Vec<u16>` per `measure_text` / `text_out` call was
    /// visible as scroll stutter.
    static WIDE_SCRATCH: RefCell<Vec<u16>> = RefCell::new(Vec::with_capacity(256));
}

/// SAFETY: GetTextExtentPoint32W reads the wide buffer for its full
/// length; the buffer outlives the call (the `with` borrow scope wraps
/// it).
unsafe fn measure_text(hdc: HDC, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    WIDE_SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        buf.extend(text.encode_utf16());
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &buf, &mut size);
        size.cx
    })
}

/// SAFETY: ExtTextOutW reads the wide buffer for `buf.len()` chars; the
/// buffer outlives the call (the `with` borrow scope wraps it).
unsafe fn text_out(hdc: HDC, x: i32, y: i32, text: &str) {
    if text.is_empty() {
        return;
    }
    WIDE_SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        buf.extend(text.encode_utf16());
        let _ = ExtTextOutW(
            hdc,
            x,
            y,
            ETO_OPTIONS::default(),
            None,
            PCWSTR(buf.as_ptr()),
            buf.len() as u32,
            None,
        );
    });
}

/// SAFETY: same as `text_out`; clip rect is read by-pointer for the
/// duration of the call.
unsafe fn text_out_clipped(hdc: HDC, x: i32, y: i32, text: &str, clip: &RECT) {
    if text.is_empty() {
        return;
    }
    WIDE_SCRATCH.with(|s| {
        let mut buf = s.borrow_mut();
        buf.clear();
        buf.extend(text.encode_utf16());
        let _ = ExtTextOutW(
            hdc,
            x,
            y,
            ETO_CLIPPED,
            Some(clip as *const _),
            PCWSTR(buf.as_ptr()),
            buf.len() as u32,
            None,
        );
    });
}

// =====================================================================
// Image thumbnail decoding + cache.
//
// `decode_to_bgra` normalizes any supported clipboard image format (PNG
// blob from history, BI_RGB 24/32bpp DIB, BI_BITFIELDS 32bpp DIB, with
// any header size — V4 / V5 included) to top-down 32bpp BGRA pixels.
// `build_thumb_bitmap` then halftone-downscales it once into a bitmap at
// the exact display size, so the per-row paint is a plain 1:1 BitBlt.
// =====================================================================

struct CachedThumb {
    hbmp: HBITMAP,
    /// Square display size (px) this bitmap was pre-scaled to. The paint
    /// path BitBlts it 1:1 — no per-paint StretchBlt — so a DPI/layout
    /// change that alters the row height invalidates the entry, which is
    /// then rebuilt at the new size.
    size: i32,
}

impl Drop for CachedThumb {
    fn drop(&mut self) {
        // SAFETY: hbmp came from CreateDIBSection in this module; the
        // bitmap is never selected into a DC outside a single
        // `draw_image_thumbnail` call, which has returned by the time we
        // get here (the cache only releases entries on explicit
        // invalidation, never mid-paint).
        unsafe {
            let _ = DeleteObject(self.hbmp);
        }
    }
}

thread_local! {
    static THUMB_CACHE: RefCell<HashMap<u64, CachedThumb>> = RefCell::new(HashMap::new());
}

/// Drop the cached bitmap for a single history item. Call when an item
/// is removed from HISTORY so its GDI handle is released promptly.
pub(crate) fn invalidate_thumb_cache(id: u64) {
    THUMB_CACHE.with(|c| {
        c.borrow_mut().remove(&id);
    });
}

/// Drop every cached bitmap. Call on full history clear.
pub(crate) fn clear_thumb_cache() {
    THUMB_CACHE.with(|c| c.borrow_mut().clear());
}

/// Drop cache entries whose id is no longer present in `live_ids`.
/// `prune_history` silently drops old items to stay under MAX_ITEMS, so
/// without this sweep the cache would slowly accumulate orphan HBITMAPs
/// across long sessions.
pub(crate) fn sweep_thumb_cache(live_ids: &std::collections::HashSet<u64>) {
    THUMB_CACHE.with(|c| {
        c.borrow_mut().retain(|id, _| live_ids.contains(id));
    });
}

fn decode_to_bgra(raw: &[u8]) -> Option<(Vec<u8>, i32, i32)> {
    if looks_like_png(raw) {
        // png_to_dib emits a 40-byte header + top-down 32bpp BGRA pixels,
        // so we can hand back the body directly.
        let dib = png_to_dib(raw)?;
        if dib.len() < 40 {
            return None;
        }
        let w = i32::from_le_bytes(dib[4..8].try_into().ok()?);
        let h = i32::from_le_bytes(dib[8..12].try_into().ok()?).abs();
        if w <= 0 || h <= 0 {
            return None;
        }
        let pixels = dib[40..].to_vec();
        return Some((pixels, w, h));
    }
    decode_dib_to_bgra(raw)
}

/// Create a top-down 32bpp BGRA DIB section of `w`×`h`. Returns the
/// bitmap handle and a pointer to its pixel store. The caller owns the
/// returned HBITMAP.
///
/// SAFETY: caller passes a screen-compatible HDC; on failure any
/// partially-created handle is released before returning None.
unsafe fn create_bgra_dib(hdc: HDC, w: i32, h: i32) -> Option<(HBITMAP, *mut u8)> {
    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = w;
    // Negative height = top-down, matching what `decode_to_bgra` produced.
    bmi.bmiHeader.biHeight = -h;
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(
        hdc,
        &bmi,
        DIB_RGB_COLORS,
        &mut bits,
        HANDLE(std::ptr::null_mut()),
        0,
    )
    .ok()?;
    if bits.is_null() || hbmp.0.is_null() {
        if !hbmp.0.is_null() {
            let _ = DeleteObject(hbmp);
        }
        return None;
    }
    Some((hbmp, bits as *mut u8))
}

/// Decode `raw`, then halftone-downscale it ONCE into a square
/// `size`×`size` bitmap the paint path can BitBlt 1:1. Previously the
/// cache held the image at its native resolution and every repaint ran a
/// `StretchBlt` with `STRETCH_HALFTONE` (GDI's slowest mode) — re-resampled
/// per scroll tick for each visible image row. Paying the resample once at
/// cache-build time keeps the halftone quality and makes paint a plain blit.
///
/// SAFETY: caller passes a screen-compatible HDC. Two DIB sections and two
/// memory DCs are created; every selection is restored and every handle
/// freed on each exit path, except the scaled destination bitmap, which is
/// handed to the cache and freed by `CachedThumb::drop`.
unsafe fn build_thumb_bitmap(hdc: HDC, raw: &[u8], size: i32) -> Option<CachedThumb> {
    if size <= 0 {
        return None;
    }
    let (pixels, w, h) = decode_to_bgra(raw)?;
    if w <= 0 || h <= 0 {
        return None;
    }

    // Source bitmap at the image's native thumbnail resolution.
    let (src_bmp, src_bits) = create_bgra_dib(hdc, w, h)?;
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), src_bits, pixels.len());
    // CreateDIBSection writes are buffered in the GDI batch; flush before
    // the stretch reads them through a memory DC.
    let _ = GdiFlush();

    // Destination bitmap at the exact square display size.
    let Some((dst_bmp, _dst_bits)) = create_bgra_dib(hdc, size, size) else {
        let _ = DeleteObject(src_bmp);
        return None;
    };

    let src_dc = CreateCompatibleDC(hdc);
    let dst_dc = CreateCompatibleDC(hdc);
    if src_dc.0.is_null() || dst_dc.0.is_null() {
        if !src_dc.0.is_null() {
            let _ = DeleteDC(src_dc);
        }
        if !dst_dc.0.is_null() {
            let _ = DeleteDC(dst_dc);
        }
        let _ = DeleteObject(src_bmp);
        let _ = DeleteObject(dst_bmp);
        return None;
    }
    let prev_src = SelectObject(src_dc, src_bmp);
    let prev_dst = SelectObject(dst_dc, dst_bmp);
    let prev_mode = SetStretchBltMode(dst_dc, STRETCH_HALFTONE);
    // STRETCH_HALFTONE requires re-anchoring the brush origin per Win32 docs.
    let mut prev_org = POINT::default();
    let _ = SetBrushOrgEx(dst_dc, 0, 0, Some(&mut prev_org));
    let _ = StretchBlt(dst_dc, 0, 0, size, size, src_dc, 0, 0, w, h, SRCCOPY);
    let _ = SetBrushOrgEx(dst_dc, prev_org.x, prev_org.y, None);
    let _ = SetStretchBltMode(dst_dc, STRETCH_BLT_MODE(prev_mode));
    let _ = GdiFlush();

    // Restore selections, then tear down everything but the dest bitmap.
    let _ = SelectObject(src_dc, prev_src);
    let _ = SelectObject(dst_dc, prev_dst);
    let _ = DeleteDC(src_dc);
    let _ = DeleteDC(dst_dc);
    let _ = DeleteObject(src_bmp);

    Some(CachedThumb { hbmp: dst_bmp, size })
}

/// SAFETY: hdc is the back-buffer DC. The cached HBITMAP is selected into
/// a memory DC that is created and destroyed in this call; the previous
/// GDI selection is restored before DeleteDC.
unsafe fn draw_image_thumbnail(
    hdc: HDC,
    id: u64,
    thumb_file: Option<&str>,
    x: i32,
    y: i32,
    size: i32,
) {
    let hbmp = THUMB_CACHE.with(|cache| {
        if let Some(thumb) = cache.borrow().get(&id) {
            // Reuse only when the cached bitmap was scaled to the size this
            // paint wants; a DPI/layout change falls through to a rebuild.
            if thumb.size == size {
                return Some(thumb.hbmp);
            }
        }
        let path = media_path(thumb_file?)?;
        let bytes = std::fs::read(&path).ok()?;
        let new_thumb = build_thumb_bitmap(hdc, &bytes, size)?;
        let result = new_thumb.hbmp;
        cache.borrow_mut().insert(id, new_thumb);
        Some(result)
    })
    .unwrap_or(HBITMAP(std::ptr::null_mut()));
    if hbmp.0.is_null() {
        // Placeholder block: a flat tinted square so the row geometry
        // still reads as image-shaped.
        let pal = PALETTE.with(|c| c.get());
        let brush = CreateSolidBrush(COLORREF(pal.tag_image));
        if !brush.0.is_null() {
            let rc = RECT { left: x, top: y, right: x + size, bottom: y + size };
            FillRect(hdc, &rc, brush);
            let _ = DeleteObject(brush);
        }
        return;
    }

    // The cached bitmap is already the exact display size (see
    // build_thumb_bitmap), so this is a straight 1:1 blit — no stretch.
    let mem_dc = CreateCompatibleDC(hdc);
    if mem_dc.0.is_null() {
        return;
    }
    let prev_obj = SelectObject(mem_dc, hbmp);
    let _ = BitBlt(hdc, x, y, size, size, mem_dc, 0, 0, SRCCOPY);
    let _ = SelectObject(mem_dc, prev_obj);
    let _ = DeleteDC(mem_dc);
}

// =====================================================================
// Cached per-font metrics.
// =====================================================================

/// Per-font invariants reused across every painted row.
struct RowMetrics {
    /// The listbox font handle these were measured with; when it changes
    /// (theme / DPI) the cache is rebuilt.
    font: HFONT,
    tm_height: i32,
    glyph_pinned_w: i32,
    glyph_unpinned_w: i32,
}

thread_local! {
    static ROW_METRICS: RefCell<Option<RowMetrics>> = const { RefCell::new(None) };
}

/// Text height and the two pin-glyph widths for the current listbox font.
/// These were a `GetTextMetricsW` plus a glyph `GetTextExtentPoint32W` on
/// every painted row even though they only change when the font changes;
/// caching them keyed on the font handle removes that per-row GDI cost.
///
/// SAFETY: hdc is the back-buffer DC; font is the session-lived listbox
/// font. On a cache miss the font is selected before measuring.
unsafe fn row_metrics(hdc: HDC, font: HFONT) -> (i32, i32, i32) {
    ROW_METRICS.with(|c| {
        if let Some(m) = c.borrow().as_ref() {
            if m.font.0 == font.0 {
                return (m.tm_height, m.glyph_pinned_w, m.glyph_unpinned_w);
            }
        }
        let _ = SelectObject(hdc, font);
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        let glyph_pinned_w = measure_text(hdc, PIN_GLYPH_PINNED);
        let glyph_unpinned_w = measure_text(hdc, PIN_GLYPH_UNPINNED);
        *c.borrow_mut() = Some(RowMetrics {
            font,
            tm_height: tm.tmHeight,
            glyph_pinned_w,
            glyph_unpinned_w,
        });
        (tm.tmHeight, glyph_pinned_w, glyph_unpinned_w)
    })
}

/// Per-paint GDI context shared across every run emitted for one row.
/// Built once before the run-walk loop; passed by reference to keep
/// `emit_preview_run`'s signature small.
struct PreviewRunCtx {
    hdc: HDC,
    text_y: i32,
    bold: HFONT,
    regular: HFONT,
    clip: RECT,
}

/// Render one matched/unmatched run of the preview at `*x`, advancing
/// `*x` past the rendered width.
///
/// SAFETY: ctx.hdc is the back-buffer DC; bold/regular are session-lived
/// HFONTs; ctx.clip lives for the row paint.
unsafe fn emit_preview_run(ctx: &PreviewRunCtx, x: &mut i32, run: &str, is_match: bool) {
    if run.is_empty() {
        return;
    }
    let font = if is_match && !ctx.bold.0.is_null() {
        ctx.bold
    } else {
        ctx.regular
    };
    let _ = SelectObject(ctx.hdc, font);
    text_out_clipped(ctx.hdc, *x, ctx.text_y, run, &ctx.clip);
    *x += measure_text(ctx.hdc, run);
}

// =====================================================================
// Row geometry.
// =====================================================================

/// Recompute `LIST_ROW_TOPS` from the current FILTERED view. Image rows
/// are `TEXT_ITEM_HEIGHT * 3` tall, every other row `TEXT_ITEM_HEIGHT`.
fn rebuild_row_geometry() {
    let tops = FILTERED.with(|f| {
        let filtered = f.borrow();
        HISTORY.with(|h| {
            let hist = h.borrow();
            let mut tops = Vec::with_capacity(filtered.len() + 1);
            let mut y = 0i32;
            tops.push(0);
            for row in filtered.iter() {
                let row_h = match hist.get(row.hist_index).map(|item| &item.kind) {
                    Some(ItemType::Image) => (TEXT_ITEM_HEIGHT * 3) as i32,
                    _ => TEXT_ITEM_HEIGHT as i32,
                };
                y += row_h;
                tops.push(y);
            }
            tops
        })
    });
    LIST_ROW_TOPS.with(|c| *c.borrow_mut() = tops);
}

fn content_height() -> i32 {
    LIST_ROW_TOPS.with(|c| c.borrow().last().copied().unwrap_or(0))
}

fn row_count() -> usize {
    LIST_ROW_TOPS.with(|c| c.borrow().len().saturating_sub(1))
}

/// Top/bottom pixel offsets (content space) of a row.
fn row_bounds(idx: usize) -> Option<(i32, i32)> {
    LIST_ROW_TOPS.with(|c| {
        let tops = c.borrow();
        if idx + 1 < tops.len() {
            Some((tops[idx], tops[idx + 1]))
        } else {
            None
        }
    })
}

/// SAFETY: hwnd is the list window.
unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// Maximum scroll offset such that the last content pixel sits at the
/// bottom edge; 0 when the content fits.
unsafe fn max_scroll(hwnd: HWND) -> i32 {
    let (_, ch) = client_size(hwnd);
    (content_height() - ch).max(0)
}

/// The row under a client-space y coordinate, or None past the content.
fn row_at_content_y(content_y: i32) -> Option<usize> {
    LIST_ROW_TOPS.with(|c| {
        let tops = c.borrow();
        if tops.len() < 2 {
            return None;
        }
        let total = *tops.last().unwrap();
        if content_y < 0 || content_y >= total {
            return None;
        }
        match tops.binary_search(&content_y) {
            Ok(i) => Some(i.min(tops.len() - 2)),
            Err(i) => Some(i - 1),
        }
    })
}

// =====================================================================
// Scrolling: scrollbar sync + eased animation.
// =====================================================================

/// SAFETY: hwnd is the list window; SCROLLINFO is a stack POD.
unsafe fn update_scrollbar(hwnd: HWND) {
    let (_, ch) = client_size(hwnd);
    let total = content_height();
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
        nMin: 0,
        nMax: (total - 1).max(0),
        nPage: ch.max(0) as u32,
        nPos: scroll_y(),
        ..Default::default()
    };
    SetScrollInfo(hwnd, SB_VERT, &si, true);
}

unsafe fn start_anim(hwnd: HWND) {
    if !LIST_ANIM.with(|c| c.get()) {
        LIST_ANIM.with(|c| c.set(true));
        let _ = SetTimer(hwnd, SCROLL_TIMER_ID, SCROLL_TIMER_MS, None);
    }
}

unsafe fn stop_anim(hwnd: HWND) {
    if LIST_ANIM.with(|c| c.get()) {
        LIST_ANIM.with(|c| c.set(false));
        let _ = KillTimer(hwnd, SCROLL_TIMER_ID);
    }
}

/// Animated scroll: set a target and let the frame timer ease toward it.
unsafe fn scroll_to_target(hwnd: HWND, target: i32) {
    let target = target.clamp(0, max_scroll(hwnd));
    set_scroll_target(target);
    if scroll_y() != target {
        start_anim(hwnd);
    } else {
        stop_anim(hwnd);
    }
}

/// Immediate (non-animated) scroll — used for thumb drags and to keep the
/// selection visible without a visible glide.
unsafe fn scroll_immediate(hwnd: HWND, y: i32) {
    let y = y.clamp(0, max_scroll(hwnd));
    stop_anim(hwnd);
    set_scroll_y(y);
    set_scroll_target(y);
    SetScrollPos(hwnd, SB_VERT, y, true);
    let _ = InvalidateRect(hwnd, None, false);
}

/// One easing step toward the target. Closes 1/`SCROLL_EASE_DIV` of the
/// remaining distance per tick (min 1px) so motion decelerates smoothly.
unsafe fn anim_tick(hwnd: HWND) {
    let cur = scroll_y();
    let tgt = scroll_target();
    let diff = tgt - cur;
    if diff == 0 {
        stop_anim(hwnd);
        return;
    }
    let mut step = diff / SCROLL_EASE_DIV;
    if step == 0 {
        step = diff.signum();
    }
    let next = cur + step;
    set_scroll_y(next);
    SetScrollPos(hwnd, SB_VERT, next, true);
    let _ = InvalidateRect(hwnd, None, false);
    if next == tgt {
        stop_anim(hwnd);
    }
}

/// Scroll the minimum amount (immediately) to bring a row fully in view.
unsafe fn ensure_visible(hwnd: HWND, idx: usize) {
    let Some((top, bottom)) = row_bounds(idx) else {
        return;
    };
    let (_, ch) = client_size(hwnd);
    let cur = scroll_y();
    let mut y = cur;
    if top < cur {
        y = top;
    } else if bottom > cur + ch {
        y = bottom - ch;
    }
    if y != cur {
        scroll_immediate(hwnd, y);
    }
}

// =====================================================================
// Public API used by other modules (replaces the old LB_* messages).
// =====================================================================

/// Number of visible rows.
pub(crate) fn list_count() -> i32 {
    row_count() as i32
}

/// Currently selected row index, or -1.
pub(crate) fn list_get_sel() -> i32 {
    sel()
}

/// Select a row (clamped; negative or empty-list clears the selection),
/// scroll it into view, and repaint.
pub(crate) fn list_set_sel(idx: i32) {
    let count = row_count() as i32;
    let new = if count == 0 || idx < 0 {
        -1
    } else {
        idx.min(count - 1)
    };
    LIST_SEL.with(|c| c.set(new));
    let hwnd = LISTBOX.with(|l| *l.borrow());
    if hwnd.0.is_null() {
        return;
    }
    // SAFETY: same-thread Win32 calls on the owned list window.
    unsafe {
        if new >= 0 {
            ensure_visible(hwnd, new as usize);
        }
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// Rebuild row geometry from the current FILTERED view, clamp the
/// selection and scroll offset to the new bounds, refresh the scrollbar,
/// and repaint. Call after FILTERED is replaced.
pub(crate) fn list_rebuild() {
    rebuild_row_geometry();
    let hwnd = LISTBOX.with(|l| *l.borrow());
    if hwnd.0.is_null() {
        return;
    }
    let count = row_count() as i32;
    LIST_SEL.with(|c| {
        if c.get() >= count {
            c.set(count - 1);
        }
    });
    // SAFETY: same-thread Win32 calls on the owned list window.
    unsafe {
        let max = max_scroll(hwnd);
        set_scroll_y(scroll_y().clamp(0, max));
        set_scroll_target(scroll_target().clamp(0, max));
        update_scrollbar(hwnd);
        let _ = InvalidateRect(hwnd, None, false);
    }
}

// =====================================================================
// Owner-draw row rendering with fuzzy-match highlights.
// =====================================================================

/// Paint a single row into `hdc` at `rc` (client/back-buffer coords).
///
/// SAFETY: hdc is the back-buffer DC; hwnd_list is the list window; all
/// GDI handles read from thread-locals are valid for the session.
unsafe fn draw_row(hdc: HDC, hwnd_list: HWND, row_idx: usize, rc: &RECT, selected: bool) {
    let pal = PALETTE.with(|c| c.get());

    // Win11-style row fill: background color when unselected, a flat
    // subtle surface color when selected (no harsh accent-blue highlight).
    let bg_brush = if selected {
        SEL_BRUSH.with(|c| c.get())
    } else {
        BG_BRUSH.with(|c| c.get())
    };
    if !bg_brush.0.is_null() {
        FillRect(hdc, rc, bg_brush);
    }
    SetBkMode(hdc, TRANSPARENT);

    FILTERED.with(|f| {
        let filtered = f.borrow();
        let Some(row) = filtered.get(row_idx) else { return };
        HISTORY.with(|h| {
            let hist = h.borrow();
            let Some(item) = hist.get(row.hist_index) else { return };

            let prefix_owned: Option<String> = match (&item.kind, item.lang.as_deref()) {
                (ItemType::Code, Some(lang)) if !lang.is_empty() => Some(format!("[C:{}]", lang)),
                _ => None,
            };
            let prefix: &str = prefix_owned.as_deref().unwrap_or(item.kind.tag());
            let preview: &str = if item.kind == ItemType::Image {
                ""
            } else {
                item.preview.as_str()
            };
            let suffix = relative_time(item.timestamp);

            let pad: i32 = 12;
            let col_gap: i32 = 10;
            let row_h = rc.bottom - rc.top;

            // The listbox font is the session UI font (set via WM_SETFONT);
            // read the cached handle instead of a per-row WM_GETFONT round
            // trip, falling back to the query if it isn't populated yet.
            let regular = {
                let ff = UI_FONT.with(|c| c.get());
                if ff.0.is_null() {
                    HFONT(SendMessageW(hwnd_list, WM_GETFONT, WPARAM(0), LPARAM(0)).0 as *mut _)
                } else {
                    ff
                }
            };
            let bold = BOLD_FONT.with(|c| c.get());
            // Vertically center the text using the font's (cached) metrics.
            let (tm_height, glyph_pinned_w, glyph_unpinned_w) = row_metrics(hdc, regular);
            let _ = SelectObject(hdc, regular);

            let text_y = rc.top + (row_h - tm_height) / 2;

            // Layout: [pad] tag [gap] (thumb [gap])? preview [gap] time [pin column].
            let tag_x = rc.left + pad;
            let tag_w = measure_text(hdc, prefix);
            let pin_left = rc.right - PIN_AREA_W;
            let time_w = measure_text(hdc, &suffix);
            let has_thumbnail = item.kind == ItemType::Image;
            let thumb_size = (TEXT_ITEM_HEIGHT * 2) as i32;
            let thumb_left = tag_x + tag_w + col_gap;
            let preview_left = if has_thumbnail {
                thumb_left + thumb_size + col_gap
            } else {
                tag_x + tag_w + col_gap
            };
            let time_x = (pin_left - col_gap - time_w).max(preview_left);
            let preview_right = (time_x - col_gap).max(preview_left);
            let preview_clip = RECT {
                left: preview_left,
                top: rc.top,
                right: preview_right,
                bottom: rc.bottom,
            };

            // Tag chip — colored per format.
            SetTextColor(hdc, COLORREF(item.kind.tag_color(&pal)));
            text_out(hdc, tag_x, text_y, prefix);

            if has_thumbnail {
                let thumb_y = rc.top + (row_h - thumb_size) / 2;
                draw_image_thumbnail(
                    hdc,
                    item.id,
                    item.thumb_file.as_deref(),
                    thumb_left,
                    thumb_y,
                    thumb_size,
                );
            }

            // Preview in primary text color, with bold runs for fuzzy-match
            // highlights, clipped to the middle column.
            SetTextColor(hdc, COLORREF(pal.text));
            let mut x = preview_left;
            if !preview.is_empty() {
                if row.indices.is_empty() {
                    text_out_clipped(hdc, x, text_y, preview, &preview_clip);
                } else {
                    let ctx = PreviewRunCtx {
                        hdc,
                        text_y,
                        bold,
                        regular,
                        clip: preview_clip,
                    };
                    let mut idx_iter = row.indices.iter().copied().peekable();
                    let mut run_start: usize = 0;
                    let mut run_match = false;
                    for (byte_i, _ch) in preview.char_indices() {
                        while let Some(&i) = idx_iter.peek() {
                            if i < byte_i {
                                idx_iter.next();
                            } else {
                                break;
                            }
                        }
                        let is_match = matches!(idx_iter.peek(), Some(&i) if i == byte_i);
                        if is_match {
                            idx_iter.next();
                        }
                        if byte_i > run_start && is_match != run_match {
                            emit_preview_run(&ctx, &mut x, &preview[run_start..byte_i], run_match);
                            run_start = byte_i;
                        }
                        run_match = is_match;
                    }
                    if run_start < preview.len() {
                        emit_preview_run(&ctx, &mut x, &preview[run_start..], run_match);
                    }
                    let _ = SelectObject(hdc, regular);
                }
            }

            // Time column — right-anchored before the pin glyph.
            let secondary = if selected { pal.text } else { pal.subtext };
            SetTextColor(hdc, COLORREF(secondary));
            text_out(hdc, time_x, text_y, &suffix);

            // Pin glyph: gold accent when pinned, dim when not.
            let pin_color = if item.pinned {
                pal.accent
            } else if selected {
                pal.text
            } else {
                pal.pin_dim
            };
            SetTextColor(hdc, COLORREF(pin_color));
            let glyph = if item.pinned {
                PIN_GLYPH_PINNED
            } else {
                PIN_GLYPH_UNPINNED
            };
            let glyph_w = if item.pinned {
                glyph_pinned_w
            } else {
                glyph_unpinned_w
            };
            let glyph_x = pin_left + (PIN_AREA_W - glyph_w) / 2;
            text_out(hdc, glyph_x, text_y, glyph);
        });
    });
}

/// Double-buffered paint of every visible row.
///
/// SAFETY: hwnd is the list window; BeginPaint/EndPaint are paired and the
/// memory DC + bitmap are released on every exit path.
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let (cw, ch) = client_size(hwnd);
    if cw <= 0 || ch <= 0 {
        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // Back buffer: paint the whole frame off-screen, then blit once.
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, cw, ch);
    if mem.0.is_null() || bmp.0.is_null() {
        if !bmp.0.is_null() {
            let _ = DeleteObject(bmp);
        }
        if !mem.0.is_null() {
            let _ = DeleteDC(mem);
        }
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let prev = SelectObject(mem, bmp);

    let full = RECT { left: 0, top: 0, right: cw, bottom: ch };
    let bg = BG_BRUSH.with(|c| c.get());
    if !bg.0.is_null() {
        FillRect(mem, &full, bg);
    }

    let scroll = scroll_y();
    let selected = sel();
    let tops = LIST_ROW_TOPS.with(|c| c.borrow().clone());
    if tops.len() >= 2 {
        for i in 0..tops.len() - 1 {
            let top = tops[i] - scroll;
            let bottom = tops[i + 1] - scroll;
            if bottom <= 0 || top >= ch {
                continue;
            }
            let rc = RECT { left: 0, top, right: cw, bottom };
            draw_row(mem, hwnd, i, &rc, i as i32 == selected);
        }
    }

    let _ = BitBlt(hdc, 0, 0, cw, ch, mem, 0, 0, SRCCOPY);
    let _ = SelectObject(mem, prev);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

// =====================================================================
// Per-row operations: pin toggle, copy, delete, context menu.
// =====================================================================

/// SAFETY: lb is a valid list handle; HISTORY mutation is on the UI thread.
pub(crate) unsafe fn toggle_pin_at_row(lb: HWND, row: i32) {
    let Some(hi) = row_to_hist(row) else { return };
    HISTORY.with(|h| {
        let mut hist = h.borrow_mut();
        if let Some(item) = hist.get_mut(hi) {
            item.pinned = !item.pinned;
        }
    });
    HISTORY.with(|h| crate::storage::save_history(&h.borrow()));
    refresh_listbox();
    // Re-select the same item at its new row position.
    let new_row = FILTERED.with(|f| f.borrow().iter().position(|r| r.hist_index == hi));
    if let Some(nr) = new_row {
        list_set_sel(nr as i32);
    }
    let _ = lb;
    let parent = SELF_HWND.with(|c| c.get());
    if !parent.0.is_null() {
        update_tray_tooltip(parent);
    }
}

/// SAFETY: hwnd is the popup window; set_clipboard_from_item handles
/// the OpenClipboard pairing internally.
pub(crate) unsafe fn copy_at_row(hwnd: HWND, row: i32) {
    let Some(hi) = row_to_hist(row) else { return };
    let item = HISTORY.with(|h| h.borrow().get(hi).cloned());
    if let Some(item) = item {
        set_clipboard_from_item(hwnd, &item);
    }
}

/// SAFETY: HISTORY mutation is on the UI thread.
pub(crate) unsafe fn delete_at_row(hwnd: HWND, row: i32) {
    let Some(hi) = row_to_hist(row) else { return };
    let removed = HISTORY.with(|h| {
        let mut hist = h.borrow_mut();
        if hi < hist.len() {
            Some(hist.remove(hi))
        } else {
            None
        }
    });
    if let Some(item) = removed {
        crate::storage::delete_media_for(&item);
        invalidate_thumb_cache(item.id);
    }
    HISTORY.with(|h| crate::storage::save_history(&h.borrow()));
    refresh_listbox();
    update_tray_tooltip(hwnd);
}

/// Build the per-row context menu with state-aware Pin/Unpin label,
/// anchor it at the click point (or row rect for keyboard invocation),
/// and dispatch the chosen command. screen_x/y == -1 signals
/// keyboard-triggered.
///
/// SAFETY: the popup menu is created and destroyed in the same call;
/// the list handle is owned by this thread.
pub(crate) unsafe fn show_row_context_menu(
    hwnd_parent: HWND,
    lb: HWND,
    screen_x: i32,
    screen_y: i32,
) {
    let row = if screen_x == -1 && screen_y == -1 {
        sel()
    } else {
        let mut pt = POINT { x: screen_x, y: screen_y };
        let _ = ScreenToClient(lb, &mut pt);
        match row_at_content_y(pt.y + scroll_y()) {
            Some(r) => r as i32,
            None => return,
        }
    };
    if row < 0 {
        return;
    }
    let Some(hi) = row_to_hist(row) else { return };
    let pinned = HISTORY.with(|h| h.borrow().get(hi).map(|i| i.pinned).unwrap_or(false));

    // Move selection to the target row so subsequent Enter / paste operate
    // on the same item the menu refers to.
    list_set_sel(row);

    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let s_paste = to_wide("Paste");
    let s_pin = to_wide(if pinned { "Unpin" } else { "Pin" });
    let s_copy = to_wide("Copy to clipboard");
    let s_delete = to_wide("Delete this item");
    let _ = AppendMenuW(menu, MF_STRING, IDM_ROW_PASTE as usize, PCWSTR(s_paste.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_ROW_PIN as usize, PCWSTR(s_pin.as_ptr()));
    let _ = AppendMenuW(menu, MF_STRING, IDM_ROW_COPY as usize, PCWSTR(s_copy.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_ROW_DELETE as usize, PCWSTR(s_delete.as_ptr()));

    // Anchor: cursor for mouse, row's bottom-left for keyboard.
    let (ax, ay) = if screen_x == -1 && screen_y == -1 {
        let (top, bottom) = row_bounds(row as usize).unwrap_or((0, 0));
        let _ = top;
        let mut pt = POINT { x: 0, y: bottom - scroll_y() };
        let _ = ClientToScreen(lb, &mut pt);
        (pt.x, pt.y)
    } else {
        (screen_x, screen_y)
    };

    let _ = SetForegroundWindow(hwnd_parent);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        ax,
        ay,
        0,
        hwnd_parent,
        None,
    );
    let _ = PostMessageW(hwnd_parent, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    match cmd.0 as u32 {
        IDM_ROW_PASTE => {
            activate_selected(hwnd_parent);
        }
        IDM_ROW_PIN => toggle_pin_at_row(lb, row),
        IDM_ROW_COPY => copy_at_row(hwnd_parent, row),
        IDM_ROW_DELETE => delete_at_row(hwnd_parent, row),
        _ => {}
    }
}

// =====================================================================
// Window procedure for the custom list control.
// =====================================================================

/// Handle a left-button press: focus the control, then either toggle the
/// pin glyph (clicks in the pin column) or select the clicked row.
unsafe fn on_lbuttondown(hwnd: HWND, lp: LPARAM) {
    let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
    let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
    let _ = SetFocus(hwnd);
    let Some(row) = row_at_content_y(y + scroll_y()) else { return };
    let (cw, _) = client_size(hwnd);
    if x >= cw - PIN_AREA_W {
        toggle_pin_at_row(hwnd, row as i32);
        return;
    }
    list_set_sel(row as i32);
}

/// Translate a WM_VSCROLL command into a scroll movement.
unsafe fn handle_vscroll(hwnd: HWND, wp: WPARAM) {
    let code = (wp.0 & 0xFFFF) as i32;
    let (_, ch) = client_size(hwnd);
    let line = TEXT_ITEM_HEIGHT as i32;
    let tgt = scroll_target();
    if code == SB_LINEUP.0 {
        scroll_to_target(hwnd, tgt - line);
    } else if code == SB_LINEDOWN.0 {
        scroll_to_target(hwnd, tgt + line);
    } else if code == SB_PAGEUP.0 {
        scroll_to_target(hwnd, tgt - ch);
    } else if code == SB_PAGEDOWN.0 {
        scroll_to_target(hwnd, tgt + ch);
    } else if code == SB_TOP.0 {
        scroll_to_target(hwnd, 0);
    } else if code == SB_BOTTOM.0 {
        scroll_to_target(hwnd, max_scroll(hwnd));
    } else if code == SB_THUMBTRACK.0 || code == SB_THUMBPOSITION.0 {
        let mut si = SCROLLINFO {
            cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
            fMask: SIF_TRACKPOS,
            ..Default::default()
        };
        let _ = GetScrollInfo(hwnd, SB_VERT, &mut si);
        scroll_immediate(hwnd, si.nTrackPos);
    }
}

/// SAFETY: signature matches the WNDPROC contract; DefWindowProcW handles
/// every message not consumed here.
unsafe extern "system" fn list_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        // paint() covers the whole client; skip the default erase to kill
        // the erase-then-draw flicker.
        WM_ERASEBKGND => LRESULT(1),
        WM_VSCROLL => {
            handle_vscroll(hwnd, wp);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
            // Positive delta = wheel forward = scroll toward earlier items.
            let dy = -(delta * WHEEL_NOTCH_PX / 120);
            scroll_to_target(hwnd, scroll_target() + dy);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == SCROLL_TIMER_ID => {
            anim_tick(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_lbuttondown(hwnd, lp);
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            // Selection was already set by the preceding WM_LBUTTONDOWN.
            // Activate (paste) unless the double-click landed on the pin
            // column, where the first click already toggled the pin.
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let (cw, _) = client_size(hwnd);
            if x < cw - PIN_AREA_W {
                let parent = SELF_HWND.with(|c| c.get());
                if !parent.0.is_null() {
                    // Mimic the listbox LBN_DBLCLK notification main.rs expects.
                    let wparam = ((2u32 << 16) | LISTBOX_ID as u32) as usize;
                    SendMessageW(parent, WM_COMMAND, WPARAM(wparam), LPARAM(hwnd.0 as isize));
                }
            }
            LRESULT(0)
        }
        WM_CONTEXTMENU => {
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
            let parent = SELF_HWND.with(|c| c.get());
            if !parent.0.is_null() {
                show_row_context_menu(parent, hwnd, x, y);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let max = max_scroll(hwnd);
            set_scroll_y(scroll_y().clamp(0, max));
            set_scroll_target(scroll_target().clamp(0, max));
            update_scrollbar(hwnd);
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_NCDESTROY => {
            stop_anim(hwnd);
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
