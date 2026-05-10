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
    REG, THUMB_MAX_SIZE,
};
use crate::storage::{media_filenames, write_media_atomic};
use crate::util::{
    cf_html_to_plain, debug_log, extract_lang_from_title, first_nonempty_line, fnv1a_64,
    foreground_window_title, now_unix, rtf_to_plain, title_is_ide, truncate_preview,
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
    let (mut pixels, w, h) = decode_dib_to_bgra(dib)?;
    // Shared decoder emits BGRA; PNG encoder wants RGBA — swap in place.
    for px in pixels.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&pixels).ok()?;
    }
    Some(out)
}

/// Decode any supported clipboard DIB (`BI_RGB` 24/32bpp or `BI_BITFIELDS`
/// 32bpp, with any header size ≥ 40 — V4/V5 included) to top-down 32bpp
/// BGRA pixels. Returns `(pixels, width, height)` or `None` if the DIB
/// shape isn't one we can decode.
///
/// Shared between the capture path (`dib_to_png`) and the listbox thumb
/// renderer so both sites stay in sync — historically the listbox decoder
/// supported BI_BITFIELDS but `dib_to_png` didn't, so modern Snipping-Tool
/// captures ended up stored as raw DIB on disk and never got a thumbnail.
pub(crate) fn decode_dib_to_bgra(dib: &[u8]) -> Option<(Vec<u8>, i32, i32)> {
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
    // (CreateDIBSection / png encoder) doesn't need to know which it was.
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

    let content_hash = fnv1a_64(&raw);
    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind: ItemType::File,
        raw,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang: None,
        content_hash,
        media_file: None,
        thumb_file: None,
        media_w: None,
        media_h: None,
    })
}

