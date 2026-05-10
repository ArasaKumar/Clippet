//! Clipboard read pipeline: capture the most informative format
//! available on each WM_CLIPBOARDUPDATE.
//!
//! Priority order: Files > Spreadsheet > RTF > HTML > Image > Unicode
//! text > ANSI text. Each `read_*` helper assumes the clipboard is
//! already open (we open it once in `capture_clipboard` and pair it
//! with a single `CloseClipboard` on every exit path).

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::UI::Shell::*;

use crate::state::{
    ClipItem, ItemType, RegFormats, CF_BITMAP, CF_DIB, CF_HDROP, CF_TEXT, CF_UNICODETEXT, NEXT_ID,
    REG,
};
use crate::util::{
    cf_html_to_plain, extract_lang_from_title, first_nonempty_line, foreground_window_title,
    now_unix, rtf_to_plain, title_is_ide, truncate_preview,
};

// =====================================================================
// DIB <-> PNG. We only handle the common BI_RGB cases (24bpp / 32bpp)
// that real-world sources (Snipping Tool, browsers, Office) actually
// produce. Anything else is stored as raw DIB; the paste path detects
// which form we have by sniffing the PNG signature.
// =====================================================================

const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

pub(crate) fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.len() >= 8 && bytes[..8] == PNG_SIGNATURE
}

pub(crate) fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let bi_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let width = i32::from_le_bytes([dib[4], dib[5], dib[6], dib[7]]);
    let height = i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]);
    let planes = u16::from_le_bytes([dib[12], dib[13]]);
    let bitcount = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);

    if compression != 0 || planes != 1 || (bitcount != 24 && bitcount != 32) {
        return None;
    }
    if bi_size < 40 || width <= 0 || height == 0 {
        return None;
    }
    let w = width as usize;
    let abs_h = height.unsigned_abs() as usize;
    let stride = match bitcount {
        32 => w * 4,
        24 => (w * 3).div_ceil(4) * 4,
        _ => unreachable!(),
    };
    let pixels_offset = bi_size; // 24/32bpp BI_RGB has no color table
    let pixels_size = stride.checked_mul(abs_h)?;
    if dib.len() < pixels_offset + pixels_size {
        return None;
    }
    let pixels = &dib[pixels_offset..pixels_offset + pixels_size];
    let bottom_up = height > 0;

    let mut rgba = Vec::with_capacity(w * abs_h * 4);
    for y in 0..abs_h {
        let src_y = if bottom_up { abs_h - 1 - y } else { y };
        let row = &pixels[src_y * stride..src_y * stride + stride];
        match bitcount {
            32 => {
                for px in row.chunks_exact(4).take(w) {
                    rgba.push(px[2]); // R
                    rgba.push(px[1]); // G
                    rgba.push(px[0]); // B
                    // Alpha is undefined in BI_RGB 32bpp; force opaque
                    // so sources that zero it (most clipboard providers)
                    // don't silently produce a fully-transparent PNG.
                    rgba.push(0xFF);
                }
            }
            24 => {
                for x in 0..w {
                    let i = x * 3;
                    rgba.push(row[i + 2]);
                    rgba.push(row[i + 1]);
                    rgba.push(row[i]);
                    rgba.push(0xFF);
                }
            }
            _ => unreachable!(),
        }
    }

    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, abs_h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&rgba).ok()?;
    }
    Some(out)
}

pub(crate) fn png_to_dib(png_bytes: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = png::Decoder::new(png_bytes);
    // EXPAND collapses sub-byte Grayscale/Indexed to 8-bit, expands palettes
    // to RGB(A), and lifts tRNS chunks into a real alpha channel; STRIP_16
    // narrows 16-bit channels to 8. Without these, the per-color-type arms
    // below would misread packed pixels and 16-bit samples as 8-bit data.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let pixels = &buf[..info.buffer_size()];
    let w = info.width as usize;
    let h = info.height as usize;
    if w == 0 || h == 0 {
        return None;
    }
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }

    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => {
            let mut v = Vec::with_capacity(w * h * 4);
            for px in pixels.chunks_exact(3) {
                v.push(px[0]);
                v.push(px[1]);
                v.push(px[2]);
                v.push(0xFF);
            }
            v
        }
        png::ColorType::Grayscale => {
            let mut v = Vec::with_capacity(w * h * 4);
            for gray in pixels.iter().copied() {
                v.push(gray);
                v.push(gray);
                v.push(gray);
                v.push(0xFF);
            }
            v
        }
        png::ColorType::GrayscaleAlpha => {
            let mut v = Vec::with_capacity(w * h * 4);
            for px in pixels.chunks_exact(2) {
                let gray = px[0];
                let alpha = px[1];
                v.push(gray);
                v.push(gray);
                v.push(gray);
                v.push(alpha);
            }
            v
        }
        // EXPAND turns Indexed into Rgb/Rgba before we get here.
        png::ColorType::Indexed => return None,
    };

    let mut dib = Vec::with_capacity(40 + w * h * 4);
    dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
    dib.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    dib.extend_from_slice(&(-(h as i32)).to_le_bytes()); // biHeight (negative = top-down)
    dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    dib.extend_from_slice(&((w * h * 4) as u32).to_le_bytes()); // biSizeImage
    dib.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    dib.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    dib.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    for px in rgba.chunks_exact(4) {
        dib.push(px[2]); // B
        dib.push(px[1]); // G
        dib.push(px[0]); // R
        dib.push(px[3]); // A
    }

    Some(dib)
}

