//! Small helpers used everywhere: UTF-16 conversion, debug logging,
//! time, text shaping for previews, foreground-window lookups, and a
//! MessageBoxW wrapper.

use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 64-bit FNV-1a of `bytes`. Used as a stable, version-agnostic content
/// fingerprint for consecutive-duplicate suppression — `std`'s default
/// hasher is explicitly not stable across releases, so persisting it on
/// disk would silently re-key after a Rust upgrade.
pub(crate) fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn debug_log(msg: &str) {
    let wide = to_wide(msg);
    // SAFETY: OutputDebugStringW takes a null-terminated UTF-16 PCWSTR;
    // the Vec lives until the call returns.
    unsafe {
        OutputDebugStringW(PCWSTR(wide.as_ptr()));
    }
}

/// Modal MessageBoxW with title/body/flags. Centralises the to_wide
/// boilerplate so callers don't repeat it.
pub(crate) fn show_msgbox(
    hwnd: HWND,
    title: &str,
    body: &str,
    flags: MESSAGEBOX_STYLE,
) -> MESSAGEBOX_RESULT {
    let title_w = to_wide(title);
    let body_w = to_wide(body);
    // SAFETY: title_w/body_w outlive the call; flags is a valid combination.
    unsafe { MessageBoxW(hwnd, PCWSTR(body_w.as_ptr()), PCWSTR(title_w.as_ptr()), flags) }
}

pub(crate) fn truncate_preview(s: &str, max: usize) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '\r' || c == '\n' || c == '\t' { ' ' } else { c })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.chars().count() <= max {
        trimmed
    } else {
        let head: String = trimmed.chars().take(max).collect();
        format!("{head}...")
    }
}

pub(crate) fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth: i32 = 0;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode a CF_HTML clipboard payload to readable plain text.
///
/// CF_HTML begins with a `Key:Value` metadata header (`Version:`,
/// `StartHTML:`, `EndHTML:`, `StartFragment:`, `EndFragment:`,
/// `SourceURL:`) before the actual `<html>...</html>`. We use the
/// `StartFragment` / `EndFragment` byte offsets when present to slice
/// out only the fragment that the source intended to share, then strip
/// tags. Falls back to "skip past the first `<`" when the header is
/// missing or malformed.
pub(crate) fn cf_html_to_plain(s: &str) -> String {
    let body: &str = match (
        parse_cf_html_offset(s, "StartFragment:"),
        parse_cf_html_offset(s, "EndFragment:"),
    ) {
        (Some(start), Some(end)) => {
            let bytes = s.as_bytes();
            let start = align_char_boundary(s, start.min(bytes.len()));
            let end = align_char_boundary(s, end.min(bytes.len()).max(start));
            &s[start..end]
        }
        _ => s.find('<').map(|i| &s[i..]).unwrap_or(s),
    };
    let stripped = strip_html_tags(body);
    decode_html_entities(&stripped)
}

