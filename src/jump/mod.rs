//! Fuzzy jump (NORTHSTAR Core): ⌘J opens a box over the window, typing ranks the folders
//! you've been to by how well they match times how often and how lately you went there
//! (frecency, as zoxide does), and Enter goes to the best one.
//!
//! Frecency is user data, which NORTHSTAR allows to be stored, but it costs nothing on the
//! default path:
//!
//! - **Recording**: each navigation that lands sends its folder to one writer thread (started
//!   by the first one), which appends a line to `history.log` beside the settings file. The UI
//!   thread does no disk I/O, and nothing is read at startup.
//! - **Loading**: only when Jump first opens. The writer reads and aggregates the log; the box
//!   waits at most [`OPEN_WAIT`] for it, as the first window does for its folder, then shows
//!   "Loading…" until it lands. The aggregate then stays in memory for the session (it's the
//!   user's history, not a copy of the file system). Past [`COMPACT_AT`] lines the writer
//!   rewrites the log as one line for each of the top [`KEEP`] folders.
//! - **Folders that are gone** aren't checked: going to one says so and drops it (and the log
//!   records the drop). Nothing stats the whole list.
//! - All history I/O runs on that one thread, in order, so a load sees every visit sent before
//!   it. Every append, load (with its compaction) and clear holds the history's lock file, so
//!   two Arbor processes never interleave them: a compaction can't drop another process's
//!   append, and each writes through a temporary file of its own.
//! - Failures are said: a write that fails shows in the status bar (once, until one works
//!   again), and Clear says "cleared" only when the writer has removed the log (and any
//!   compaction's leftover temporary files). At exit the writer gets up to [`persist::DRAIN_WAIT`] to
//!   finish what's queued.
//!
//! The log: one line per event, `<unix seconds>\t<rank>\t<path>` for a visit (rank 1) or a
//! compacted folder, `<unix seconds>\t-\t<path>` for a drop. A last line without its newline
//! (a write cut short) is ignored, and the next append ends it with a NUL first, so it stays
//! invalid. The log is read a line at a time, however long. Paths that aren't UTF-8 or hold a
//! line break aren't recorded.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use rune::{Key, KeyEvent, TextEdit, Tx};

use crate::app::{Nav, State};
use crate::commands::Cmd;
use crate::fs;
use crate::pattern::{Case, Glob};
use crate::persist;

mod frecency;
mod history;

pub use frecency::*;
pub use history::*;

/// How long opening Jump waits for the history to load before showing "Loading…": one frame
/// at 120 Hz, as the first window waits for its folder.
pub const OPEN_WAIT: Duration = Duration::from_millis(8);
/// The log is compacted when it has more lines than this…
pub const COMPACT_AT: usize = 10_000;
/// …down to one line for each of this many folders, the highest frecency first.
pub const KEEP: usize = 5_000;
/// When the ranks add up to more than this, they all shrink to 90% of it and folders under
/// rank 1 go (zoxide's aging), so old habits fade.
pub const MAX_AGE: f64 = 10_000.0;
/// Rows the box ranks and shows at most: far more than anyone scrolls through, and each key
/// that changes how many there are makes Rune read every row's key again.
pub const LIMIT: usize = 200;
/// Rows ↑↓ skip with Page Up / Page Down.
const PAGE: usize = 8;

const HOUR: u64 = 3600;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

// ---- the box -------------------------------------------------------------------------------

pub struct JumpBox {
    pub edit: TextEdit,
    /// The rows for `hits_for`.
    pub hits: Vec<Hit>,
    hits_for: Option<String>,
    /// The words the rows matched, folded, for their marks.
    pub words: Vec<Vec<char>>,
    /// Every folder the last query matched (not only the rows shown), and the history's
    /// generation then: a query that narrows it ranks among these alone.
    matched: Option<(String, u64, Vec<u32>)>,
    /// The cursor row, for the query `sel_for`; a new query starts it over at the top.
    sel: usize,
    sel_for: String,
    /// Waiting for the history to load.
    pub loading: bool,
}

impl JumpBox {
    /// Whether what's typed is a path (go-to) rather than words to match.
    pub fn path_mode(&self) -> bool {
        crate::app::is_path_like(self.edit.text.trim_start())
    }

    pub fn sel(&self) -> usize {
        if self.sel_for == self.edit.text { self.sel.min(self.hits.len().saturating_sub(1)) } else { 0 }
    }
}

impl State {
    pub fn open_jump(&mut self, tx: &mut Tx) {
        self.close_overlays(tx);
        let loaded = self.history.load(OPEN_WAIT, self.waker.clone());
        if let Some(e) = self.history.error.take() {
            self.note = Some(e);
        }
        let mut edit = TextEdit::new("");
        edit.focused = true;
        self.jump = Some(JumpBox {
            edit,
            hits: Vec::new(),
            hits_for: None,
            words: Vec::new(),
            matched: None,
            sel: 0,
            sel_for: String::new(),
            loading: !loaded,
        });
        self.jump_refresh();
    }

    pub fn close_jump(&mut self, tx: &mut Tx) {
        if self.jump.take().is_some() {
            tx.blur();
        }
    }

