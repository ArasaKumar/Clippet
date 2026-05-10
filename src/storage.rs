//! Persistent storage at `%APPDATA%\Clippet\`.
//!
//! - `history.json` — the captured clipboard history (atomic write,
//!   200-item cap; pinned items are preserved past the cap).
//! - `settings.json` — autostart prompt state and the user's last
//!   resized popup size.
//! - `Software\Microsoft\Windows\CurrentVersion\Run` — autostart
//!   registry value (HKCU). Optional; written only if the user opts in
//!   on first launch.

use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Registry::*;

use crate::state::{ClipItem, ItemType, MAX_ITEMS, STORAGE_VERSION};
use crate::util::{debug_log, to_wide};

// ---------------------------------------------------------------------
// Disk schema. Mirrors `ClipItem` but with the binary `raw` field encoded
// as base64 so it can travel through JSON.
// ---------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct StoredFile {
    version: u32,
    items: Vec<StoredItem>,
}

#[derive(Serialize, Deserialize)]
struct StoredItem {
    id: u64,
    #[serde(rename = "type")]
    kind: String,
    content_b64: String,
    preview: String,
    ts: u64,
    #[serde(default)]
    pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lang: Option<String>,
}

fn kind_str(k: &ItemType) -> &'static str {
    match k {
        ItemType::Text => "text",
        ItemType::RichText => "richtext",
        ItemType::Image => "image",
        ItemType::File => "file",
        ItemType::Html => "html",
        ItemType::Spreadsheet => "spreadsheet",
        ItemType::Code => "code",
    }
}

fn str_kind(s: &str) -> Option<ItemType> {
    match s {
        "text" => Some(ItemType::Text),
        "richtext" => Some(ItemType::RichText),
        "image" => Some(ItemType::Image),
        "file" => Some(ItemType::File),
        "html" => Some(ItemType::Html),
        "spreadsheet" => Some(ItemType::Spreadsheet),
        "code" => Some(ItemType::Code),
        _ => None,
    }
}

pub(crate) fn data_dir() -> Option<PathBuf> {
    let mut d = dirs::config_dir()?;
    d.push("Clippet");
    Some(d)
}

fn item_to_stored(c: &ClipItem) -> StoredItem {
    StoredItem {
        id: c.id,
        kind: kind_str(&c.kind).to_string(),
        content_b64: B64.encode(&c.raw),
        preview: c.preview.clone(),
        ts: c.timestamp,
        pinned: c.pinned,
        lang: c.lang.clone(),
    }
}

fn stored_to_item(s: &StoredItem) -> Option<ClipItem> {
    let kind = str_kind(&s.kind)?;
    let raw = B64.decode(s.content_b64.as_bytes()).ok()?;
    Some(ClipItem {
        id: s.id,
        kind,
        raw,
        preview: s.preview.clone(),
        timestamp: s.ts,
        pinned: s.pinned,
        lang: s.lang.clone(),
    })
}

// =====================================================================
// History — load, save, prune.
// =====================================================================

pub(crate) fn load_history() -> (Vec<ClipItem>, u64) {
    let Some(dir) = data_dir() else {
        return (Vec::new(), 1);
    };
    let path = dir.join("history.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return (Vec::new(), 1),
    };
    let stored: StoredFile = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            debug_log(&format!("Clippet: history.json parse failed: {}", e));
            return (Vec::new(), 1);
        }
    };
    if stored.version != STORAGE_VERSION {
        debug_log(&format!(
            "Clippet: history.json version {} != {}, treating as empty",
            stored.version, STORAGE_VERSION
        ));
        return (Vec::new(), 1);
    }
    let mut items: Vec<ClipItem> = Vec::with_capacity(stored.items.len());
    let mut max_id: u64 = 0;
    for s in &stored.items {
        if let Some(it) = stored_to_item(s) {
            if it.id > max_id {
                max_id = it.id;
            }
            items.push(it);
        }
    }
    (items, max_id + 1)
}

