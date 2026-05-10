//! Filter pipeline + history mutators.
//!
//! `update_filter` rebuilds `FILTERED` (the visible-row mapping) from
//! `HISTORY` based on the current search query, sorts pinned-first
//! then by score (or recency), and re-publishes the listbox contents.
//! `push_item`, `refresh_listbox`, and `row_to_hist` are the small
//! helpers other modules use to interact with the filtered view.

use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::listbox::sweep_thumb_cache;
use crate::state::{ClipItem, FilterRow, ItemType, FILTERED, HISTORY, LISTBOX, SEARCH};
use crate::storage::save_history;
use crate::storage::prune_history;
use crate::util::{relative_time, to_wide};

/// Tag + preview + relative time, used for the LB_HASSTRINGS shadow copy
/// (screen readers / IME). The owner-draw renderer lays the same fields
/// out as colored columns and ignores this string.
pub(crate) fn format_item_line(item: &ClipItem) -> String {
    let kind_label: String = match (&item.kind, item.lang.as_deref()) {
        (ItemType::Code, Some(lang)) if !lang.is_empty() => format!("[C:{}]", lang),
        _ => item.kind.tag().to_string(),
    };
    format!(
        "{}  {}  {}",
        kind_label,
        item.preview,
        relative_time(item.timestamp)
    )
}

/// Append a freshly captured item, dedupe against the previous entry,
/// prune to MAX_ITEMS (preserving pins), and persist. Returns true if
/// the item was actually added (callers use that to gate refresh).
pub(crate) fn push_item(item: ClipItem) -> bool {
    let pushed = HISTORY.with(|h| {
        let mut hist = h.borrow_mut();
        if let Some(last) = hist.last() {
            if last.kind == item.kind && last.raw == item.raw {
                return false;
            }
        }
        hist.push(item);
        prune_history(&mut hist);
        true
    });
    if pushed {
        HISTORY.with(|h| {
            let hist = h.borrow();
            save_history(&hist);
            let live: std::collections::HashSet<u64> = hist.iter().map(|i| i.id).collect();
            sweep_thumb_cache(&live);
        });
    }
    pushed
}

/// Re-run the filter and republish the listbox. Any rebuild of the
/// visible rows must run through here so the current search query
/// keeps its effect.
pub(crate) fn refresh_listbox() {
    // SAFETY: same-thread Win32 messages on owned controls.
    unsafe {
        update_filter();
    }
}

/// SAFETY: the SEARCH edit handle, when set, is valid for the lifetime
/// of the popup. Reads from the edit control happen on the UI thread.
pub(crate) unsafe fn current_search_query() -> String {
    let edit = SEARCH.with(|c| c.get());
    if edit.0.is_null() {
        return String::new();
    }
    let len = GetWindowTextLengthW(edit);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = GetWindowTextW(edit, &mut buf);
    if n <= 0 {
        return String::new();
    }
    buf.truncate(n as usize);
    String::from_utf16_lossy(&buf)
}

/// Rebuild FILTERED from HISTORY based on the current search query
/// (pinned-first, then score, then recency), then republish the
/// listbox contents. Selects the first row when there's anything
/// to show.
///
/// SAFETY: SendMessageW operates on the listbox handle owned by this
/// thread; thread-locals are accessed only on this thread.
pub(crate) unsafe fn update_filter() {
    use fuzzy_matcher::skim::SkimMatcherV2;
    use fuzzy_matcher::FuzzyMatcher;

    let lb = LISTBOX.with(|lb| *lb.borrow());
    if lb.0.is_null() {
        return;
    }

    let query = current_search_query();
    let trimmed = query.trim();

    let rows: Vec<FilterRow> = HISTORY.with(|h| {
        let hist = h.borrow();
        if trimmed.is_empty() {
            // No query: pinned first, then newest first.
            let mut indices: Vec<usize> = (0..hist.len()).collect();
            indices.sort_by(|&a, &b| {
                let pa = !hist[a].pinned;
                let pb = !hist[b].pinned;
                pa.cmp(&pb).then(b.cmp(&a))
            });
            indices
                .into_iter()
                .map(|i| FilterRow {
                    hist_index: i,
                    indices: Vec::new(),
                })
                .collect()
        } else {
            let matcher = SkimMatcherV2::default();
            let mut scored: Vec<(usize, i64, Vec<usize>)> = Vec::new();
            for (i, item) in hist.iter().enumerate() {
                if let Some((score, idxs)) = matcher.fuzzy_indices(&item.preview, trimmed) {
                    scored.push((i, score, idxs));
                }
            }
            // pinned first, then highest score, then newest.
            scored.sort_by(|a, b| {
                let pa = !hist[a.0].pinned;
                let pb = !hist[b.0].pinned;
                pa.cmp(&pb).then(b.1.cmp(&a.1)).then(b.0.cmp(&a.0))
            });
            scored
                .into_iter()
                .map(|(i, _, idxs)| FilterRow {
                    hist_index: i,
                    indices: idxs,
                })
                .collect()
        }
    });

    FILTERED.with(|f| *f.borrow_mut() = rows);

    SendMessageW(lb, LB_RESETCONTENT, WPARAM(0), LPARAM(0));
    let n_rows = FILTERED.with(|f| f.borrow().len());
    HISTORY.with(|h| {
        let hist = h.borrow();
        FILTERED.with(|f| {
            let rows = f.borrow();
            for row in rows.iter() {
                if let Some(item) = hist.get(row.hist_index) {
                    let line = format_item_line(item);
                    let wide = to_wide(&line);
                    SendMessageW(
                        lb,
                        LB_ADDSTRING,
                        WPARAM(0),
                        LPARAM(wide.as_ptr() as isize),
                    );
                }
            }
        });
    });
    if n_rows > 0 {
        SendMessageW(lb, LB_SETCURSEL, WPARAM(0), LPARAM(0));
    }
}

/// Map a visible-row index (FILTERED order) to its history index.
/// Returns None if the row no longer exists or maps to a stale entry.
pub(crate) fn row_to_hist(row: i32) -> Option<usize> {
    if row < 0 {
        return None;
    }
    FILTERED.with(|f| f.borrow().get(row as usize).map(|r| r.hist_index))
}

pub(crate) fn pinned_count() -> usize {
    HISTORY.with(|h| h.borrow().iter().filter(|i| i.pinned).count())
}
