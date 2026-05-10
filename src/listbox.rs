//! Listbox + search-edit control: creation, owner-draw rendering with
//! fuzzy-match bold runs, the subclass that intercepts pin clicks and
//! per-row context menus, and the pin/copy/delete row operations.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::clipboard::{looks_like_png, png_to_dib};
use crate::paste::{activate_selected, set_clipboard_from_item};
use crate::search::{drawable_line, refresh_listbox, row_to_hist};
use crate::state::{
    BG_BRUSH, BOLD_FONT, EDIT_ID, ES_AUTOHSCROLL_BIT, ES_LEFT_BIT, FILTERED, HISTORY, IDM_ROW_COPY,
    IDM_ROW_DELETE, IDM_ROW_PASTE, IDM_ROW_PIN,
    ItemType, LBS_HASSTRINGS_BIT, LBS_NOINTEGRALHEIGHT_BIT, LBS_NOTIFY_BIT,
    LBS_OWNERDRAWVARIABLE_BIT, LBS_WANTKEYBOARDINPUT_BIT, LISTBOX, LISTBOX_ID,
    LISTBOX_SUBCLASS_ID, ODS_SELECTED_BIT, PALETTE, PIN_AREA_W, PIN_GLYPH_PINNED,
    PIN_GLYPH_UNPINNED, SEARCH, SEARCH_HEIGHT, SELF_HWND, SEL_BRUSH, TEXT_ITEM_HEIGHT,
};
use crate::tray::update_tray_tooltip;
use crate::util::to_wide;

// =====================================================================
// Control creation.
// =====================================================================

/// SAFETY: GetModuleHandleW returns a valid HMODULE; CreateWindowExW
/// uses the LISTBOX system class which is always registered. The
/// resulting HWND is stored in LISTBOX for the rest of the session.
pub(crate) unsafe fn create_listbox(parent: HWND) -> Result<()> {
    let hmod = GetModuleHandleW(None)?;
    let hinst = HINSTANCE(hmod.0);
    // LBS_OWNERDRAWVARIABLE allows different heights per row for images vs text.
    // LBS_HASSTRINGS keeps the listbox storing strings (helpful for screen readers / IME), and
    // we ignore those copies in the owner-draw callback.
    let style = WINDOW_STYLE(
        WS_CHILD.0
            | WS_VISIBLE.0
            | WS_VSCROLL.0
            | LBS_NOTIFY_BIT
            | LBS_HASSTRINGS_BIT
            | LBS_NOINTEGRALHEIGHT_BIT
            | LBS_WANTKEYBOARDINPUT_BIT
            | LBS_OWNERDRAWVARIABLE_BIT,
    );
    let lb = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("LISTBOX"),
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
    // Subclass to intercept pin-column clicks and per-row context menus.
    let _ = SetWindowSubclass(lb, Some(listbox_subclass_proc), LISTBOX_SUBCLASS_ID, 0);
    Ok(())
}

/// SAFETY: see `create_listbox`. The EDIT system class is always
/// registered.
pub(crate) unsafe fn create_search_box(parent: HWND) -> Result<HWND> {
    let hmod = GetModuleHandleW(None)?;
    let hinst = HINSTANCE(hmod.0);
    // No WS_BORDER — the popup paints a tinted surface behind the field
    // via WM_CTLCOLOREDIT, matching the inset-pill look of the mockup.
    let style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | ES_LEFT_BIT | ES_AUTOHSCROLL_BIT,
    );
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

/// Derive a bold version of the listbox's default font so owner-draw
/// can switch fonts for matched-character runs without disturbing the
/// global system font. Returns null on any GDI failure — the draw path
/// handles that by falling back to the regular font (no highlight).
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
// Owner-draw rendering with fuzzy-match highlights.
// =====================================================================

/// SAFETY: GetTextExtentPoint32W reads the wide buffer for `wide.len()`
/// chars; the buffer outlives the call.
unsafe fn measure_text(hdc: HDC, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut size = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &wide, &mut size);
    size.cx
}