/// Encode a thumbnail PNG (longest edge ≤ THUMB_MAX_SIZE) from a
/// full-resolution PNG. Returns the encoded bytes on success.
fn encode_thumbnail_png(full_png: &[u8]) -> Option<Vec<u8>> {
    let img = image::load_from_memory_with_format(full_png, image::ImageFormat::Png).ok()?;
    // `thumbnail` does box-then-triangle resampling — fast, good enough
    // for a 96-px UI thumb. Lanczos is overkill for this size.
    let thumb = img.thumbnail(THUMB_MAX_SIZE, THUMB_MAX_SIZE);
    let mut out: Vec<u8> = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// Repair on-disk storage for one image item that may have been written
/// by an earlier build that couldn't encode the source DIB (BI_BITFIELDS
/// was rejected, so the raw DIB ended up on disk with a `.png` extension
/// and no thumbnail). If the media file is already a real PNG and a
/// thumbnail exists, this is a no-op. Otherwise, we convert the media
/// file to PNG and/or generate the missing thumbnail, mutating `item`'s
/// `thumb_file` to point at the freshly-written file.
///
/// Returns `true` when anything on disk or in `item` changed — caller can
/// use that signal to schedule a `save_history` after the load-time sweep.
pub(crate) fn repair_image_storage_if_needed(item: &mut ClipItem) -> bool {
    if item.kind != ItemType::Image {
        return false;
    }
    let Some(media_name) = item.media_file.clone() else {
        return false;
    };
    let Some(media_path) = crate::storage::media_path(&media_name) else {
        return false;
    };
    let bytes = match std::fs::read(&media_path) {
        Ok(b) => b,
        Err(_) => return false,
    };

    let mut changed = false;
    // Normalize the on-disk media to a real PNG so encode_thumbnail_png
    // (which goes through the `image` crate) can read it back. Raw-DIB
    // files written by older builds get rewritten here.
    let png_bytes = if looks_like_png(&bytes) {
        bytes
    } else {
        let Some(png) = dib_to_png(&bytes) else {
            debug_log(&format!(
                "Clippet: repair: id={} media is neither PNG nor decodable DIB",
                item.id
            ));
            return false;
        };
        if let Err(e) = crate::storage::write_media_atomic(&media_name, &png) {
            debug_log(&format!(
                "Clippet: repair: rewrite media {} failed: {}",
                media_name, e
            ));
            return false;
        }
        changed = true;
        png
    };

    // Decide whether the thumbnail needs (re)generating: missing field,
    // or pointed-at file that's gone from disk.
    let thumb_exists = match &item.thumb_file {
        Some(name) => crate::storage::media_path(name)
            .map(|p| p.is_file())
            .unwrap_or(false),
        None => false,
    };
    if !thumb_exists {
        let thumb_name = crate::storage::media_filenames(item.id).1;
        match encode_thumbnail_png(&png_bytes) {
            Some(thumb_bytes) => match crate::storage::write_media_atomic(&thumb_name, &thumb_bytes) {
                Ok(()) => {
                    item.thumb_file = Some(thumb_name);
                    changed = true;
                }
                Err(e) => {
                    debug_log(&format!(
                        "Clippet: repair: write thumb {} failed: {}",
                        thumb_name, e
                    ));
                }
            },
            None => {
                debug_log(&format!(
                    "Clippet: repair: thumbnail encode failed for id={}",
                    item.id
                ));
            }
        }
    }

    changed
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

    // Prefer PNG on disk so history.json stays tiny and the listbox
    // can pull thumbnails without re-decoding the full image. If the
    // DIB shape isn't one we can encode (rare clipboard providers),
    // fall back to writing the raw DIB bytes — paste path detects the
    // form via PNG-signature sniff.
    let png_bytes = dib_to_png(&dib_bytes);
    let (full_bytes, is_png) = match png_bytes {
        Some(b) => (b, true),
        None => (dib_bytes, false),
    };

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (full_name, thumb_name) = media_filenames(id);

    if let Err(e) = write_media_atomic(&full_name, &full_bytes) {
        debug_log(&format!("Clippet: write media {} failed: {}", full_name, e));
        return None;
    }

    // Thumbnail is only feasible when we managed to encode PNG. If we
    // had to fall back to raw DIB, skip the thumb (listbox will render
    // a placeholder block).
    let thumb_file = if is_png {
        match encode_thumbnail_png(&full_bytes) {
            Some(thumb_bytes) => match write_media_atomic(&thumb_name, &thumb_bytes) {
                Ok(()) => Some(thumb_name.clone()),
                Err(e) => {
                    debug_log(&format!(
                        "Clippet: write thumb {} failed: {}",
                        thumb_name, e
                    ));
                    None
                }
            },
            None => {
                debug_log("Clippet: thumbnail encode failed");
                None
            }
        }
    } else {
        None
    };

    let content_hash = fnv1a_64(&full_bytes);

    Some(ClipItem {
        id,
        kind: ItemType::Image,
        raw: Vec::new(),
        preview: format!("[Image {}x{}]", w, h_abs),
        timestamp: now_unix(),
        pinned: false,
        lang: None,
        content_hash,
        media_file: Some(full_name),
        thumb_file,
        media_w: if w > 0 { Some(w as u32) } else { None },
        media_h: if h_abs > 0 { Some(h_abs as u32) } else { None },
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
    let raw = s.as_bytes().to_vec();
    let content_hash = fnv1a_64(&raw);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        raw,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang,
        content_hash,
        media_file: None,
        thumb_file: None,
        media_w: None,
        media_h: None,
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
    let content_hash = fnv1a_64(&bytes);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        raw: bytes,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang,
        content_hash,
        media_file: None,
        thumb_file: None,
        media_w: None,
        media_h: None,
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
    let content_hash = fnv1a_64(&bytes);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind,
        raw: bytes,
        preview,
        timestamp: now_unix(),
        pinned: false,
        lang: None,
        content_hash,
        media_file: None,
        thumb_file: None,
        media_w: None,
        media_h: None,
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
    let content_hash = fnv1a_64(&bytes);

    Some(ClipItem {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        kind: ItemType::Spreadsheet,
        raw: bytes,
        preview: format!("{} rows x {} cols", rows, cols),
        timestamp: now_unix(),
        pinned: false,
        lang: None,
        content_hash,
        media_file: None,
        thumb_file: None,
        media_w: None,
        media_h: None,
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