fn parse_cf_html_offset(s: &str, key: &str) -> Option<usize> {
    let i = s.find(key)?;
    let rest = &s[i + key.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<usize>().ok()
}

/// Walk back to the nearest UTF-8 char boundary so that slicing into
/// `&str` is always valid even if a CF_HTML header offset lands mid
/// codepoint.
fn align_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Decode the handful of named/numeric HTML entities common in copied
/// fragments. We don't need a full entity table for a one-line preview.
fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = bytes[i + 1..].iter().position(|&b| b == b';') {
                let name = &s[i + 1..i + 1 + semi];
                let decoded = match name {
                    "amp" => Some('&'),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "quot" => Some('"'),
                    "apos" => Some('\''),
                    "nbsp" => Some(' '),
                    n if n.starts_with("#x") || n.starts_with("#X") => {
                        u32::from_str_radix(&n[2..], 16)
                            .ok()
                            .and_then(char::from_u32)
                    }
                    n if n.starts_with('#') => {
                        n[1..].parse::<u32>().ok().and_then(char::from_u32)
                    }
                    _ => None,
                };
                if let Some(c) = decoded {
                    out.push(c);
                    i += 2 + semi;
                    continue;
                }
            }
        }
        // Fallback: copy this codepoint verbatim.
        let ch_len = s[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&s[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// Strip RTF control words, control symbols, and non-textual destination
/// groups (font tables, color tables, stylesheets, generators, pictures,
/// etc.) leaving only the visible text. Used for the preview line — the
/// original RTF bytes are still kept verbatim so paste preserves
/// formatting.
pub(crate) fn rtf_to_plain(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    let mut depth: i32 = 0;
    let mut skip_to_depth: Option<i32> = None;
    let mut unicode_skip: i32 = 1;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'{' => {
                depth += 1;
                i += 1;
                if skip_to_depth.is_none() {
                    let after = &bytes[i..];
                    let is_skip = after.starts_with(b"\\*")
                        || rtf_starts_with_ctrl(after, b"fonttbl")
                        || rtf_starts_with_ctrl(after, b"colortbl")
                        || rtf_starts_with_ctrl(after, b"stylesheet")
                        || rtf_starts_with_ctrl(after, b"info")
                        || rtf_starts_with_ctrl(after, b"pict")
                        || rtf_starts_with_ctrl(after, b"object")
                        || rtf_starts_with_ctrl(after, b"listtable")
                        || rtf_starts_with_ctrl(after, b"listoverridetable")
                        || rtf_starts_with_ctrl(after, b"rsidtbl")
                        || rtf_starts_with_ctrl(after, b"generator")
                        || rtf_starts_with_ctrl(after, b"themedata")
                        || rtf_starts_with_ctrl(after, b"datastore")
                        || rtf_starts_with_ctrl(after, b"latentstyles")
                        || rtf_starts_with_ctrl(after, b"xmlnstbl")
                        || rtf_starts_with_ctrl(after, b"revtbl");
                    if is_skip {
                        skip_to_depth = Some(depth);
                    }
                }
            }
            b'}' => {
                if let Some(d) = skip_to_depth {
                    if depth <= d {
                        skip_to_depth = None;
                    }
                }
                depth -= 1;
                i += 1;
            }
            _ if skip_to_depth.is_some() => {
                // Inside a skip group, still honor `\{` / `\}` so we don't
                // miscount nesting depth.
                if c == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            b'\\' => {
                if i + 1 >= bytes.len() {
                    break;
                }
                let next = bytes[i + 1];
                match next {
                    b'\\' | b'{' | b'}' => {
                        out.push(next);
                        i += 2;
                    }
                    b'~' => {
                        out.push(b' ');
                        i += 2;
                    }
                    b'-' | b'_' | b'*' | b':' => {
                        i += 2;
                    }
                    b'\r' | b'\n' => {
                        i += 2;
                    }
                    b'\'' => {
                        if i + 3 < bytes.len() {
                            if let (Some(a), Some(b)) =
                                (hex_digit(bytes[i + 2]), hex_digit(bytes[i + 3]))
                            {
                                let byte = (a << 4) | b;
                                if byte < 0x80 {
                                    out.push(byte);
                                }
                            }
                            i += 4;
                        } else {
                            i = bytes.len();
                        }
                    }
                    b if b.is_ascii_alphabetic() => {
                        let start = i + 1;
                        let mut j = start;
                        while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
                            j += 1;
                        }
                        let name = &bytes[start..j];

                        let mut param: Option<i32> = None;
                        let p_start = j;
                        if j < bytes.len() && (bytes[j] == b'-' || bytes[j].is_ascii_digit()) {
                            if bytes[j] == b'-' {
                                j += 1;
                            }
                            while j < bytes.len() && bytes[j].is_ascii_digit() {
                                j += 1;
                            }
                            param = std::str::from_utf8(&bytes[p_start..j])
                                .ok()
                                .and_then(|s| s.parse().ok());
                        }
                        // RTF: a single trailing space delimiter is consumed.
                        if j < bytes.len() && bytes[j] == b' ' {
                            j += 1;
                        }

                        match name {
                            b"par" | b"line" | b"sect" | b"page" => out.push(b'\n'),
                            b"tab" => out.push(b'\t'),
                            b"uc" => {
                                if let Some(p) = param {
                                    if p >= 0 {
                                        unicode_skip = p;
                                    }
                                }
                            }
                            b"u" => {
                                if let Some(p) = param {
                                    let cp = if p < 0 {
                                        (p as i64 + 65536) as u32
                                    } else {
                                        p as u32
                                    };
                                    if let Some(ch) = char::from_u32(cp) {
                                        let mut buf = [0u8; 4];
                                        out.extend_from_slice(
                                            ch.encode_utf8(&mut buf).as_bytes(),
                                        );
                                    }
                                    // Skip the substitute chars that follow \uN.
                                    let mut skipped = 0;
                                    while skipped < unicode_skip
                                        && j < bytes.len()
                                        && bytes[j] != b'\\'
                                        && bytes[j] != b'{'
                                        && bytes[j] != b'}'
                                    {
                                        j += 1;
                                        skipped += 1;
                                    }
                                }
                            }
                            _ => {}
                        }
                        i = j;
                    }
                    _ => {
                        i += 2;
                    }
                }
            }
            b'\r' | b'\n' => {
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn rtf_starts_with_ctrl(after: &[u8], name: &[u8]) -> bool {
    if !after.starts_with(b"\\") || after.len() < 1 + name.len() {
        return false;
    }
    if !after[1..].starts_with(name) {
        return false;
    }
    match after.get(1 + name.len()) {
        None => true,
        Some(b) => !b.is_ascii_alphabetic(),
    }
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

pub(crate) fn relative_time(ts: u64) -> String {
    let elapsed = now_unix().saturating_sub(ts);
    if elapsed < 5 {
        "just now".to_string()
    } else if elapsed < 60 {
        format!("{}s ago", elapsed)
    } else if elapsed < 3600 {
        format!("{}m ago", elapsed / 60)
    } else if elapsed < 86400 {
        format!("{}h ago", elapsed / 3600)
    } else {
        format!("{}d ago", elapsed / 86400)
    }
}

pub(crate) fn first_nonempty_line(s: &str, max: usize) -> String {
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().count() <= max {
            return trimmed.to_string();
        }
        let head: String = trimmed.chars().take(max).collect();
        return format!("{head}...");
    }
    String::new()
}

// =====================================================================
// Foreground-window helpers (used by clipboard.rs to detect IDE copies).
// =====================================================================

/// Return the title of the foreground window, or None if no window has
/// focus or the title is empty.
pub(crate) fn foreground_window_title() -> Option<String> {
    // SAFETY: GetForegroundWindow / GetWindowTextLengthW / GetWindowTextW
    // are documented thread-safe Win32 APIs; the buffer outlives the call.
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return None;
        }
        buf.truncate(copied as usize);
        Some(String::from_utf16_lossy(&buf))
    }
}

pub(crate) fn title_is_ide(title: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "Visual Studio Code",
        "JetBrains",
        "IntelliJ",
        "PyCharm",
        "Rider",
        "GoLand",
        "WebStorm",
        "Notepad++",
        "Microsoft Visual Studio",
    ];
    NEEDLES.iter().any(|n| title.contains(n))
}

/// Find the LAST `.<ext>` token in the title where ext is 1-6 alphanumeric
/// chars. IDE titles like "main.rs - clippet - Visual Studio Code" yield
/// "rs"; "filename.spec.ts - Project" yields "ts"; titles without an
/// extension yield None. Lower-cased so it round-trips uniformly.
pub(crate) fn extract_lang_from_title(title: &str) -> Option<String> {
    let bytes = title.as_bytes();
    let mut last: Option<String> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 && j - i - 1 <= 6 {
                last = Some(title[i + 1..j].to_ascii_lowercase());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    last
}