// =====================================================================
// Format-specific readers. Caller (capture_clipboard) holds the clipboard
// open via OpenClipboard for the lifetime of every call here.
// =====================================================================

/// SAFETY: caller must hold the clipboard open via OpenClipboard before
/// this. CF_HDROP returns a global memory handle that is also a valid
/// HDROP.
unsafe fn read_files() -> Option<ClipItem> {
    let h = GetClipboardData(CF_HDROP).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hdrop = HDROP(h.0);
    let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
    let mut files: Vec<String> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let len = DragQueryFileW(hdrop, i, None) as usize;
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
        if got == 0 {
            continue;
        }
        buf.truncate(got);
        files.push(String::from_utf16_lossy(&buf));
    }
    if files.is_empty() {
        return None;
    }

    let raw = files.join("\n").into_bytes();
    let preview = if files.len() == 1 {
        let name = files[0].rsplit(['\\', '/']).next().unwrap_or(&files[0]);
        truncate_preview(name, 80)
    } else {
        let first = files[0].rsplit(['\\', '/']).next().unwrap_or(&files[0]);
        format!("{} + {} more", first, files.len() - 1)
    };

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind: ItemType::File,
        raw,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang: None,
    })
}

/// SAFETY: caller must hold the clipboard open. CF_DIB is a global
/// memory handle whose first bytes are a BITMAPINFOHEADER.
unsafe fn read_image() -> Option<ClipItem> {
    let h = GetClipboardData(CF_DIB).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hg = HGLOBAL(h.0);
    let ptr = GlobalLock(hg);
    if ptr.is_null() {
        return None;
    }
    let size = GlobalSize(hg);
    let dib_bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
    let _ = GlobalUnlock(hg);

    // BITMAPINFOHEADER: biWidth at offset 4 (i32 LE), biHeight at offset 8.
    let (w, h_abs) = if dib_bytes.len() >= 12 {
        let w = i32::from_le_bytes([dib_bytes[4], dib_bytes[5], dib_bytes[6], dib_bytes[7]]);
        let hh = i32::from_le_bytes([dib_bytes[8], dib_bytes[9], dib_bytes[10], dib_bytes[11]]);
        (w, hh.abs())
    } else {
        (0, 0)
    };

    // Prefer PNG storage so history.json stays small. If the DIB is in
    // a shape we don't handle, just keep the raw bytes — paste sniffs
    // the PNG signature to decide which branch to take.
    let raw = dib_to_png(&dib_bytes).unwrap_or(dib_bytes);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind: ItemType::Image,
        raw,
        preview: format!("[Image {}x{}]", w, h_abs),
        timestamp: now_unix(),
        pinned: false,
        lang: None,
    })
}

/// Tag plain text with Code/Text based on the foreground window title.
/// Centralised so unicode and ansi paths share the same heuristic.
fn classify_text(s: &str, title: &Option<String>) -> (ItemType, String, Option<String>) {
    if let Some(t) = title {
        if title_is_ide(t) {
            let lang = extract_lang_from_title(t);
            return (ItemType::Code, first_nonempty_line(s, 80), lang);
        }
    }
    (ItemType::Text, truncate_preview(s, 80), None)
}

/// SAFETY: caller must hold the clipboard open. CF_UNICODETEXT is a
/// null-terminated UTF-16 buffer in global memory.
unsafe fn read_unicode_text() -> Option<ClipItem> {
    let h = GetClipboardData(CF_UNICODETEXT).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hg = HGLOBAL(h.0);
    let ptr = GlobalLock(hg) as *const u16;
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(ptr, len);
    let s = String::from_utf16_lossy(slice);
    let _ = GlobalUnlock(hg);

    let title = foreground_window_title();
    let (kind, preview, lang) = classify_text(&s, &title);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        raw: s.as_bytes().to_vec(),
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang,
    })
}

/// SAFETY: caller must hold the clipboard open. CF_TEXT is null-terminated
/// ANSI bytes in global memory; we treat them as UTF-8 lossy for preview.
unsafe fn read_ansi_text() -> Option<ClipItem> {
    let h = GetClipboardData(CF_TEXT).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hg = HGLOBAL(h.0);
    let ptr = GlobalLock(hg) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    let _ = GlobalUnlock(hg);

    let s = String::from_utf8_lossy(&bytes).into_owned();
    let title = foreground_window_title();
    let (kind, preview, lang) = classify_text(&s, &title);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        raw: bytes,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang,
    })
}