/// SAFETY: ExtTextOutW reads the wide buffer for `wide.len()` chars.
unsafe fn text_out(hdc: HDC, x: i32, y: i32, text: &str) {
    if text.is_empty() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let _ = ExtTextOutW(
        hdc,
        x,
        y,
        ETO_OPTIONS::default(),
        None,
        PCWSTR(wide.as_ptr()),
        wide.len() as u32,
        None,
    );
}

/// SAFETY: same as `text_out`; clip rect is read by-pointer for the
/// duration of the call.
unsafe fn text_out_clipped(hdc: HDC, x: i32, y: i32, text: &str, clip: &RECT) {
    if text.is_empty() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let _ = ExtTextOutW(
        hdc,
        x,
        y,
        ETO_CLIPPED,
        Some(clip as *const _),
        PCWSTR(wide.as_ptr()),
        wide.len() as u32,
        None,
    );
}

// =====================================================================
// Image thumbnail decoding + cache.
//
// `decode_to_bgra` normalizes any supported clipboard image format (PNG
// blob from history, BI_RGB 24/32bpp DIB, BI_BITFIELDS 32bpp DIB, with
// any header size — V4 / V5 included) to top-down 32bpp BGRA pixels. The
// per-row paint then lifts those pixels into a cached HBITMAP via
// `get_or_create_thumb`, so subsequent repaints (selection change, scroll,
// hover) just StretchBlt the existing bitmap instead of re-decoding.
// =====================================================================
// =====================================================================