pub(crate) fn save_history(items: &[ClipItem]) {
    let Some(dir) = data_dir() else { return };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        debug_log(&format!("Clippet: create_dir_all failed: {}", e));
        return;
    }
    let final_path = dir.join("history.json");
    let tmp = dir.join("history.json.tmp");

    let stored = StoredFile {
        version: STORAGE_VERSION,
        items: items.iter().map(item_to_stored).collect(),
    };
    let json = match serde_json::to_vec(&stored) {
        Ok(j) => j,
        Err(e) => {
            debug_log(&format!("Clippet: serialize failed: {}", e));
            return;
        }
    };

    let write_then_rename = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, &json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    };
    if let Err(e) = write_then_rename() {
        debug_log(&format!("Clippet: save_history failed: {}", e));
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Drop oldest unpinned items until we're under MAX_ITEMS. Pinned items
/// are preserved even if that means staying above the cap.
pub(crate) fn prune_history(items: &mut Vec<ClipItem>) {
    while items.len() > MAX_ITEMS {
        let pos = items.iter().position(|x| !x.pinned);
        match pos {
            Some(i) => {
                items.remove(i);
            }
            None => break,
        }
    }
}

// =====================================================================
// Settings — autostart prompt state + remembered popup size. Same
// atomic-write discipline as history.json.
// =====================================================================

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Settings {
    #[serde(default)]
    pub autostart_prompted: bool,
    #[serde(default)]
    pub autostart_enabled: bool,
    /// Last popup size the user resized to. Optional so existing
    /// settings.json files (no popup_w/popup_h field) deserialize cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup_w: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup_h: Option<i32>,
}

fn settings_path() -> Option<PathBuf> {
    Some(data_dir()?.join("settings.json"))
}

pub(crate) fn load_settings() -> Settings {
    let Some(path) = settings_path() else {
        return Settings::default();
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Settings::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub(crate) fn save_settings(s: &Settings) {
    let Some(dir) = data_dir() else { return };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        debug_log(&format!("Clippet: settings create_dir_all failed: {}", e));
        return;
    }
    let final_path = dir.join("settings.json");
    let tmp = dir.join("settings.json.tmp");
    let json = match serde_json::to_vec_pretty(s) {
        Ok(j) => j,
        Err(e) => {
            debug_log(&format!("Clippet: settings serialize failed: {}", e));
            return;
        }
    };
    let write_then_rename = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, &json)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &final_path)?;
        Ok(())
    };
    if let Err(e) = write_then_rename() {
        debug_log(&format!("Clippet: save_settings failed: {}", e));
        let _ = std::fs::remove_file(&tmp);
    }
}

// =====================================================================
// Autostart — HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
// =====================================================================

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE_NAME: &str = "Clippet";

/// Set HKCU\...\Run\Clippet to the given command line (typically the
/// quoted exe path). Caller decides when to invoke — we only do the
/// registry write here.
///
/// SAFETY: All registry calls are documented Win32 APIs; the wide
/// strings outlive the call and the key is closed on every exit path.
pub(crate) unsafe fn registry_run_set(value: &str) -> windows::core::Result<()> {
    let key_name = to_wide(RUN_KEY);
    let mut hkey: HKEY = HKEY::default();
    RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR(key_name.as_ptr()),
        0,
        KEY_SET_VALUE,
        &mut hkey,
    )
    .ok()?;
    let value_name = to_wide(RUN_VALUE_NAME);
    let value_w = to_wide(value);
    // SAFETY: value_w is a valid u16 buffer; we view it as bytes for REG_SZ.
    let value_bytes =
        std::slice::from_raw_parts(value_w.as_ptr() as *const u8, value_w.len() * 2);
    let r = RegSetValueExW(
        hkey,
        PCWSTR(value_name.as_ptr()),
        0,
        REG_SZ,
        Some(value_bytes),
    );
    let _ = RegCloseKey(hkey);
    r.ok()
}

/// Remove the autostart registry value. An already-absent value is
/// treated as success — that's the desired end state.
///
/// SAFETY: Same Win32 contract as `registry_run_set`.
#[allow(dead_code)]
pub(crate) unsafe fn registry_run_remove() -> windows::core::Result<()> {
    let key_name = to_wide(RUN_KEY);
    let mut hkey: HKEY = HKEY::default();
    RegOpenKeyExW(
        HKEY_CURRENT_USER,
        PCWSTR(key_name.as_ptr()),
        0,
        KEY_SET_VALUE,
        &mut hkey,
    )
    .ok()?;
    let value_name = to_wide(RUN_VALUE_NAME);
    let r = RegDeleteValueW(hkey, PCWSTR(value_name.as_ptr()));
    let _ = RegCloseKey(hkey);
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    r.ok()
}

pub(crate) fn current_exe_quoted() -> Option<String> {
    let p = std::env::current_exe().ok()?;
    p.to_str().map(|s| format!("\"{}\"", s))
}