/// SAFETY: caller must hold the clipboard open. Reads a registered
/// global format (RTF / HTML / XML Spreadsheet) as a flat byte buffer.
unsafe fn read_global_bytes(fmt: u32, kind: ItemType, label: &str) -> Option<ClipItem> {
    if fmt == 0 {
        return None;
    }
    let h = GetClipboardData(fmt).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hg = HGLOBAL(h.0);
    let ptr = GlobalLock(hg);
    if ptr.is_null() {
        return None;
    }
    let size = GlobalSize(hg);
    let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
    let _ = GlobalUnlock(hg);

    let text = String::from_utf8_lossy(&bytes);
    let cleaned = match kind {
        ItemType::Html => cf_html_to_plain(&text),
        ItemType::RichText => rtf_to_plain(&text),
        _ => text.into_owned(),
    };
    let preview = {
        let p = truncate_preview(&cleaned, 80);
        if p.is_empty() {
            format!("[{} {}B]", label, bytes.len())
        } else {
            p
        }
    };

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        raw: bytes,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang: None,
    })
}

/// Spreadsheet capture: read CF_TEXT (which spreadsheet apps publish as
/// tab-separated values alongside their proprietary XML format) so that
/// we can produce a "rows x cols" preview and paste into anything that
/// takes plain text. Returns None if the clipboard has no usable CF_TEXT.
///
/// SAFETY: caller must hold the clipboard open.
unsafe fn read_spreadsheet_via_text() -> Option<ClipItem> {
    let h = GetClipboardData(CF_TEXT).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hg = HGLOBAL(h.0);
    let ptr = GlobalLock(hg) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
    let _ = GlobalUnlock(hg);

    let text = String::from_utf8_lossy(&bytes);
    let mut rows = 0usize;
    let mut cols = 0usize;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        rows += 1;
        let c = line.split('\t').count();
        if c > cols {
            cols = c;
        }
    }
    if rows == 0 || cols == 0 {
        return None;
    }

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind: ItemType::Spreadsheet,
        raw: bytes,
        preview: format!("{} rows x {} cols", rows, cols),
        timestamp: now_unix(),
        pinned: false,
        lang: None,
    })
}

/// Open the clipboard, enumerate available formats, dispatch to the
/// most informative reader, and close on every exit path.
///
/// SAFETY: All Win32 calls are documented to operate on the calling
/// thread's clipboard ownership, which we acquire here. OpenClipboard
/// is paired with CloseClipboard before return.
pub(crate) unsafe fn capture_clipboard(hwnd: HWND) -> Option<ClipItem> {
    if OpenClipboard(hwnd).is_err() {
        return None;
    }

    let mut available: HashSet<u32> = HashSet::new();
    let mut fmt: u32 = 0;
    loop {
        fmt = EnumClipboardFormats(fmt);
        if fmt == 0 {
            break;
        }
        available.insert(fmt);
    }

    let reg = REG.with(|r| *r.borrow());

    // Priority order: most informative format wins. Files > Spreadsheet >
    // RTF > HTML > Image > Unicode text > ANSI text.
    //
    // Spreadsheets: when the source publishes the proprietary XML format,
    // we treat that as a *signal* and grab the parallel CF_TEXT (TSV)
    // payload — that's what we want to display ("R rows x C cols") and
    // paste back as. If TSV isn't there, fall back to the raw XML bytes.
    let result = if available.contains(&CF_HDROP) {
        read_files()
    } else if reg.sheet != 0 && available.contains(&reg.sheet) {
        if available.contains(&CF_TEXT) {
            read_spreadsheet_via_text()
                .or_else(|| read_global_bytes(reg.sheet, ItemType::Spreadsheet, "Sheet"))
        } else {
            read_global_bytes(reg.sheet, ItemType::Spreadsheet, "Sheet")
        }
    } else if reg.rtf != 0 && available.contains(&reg.rtf) {
        read_global_bytes(reg.rtf, ItemType::RichText, "RTF")
    } else if reg.html != 0 && available.contains(&reg.html) {
        read_global_bytes(reg.html, ItemType::Html, "HTML")
    } else if available.contains(&CF_DIB) || available.contains(&CF_BITMAP) {
        read_image()
    } else if available.contains(&CF_UNICODETEXT) {
        read_unicode_text()
    } else if available.contains(&CF_TEXT) {
        read_ansi_text()
    } else {
        None
    };

    let _ = CloseClipboard();
    result
}

/// Ask the OS for the IDs of our custom clipboard formats and cache
/// them in the REG thread-local.
///
/// SAFETY: RegisterClipboardFormatW is documented thread-safe.
pub(crate) unsafe fn register_formats() {
    let rtf = RegisterClipboardFormatW(w!("Rich Text Format"));
    let html = RegisterClipboardFormatW(w!("HTML Format"));
    let sheet = RegisterClipboardFormatW(w!("XML Spreadsheet"));
    REG.with(|r| {
        *r.borrow_mut() = RegFormats { rtf, html, sheet };
    });
}