    /// Ranks again if the query changed or the history landed since.
    pub fn jump_refresh(&mut self) {
        let Some(j) = &mut self.jump else { return };
        j.loading = self.history.agg.is_none();
        if j.hits_for.as_deref() == Some(j.edit.text.as_str()) {
            return;
        }
        let q = j.edit.text.trim().to_string();
        let now = now();
        j.words = words(&q);
        j.hits = if crate::app::is_path_like(&q) {
            j.matched = None;
            let typed = fs::expand(&q);
            let mut v = vec![Hit { path: typed.as_path().into(), shown: q.as_str().into(), low: None, score: f64::INFINITY }];
            if let Some(a) = &self.history.agg {
                v.extend(a.under(&typed, now, LIMIT).into_iter().filter(|h| *h.path != *typed));
            }
            v
        } else if let Some(a) = &self.history.agg {
            // Typing on narrows what matched before: only those can match now.
            let among = j.matched.as_ref().filter(|(old, g, _)| *g == a.generation() && narrows(old, &q)).map(|(_, _, ix)| ix.as_slice());
            let (hits, matched) = a.rank_in(&q, now, LIMIT, among);
            j.matched = Some((q.clone(), a.generation(), matched));
            hits
        } else {
            Vec::new()
        };
        j.hits_for = Some(j.edit.text.clone());
    }

    /// The field's binding (late writes after it closes go to a scratch field).
    pub fn jump_edit(&mut self) -> &mut TextEdit {
        match &mut self.jump {
            Some(j) => &mut j.edit,
            None => &mut self.palette_sink,
        }
    }

    /// The field's keys, before it edits: ↑↓ and Page Up/Down move, Enter goes, Esc closes.
    pub fn jump_field_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        let m = ev.mods;
        if m.cmd || m.ctrl || m.alt {
            return false;
        }
        match ev.key {
            Key::Up => self.jump_move(-1, tx),
            Key::Down => self.jump_move(1, tx),
            Key::PageUp => self.jump_move(-(PAGE as isize), tx),
            Key::PageDown => self.jump_move(PAGE as isize, tx),
            Key::Enter => {
                let sel = self.jump.as_ref().map_or(0, JumpBox::sel);
                self.jump_pick(sel, tx);
            }
            Key::Escape => self.close_jump(tx),
            _ => return false,
        }
        true
    }

    /// The window's keys while the box is open (see `palette_key`): Jump's own chord closes it.
    pub fn jump_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        tx.instant();
        match self.keymap.find(|c| c.matches_tx(ev, tx)) {
            Some(Cmd::Jump) => {
                self.close_jump(tx);
                return true;
            }
            Some(Cmd::Paste) => {
                tx.request_paste();
                return true;
            }
            _ => {}
        }
        if self.jump_field_key(ev, tx) {
            return true;
        }
        if let (Key::Text(t), Some(j)) = (&ev.key, &mut self.jump)
            && !ev.mods.cmd
            && !ev.mods.ctrl
        {
            j.edit.insert(t);
            j.edit.focused = true;
            self.jump_changed(tx);
        }
        true
    }

    /// After the query changed: rank again and show the top row.
    pub fn jump_changed(&mut self, tx: &mut Tx) {
        self.jump_refresh();
        if self.jump.as_ref().is_some_and(|j| !j.hits.is_empty()) {
            tx.scroll_into_view_in("jump-list", row_key(0));
        }
    }

    fn jump_move(&mut self, by: isize, tx: &mut Tx) {
        let Some(j) = &mut self.jump else { return };
        if j.hits.is_empty() {
            return;
        }
        let to = (j.sel() as isize + by).clamp(0, j.hits.len() as isize - 1) as usize;
        j.sel = to;
        j.sel_for = j.edit.text.clone();
        tx.scroll_into_view_in("jump-list", row_key(to));
    }

    /// Enter or a click on row `i`: go there.
    pub fn jump_pick(&mut self, i: usize, tx: &mut Tx) {
        tx.instant();
        let Some(j) = &self.jump else { return };
        let Some(hit) = j.hits.get(i) else { return };
        let (path, typed) = (hit.path.to_path_buf(), j.path_mode() && i == 0);
        self.close_jump(tx);
        if typed {
            // Go-to, as typing `/` or `~` in the folder does.
            self.go_path(&path);
            return;
        }
        if path == self.cur().cwd {
            return;
        }
        self.go(path, Nav::New, None);
        self.jump_req = self.cur().pending.as_ref().map(|p| p.req);
    }

    /// A Jump to a folder that isn't there any more: say so and forget it.
    pub(crate) fn jump_missed(&mut self, path: &Path) {
        self.history.forget(path);
        self.note = Some(format!("{} no longer exists, so it was removed from Jump", fs::pretty(path)));
    }

    /// Called from `poll`: the history landed, a clear was answered, or a write failed. True
    /// when the view changes.
    pub(crate) fn poll_history(&mut self) -> bool {
        let mut said = false;
        if let Some(r) = self.history.cleared() {
            self.note = Some(match r {
                Ok(()) => "Jump history cleared".into(),
                Err(e) => format!("Couldn't clear the Jump history, so it's still there: {e}"),
            });
            said = true;
        }
        if let Some(e) = self.history.write_error() {
            self.note = Some(e);
            said = true;
        }
        if !self.history.poll() {
            return said;
        }
        if let Some(e) = self.history.error.take() {
            self.note = Some(e);
        }
        if let Some(j) = &mut self.jump {
            j.hits_for = None;
        }
        self.jump_refresh();
        true
    }

    /// Forgets the whole history: run only from the confirmation dialog (`Cmd::permanent`). It
    /// says "cleared" only once the writer has removed it from the disk (`poll_history`).
    pub(crate) fn clear_jump_history(&mut self) {
        self.history.clear();
        self.note = Some("Clearing Jump history…".into());
    }
}

/// A row's key in the box's list.
pub fn row_key(i: usize) -> String {
    format!("jr-{i}")
}

/// The case `jump.exclude` patterns match paths with: the platform's.
pub fn exclude_globs(patterns: &[String]) -> Vec<Glob> {
    patterns.iter().filter_map(|p| Glob::path(p, Case::platform()).ok()).collect()
}

#[cfg(test)]
mod tests;