struct CachedThumb {
    hbmp: HBITMAP,
    width: i32,
    height: i32,
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

fn decode_dib_to_bgra(dib: &[u8]) -> Option<(Vec<u8>, i32, i32)> {
    if dib.len() < 40 {
        return None;
    }
    let bi_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if bi_size < 40 || dib.len() < bi_size {
        return None;
    }
    let width = i32::from_le_bytes(dib[4..8].try_into().ok()?);
    let height_signed = i32::from_le_bytes(dib[8..12].try_into().ok()?);
    let planes = u16::from_le_bytes(dib[12..14].try_into().ok()?);
    let bitcount = u16::from_le_bytes(dib[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    if width <= 0 || height_signed == 0 || planes != 1 {
        return None;
    }
    let abs_h = height_signed.unsigned_abs() as usize;
    let w_usize = width as usize;
    // Positive biHeight = bottom-up DIB; we emit top-down so the consumer
    // (CreateDIBSection here) doesn't need to know which it was.
    let bottom_up = height_signed > 0;
    let out_stride = w_usize.checked_mul(4)?;
    let out_size = out_stride.checked_mul(abs_h)?;
    let mut out = vec![0u8; out_size];

    if compression == 3 && bitcount == 32 {
        let masks_offset = bi_size;
        if dib.len() < masks_offset + 12 {
            return None;
        }
        let red_mask =
            u32::from_le_bytes(dib[masks_offset..masks_offset + 4].try_into().ok()?);
        let green_mask =
            u32::from_le_bytes(dib[masks_offset + 4..masks_offset + 8].try_into().ok()?);
        let blue_mask =
            u32::from_le_bytes(dib[masks_offset + 8..masks_offset + 12].try_into().ok()?);
        let pixels_off = masks_offset + 12;
        if dib.len() < pixels_off + out_size {
            return None;
        }
        let pixels = &dib[pixels_off..pixels_off + out_size];

        for y in 0..abs_h {
            let src_y = if bottom_up { abs_h - 1 - y } else { y };
            let src_row = &pixels[src_y * out_stride..src_y * out_stride + out_stride];
            let dst_row = &mut out[y * out_stride..y * out_stride + out_stride];
            for (i, chunk) in src_row.chunks_exact(4).enumerate() {
                let value = u32::from_le_bytes(chunk.try_into().ok()?);
                let r = normalize_bitfield_component(value, red_mask)?;
                let g = normalize_bitfield_component(value, green_mask)?;
                let b = normalize_bitfield_component(value, blue_mask)?;
                let dst = &mut dst_row[i * 4..i * 4 + 4];
                dst[0] = b;
                dst[1] = g;
                dst[2] = r;
                dst[3] = 0xFF;
            }
        }
        return Some((out, width, abs_h as i32));
    }

    if compression != 0 {
        return None;
    }
    if bitcount != 24 && bitcount != 32 {
        return None;
    }

    // Source stride is 4-byte aligned per BMP spec.
    let src_stride = match bitcount {
        32 => w_usize.checked_mul(4)?,
        24 => (w_usize.checked_mul(3)? + 3) & !3usize,
        _ => unreachable!(),
    };
    let src_size = src_stride.checked_mul(abs_h)?;
    if dib.len() < bi_size + src_size {
        return None;
    }
    let pixels = &dib[bi_size..bi_size + src_size];

    for y in 0..abs_h {
        let src_y = if bottom_up { abs_h - 1 - y } else { y };
        let src_row = &pixels[src_y * src_stride..src_y * src_stride + src_stride];
        let dst_row = &mut out[y * out_stride..y * out_stride + out_stride];
        match bitcount {
            32 => {
                for x in 0..w_usize {
                    let s = &src_row[x * 4..x * 4 + 4];
                    let d = &mut dst_row[x * 4..x * 4 + 4];
                    d[0] = s[0];
                    d[1] = s[1];
                    d[2] = s[2];
                    // BI_RGB 32bpp leaves the alpha byte undefined.
                    d[3] = 0xFF;
                }
            }
            24 => {
                for x in 0..w_usize {
                    let s = &src_row[x * 3..x * 3 + 3];
                    let d = &mut dst_row[x * 4..x * 4 + 4];
                    d[0] = s[0];
                    d[1] = s[1];
                    d[2] = s[2];
                    d[3] = 0xFF;
                }
            }
            _ => unreachable!(),
        }
    }
    Some((out, width, abs_h as i32))
}

fn normalize_bitfield_component(value: u32, mask: u32) -> Option<u8> {
    if mask == 0 {
        return Some(0);
    }
    let shift = mask.trailing_zeros();
    let bits = mask >> shift;
    // Reject non-contiguous masks: bits must be of the form 0b0..01..1.
    if bits == 0 || bits & (bits + 1) != 0 {
        return None;
    }
    let raw = (value & mask) >> shift;
    let width = bits.count_ones();
    if width == 8 {
        return Some(raw as u8);
    }
    let max = (1u32 << width) - 1;
    Some(((raw * 255 + (max / 2)) / max) as u8)
}

/// SAFETY: caller passes a screen-compatible HDC; CreateDIBSection +
/// memcpy + DeleteDC are paired on every exit path. The returned HBITMAP
/// is owned by the cache and freed by `CachedThumb::drop`.
unsafe fn build_thumb_bitmap(hdc: HDC, raw: &[u8]) -> Option<CachedThumb> {
    let (pixels, w, h) = decode_to_bgra(raw)?;

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
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());
    // CreateDIBSection writes are buffered in the GDI batch; flush before
    // anyone reads the bitmap via a memory DC.
    let _ = GdiFlush();

    Some(CachedThumb {
        hbmp,
        width: w,
        height: h,
    })
}

/// SAFETY: hdc is the DC supplied by WM_DRAWITEM. The cached HBITMAP is
/// selected into a memory DC that is created and destroyed in this call;
/// the previous GDI selection is restored before DeleteDC.
unsafe fn draw_image_thumbnail(hdc: HDC, id: u64, raw: &[u8], x: i32, y: i32, size: i32) {
    // Produce or look up the cached bitmap. We borrow_mut for the
    // insertion, then drop the borrow before reading so the (very small)
    // chance of re-entry through GDI doesn't double-borrow.
    let (hbmp, src_w, src_h) = THUMB_CACHE.with(|cache| {
        if let Some(thumb) = cache.borrow().get(&id) {
            return Some((thumb.hbmp, thumb.width, thumb.height));
        }
        let new_thumb = build_thumb_bitmap(hdc, raw)?;
        let result = (new_thumb.hbmp, new_thumb.width, new_thumb.height);
        cache.borrow_mut().insert(id, new_thumb);
        Some(result)
    })
    .unwrap_or((HBITMAP(std::ptr::null_mut()), 0, 0));
    if hbmp.0.is_null() || src_w <= 0 || src_h <= 0 {
        return;
    }

    let mem_dc = CreateCompatibleDC(hdc);
    if mem_dc.0.is_null() {
        return;
    }
    let prev_obj = SelectObject(mem_dc, hbmp);
    let prev_mode = SetStretchBltMode(hdc, STRETCH_HALFTONE);
    // STRETCH_HALFTONE requires re-anchoring the brush origin per Win32
    // docs, otherwise the dither pattern can shift between paints.
    let mut prev_org = POINT::default();
    let _ = SetBrushOrgEx(hdc, 0, 0, Some(&mut prev_org));
    let _ = StretchBlt(hdc, x, y, size, size, mem_dc, 0, 0, src_w, src_h, SRCCOPY);
    let _ = SetBrushOrgEx(hdc, prev_org.x, prev_org.y, None);
    let _ = SetStretchBltMode(hdc, STRETCH_BLT_MODE(prev_mode));
    let _ = SelectObject(mem_dc, prev_obj);
    let _ = DeleteDC(mem_dc);
}

/// SAFETY: dis is supplied by the WM_DRAWITEM message and outlives this
/// call; all GDI handles read from thread-locals are valid for the
/// session.
pub(crate) unsafe fn draw_listbox_item(dis: &DRAWITEMSTRUCT) {
    if dis.itemID as i32 == -1 {
        return;
    }
    let row_idx = dis.itemID as usize;

    let selected = (dis.itemState.0 & ODS_SELECTED_BIT) != 0;
    let pal = PALETTE.with(|c| c.get());

    // Win11-style row fill: background color when unselected, a flat
    // subtle surface color when selected (no harsh accent-blue highlight).
    let bg_brush = if selected {
        SEL_BRUSH.with(|c| c.get())
    } else {
        BG_BRUSH.with(|c| c.get())
    };
    if !bg_brush.0.is_null() {
        FillRect(dis.hDC, &dis.rcItem, bg_brush);
    }
    SetBkMode(dis.hDC, TRANSPARENT);

    let row = FILTERED.with(|f| f.borrow().get(row_idx).cloned());
    let Some(row) = row else { return };
    let item = HISTORY.with(|h| h.borrow().get(row.hist_index).cloned());
    let Some(item) = item else { return };

    let mut drw = drawable_line(&item);
    // Image rows already convey the type via the [I] tag and the
    // thumbnail itself; the "[Image WxH]" caption next to them is just
    // visual noise, so drop it from owner-draw (the LB_HASSTRINGS shadow
    // copy used by screen readers / IME still includes it).
    if item.kind == ItemType::Image {
        drw.preview.clear();
    }
    let pad: i32 = 12;
    let col_gap: i32 = 10;
    // Vertically center the text in the row using the selected font's metrics.
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(dis.hDC, &mut tm);
    let row_h = dis.rcItem.bottom - dis.rcItem.top;

    let regular =
        HFONT(SendMessageW(dis.hwndItem, WM_GETFONT, WPARAM(0), LPARAM(0)).0 as *mut _);
    let bold = BOLD_FONT.with(|c| c.get());
    let _ = SelectObject(dis.hDC, regular);

    let text_y = dis.rcItem.top + (row_h - tm.tmHeight) / 2;

    // Layout: [pad] tag [gap] (thumb [gap])? preview [gap] time [pin column].
    // Measure tag + time up front so the preview's clip rect is known
    // before drawing anything.
    let tag_x = dis.rcItem.left + pad;
    let tag_w = measure_text(dis.hDC, &drw.prefix);
    let pin_left = dis.rcItem.right - PIN_AREA_W;
    let time_w = measure_text(dis.hDC, &drw.suffix);
    let has_thumbnail = item.kind == ItemType::Image;
    // Image rows are TEXT_ITEM_HEIGHT * 3 tall (see measure_listbox_item);
    // size the thumbnail at 2/3 of that so it fills the row with padding.
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
        top: dis.rcItem.top,
        right: preview_right,
        bottom: dis.rcItem.bottom,
    };

    // Tag chip — colored per format. Selection keeps the type color so
    // the chip's visual signal survives the selection brush.
    SetTextColor(dis.hDC, COLORREF(item.kind.tag_color(&pal)));
    text_out(dis.hDC, tag_x, text_y, &drw.prefix);

    if has_thumbnail {
        let thumb_y = dis.rcItem.top + (row_h - thumb_size) / 2;
        draw_image_thumbnail(dis.hDC, item.id, &item.raw, thumb_left, thumb_y, thumb_size);
    }

    // Preview in primary text color, with bold runs for fuzzy-match
    // highlights. Clipped to the middle column so a long preview can't
    // bleed under the time / pin columns.
    SetTextColor(dis.hDC, COLORREF(pal.text));
    let mut x = preview_left;
    if row.indices.is_empty() {
        text_out_clipped(dis.hDC, x, text_y, &drw.preview, &preview_clip);
    } else {
        // Split the preview into runs of consecutive matched/unmatched
        // chars (char_indices keeps multi-byte chars aligned with the
        // byte-index set returned by fuzzy_indices).
        let idx_set: std::collections::HashSet<usize> =
            row.indices.iter().copied().collect();
        let mut current_match = false;
        let mut current_str = String::new();
        let mut runs: Vec<(bool, String)> = Vec::new();
        for (byte_i, ch) in drw.preview.char_indices() {
            let is_match = idx_set.contains(&byte_i);
            if !current_str.is_empty() && is_match != current_match {
                runs.push((current_match, std::mem::take(&mut current_str)));
            }
            current_match = is_match;
            current_str.push(ch);
        }
        if !current_str.is_empty() {
            runs.push((current_match, current_str));
        }
        for (is_match, run) in runs {
            let font = if is_match && !bold.0.is_null() {
                bold
            } else {
                regular
            };
            let _ = SelectObject(dis.hDC, font);
            text_out_clipped(dis.hDC, x, text_y, &run, &preview_clip);
            x += measure_text(dis.hDC, &run);
        }
        let _ = SelectObject(dis.hDC, regular);
    }

    // Time column — right-anchored before the pin glyph. On selection
    // we collapse secondary text up to primary so it stays readable.
    let secondary = if selected { pal.text } else { pal.subtext };
    SetTextColor(dis.hDC, COLORREF(secondary));
    text_out(dis.hDC, time_x, text_y, &drw.suffix);

    // Pin glyph: gold accent when pinned, dim when not. Selected rows
    // keep the same hierarchy so the pinned state stays readable.
    let pin_color = if item.pinned {
        pal.accent
    } else if selected {
        pal.text
    } else {
        pal.pin_dim
    };
    SetTextColor(dis.hDC, COLORREF(pin_color));
    let glyph = if item.pinned {
        PIN_GLYPH_PINNED
    } else {
        PIN_GLYPH_UNPINNED
    };
    let glyph_w = measure_text(dis.hDC, glyph);
    let glyph_x = pin_left + (PIN_AREA_W - glyph_w) / 2;
    text_out(dis.hDC, glyph_x, text_y, glyph);

    // Skip DrawFocusRect — Win11 controls don't draw the dotted focus
    // rectangle on selected rows; the selection fill is the focus cue.
}

/// Measure the height for each item based on its type.
/// Images get 3x the text row height for thumbnail display.
///
/// Re-entrancy: this is called synchronously by `LB_ADDSTRING`, which
/// `update_filter` issues while it already holds *immutable* borrows of
/// `FILTERED` and `HISTORY`. The borrows here MUST stay immutable —
/// switching either to `borrow_mut` would panic on the nested borrow.
///
/// SAFETY: pointer is provided by the message and outlives this call.
pub(crate) unsafe fn measure_listbox_item(mis: *mut MEASUREITEMSTRUCT) {
    if mis.is_null() {
        return;
    }

    let item_id = (*mis).itemID as usize;
    let height = if let Some(row) = FILTERED.with(|f| f.borrow().get(item_id).cloned()) {
        if let Some(item) = HISTORY.with(|h| h.borrow().get(row.hist_index).cloned()) {
            match item.kind {
                ItemType::Image => TEXT_ITEM_HEIGHT * 3,
                _ => TEXT_ITEM_HEIGHT,
            }
        } else {
            TEXT_ITEM_HEIGHT
        }
    } else {
        TEXT_ITEM_HEIGHT
    };

    (*mis).itemHeight = height;
}

// =====================================================================
// Per-row operations: pin toggle, copy, delete, context menu.
// =====================================================================

/// SAFETY: lb is a valid listbox handle; HISTORY mutation is on the
/// UI thread.
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
        SendMessageW(lb, LB_SETCURSEL, WPARAM(nr), LPARAM(0));
    }
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
    let removed_id = HISTORY.with(|h| {
        let mut hist = h.borrow_mut();
        if hi < hist.len() {
            Some(hist.remove(hi).id)
        } else {
            None
        }
    });
    if let Some(id) = removed_id {
        invalidate_thumb_cache(id);
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
/// SendMessageW operates on a valid listbox handle.
pub(crate) unsafe fn show_row_context_menu(
    hwnd_parent: HWND,
    lb: HWND,
    screen_x: i32,
    screen_y: i32,
) {
    let row = if screen_x == -1 && screen_y == -1 {
        SendMessageW(lb, LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32
    } else {
        let mut pt = POINT { x: screen_x, y: screen_y };
        let _ = ScreenToClient(lb, &mut pt);
        let lp =
            LPARAM((((pt.y as u32) & 0xFFFF) << 16 | ((pt.x as u32) & 0xFFFF)) as isize);
        let result = SendMessageW(lb, LB_ITEMFROMPOINT, WPARAM(0), lp);
        let outside = ((result.0 >> 16) & 0xFFFF) != 0;
        if outside {
            return;
        }
        (result.0 & 0xFFFF) as i32
    };
    if row < 0 {
        return;
    }
    let Some(hi) = row_to_hist(row) else { return };
    let pinned = HISTORY.with(|h| h.borrow().get(hi).map(|i| i.pinned).unwrap_or(false));

    // Move selection to the right-clicked row so subsequent Enter /
    // paste operates on the same item the menu refers to.
    SendMessageW(lb, LB_SETCURSEL, WPARAM(row as usize), LPARAM(0));

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
    let _ = AppendMenuW(
        menu,
        MF_STRING,
        IDM_ROW_DELETE as usize,
        PCWSTR(s_delete.as_ptr()),
    );

    // Anchor: cursor for mouse, row's bottom-left for keyboard.
    let (ax, ay) = if screen_x == -1 && screen_y == -1 {
        let mut rc = RECT::default();
        SendMessageW(
            lb,
            LB_GETITEMRECT,
            WPARAM(row as usize),
            LPARAM(&mut rc as *mut _ as isize),
        );
        let mut pt = POINT { x: rc.left, y: rc.bottom };
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

/// Subclass the listbox so we can intercept clicks on the pin column
/// and the WM_CONTEXTMENU that opens the per-row menu. Everything else
/// falls through to the listbox's default proc unchanged (selection,
/// scrolling, keyboard navigation, double-click).
///
/// SAFETY: signature matches the SUBCLASSPROC contract; we invoke
/// DefSubclassProc for every message we don't fully handle and remove
/// the subclass on WM_NCDESTROY.
unsafe extern "system" fn listbox_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
            let item_lp =
                LPARAM((((y as u32) & 0xFFFF) << 16 | ((x as u32) & 0xFFFF)) as isize);
            let result = SendMessageW(hwnd, LB_ITEMFROMPOINT, WPARAM(0), item_lp);
            let outside = ((result.0 >> 16) & 0xFFFF) != 0;
            let row = (result.0 & 0xFFFF) as i32;
            if !outside && row >= 0 {
                let mut rect = RECT::default();
                SendMessageW(
                    hwnd,
                    LB_GETITEMRECT,
                    WPARAM(row as usize),
                    LPARAM(&mut rect as *mut _ as isize),
                );
                if x >= rect.right - PIN_AREA_W {
                    toggle_pin_at_row(hwnd, row);
                    // Swallow so the listbox doesn't also process the
                    // click for selection — the pin toggle is the whole
                    // point.
                    return LRESULT(0);
                }
            }
        }
        WM_CONTEXTMENU => {
            let x = (lp.0 as u32 & 0xFFFF) as i16 as i32;
            let y = ((lp.0 as u32 >> 16) & 0xFFFF) as i16 as i32;
            let parent = SELF_HWND.with(|c| c.get());
            if !parent.0.is_null() {
                show_row_context_menu(parent, hwnd, x, y);
            }
            return LRESULT(0);
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(
                hwnd,
                Some(listbox_subclass_proc),
                LISTBOX_SUBCLASS_ID,
            );
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

