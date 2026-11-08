//! A folder on screen: the live listing of a pane's visible tab, with its cursor, selection,
//! filter, sort and history. And [`TabMemo`], all a hidden tab keeps.
//!
//! NORTHSTAR bans saved listings, so a hidden tab keeps no entries: only its path, history,
//! selected names, scroll offset, filter and sort. When it's shown again, its folder is read
//! fresh (`State::show_tab`), and the selection and scroll come back once the read lands.

use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use rune::{Latest, Tx};

use crate::fs::{self, Entry, Listing, Sort, SortKey};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Nav {
    New,
    Back,
    Forward,
    Reload,
    /// A hidden tab becoming visible: no history change, its filter kept, its scroll restored.
    Show,
}

pub(crate) struct Pending {
    /// Which request this is ([`next_request`]): only its own answer lands.
    pub req: u64,
    pub path: PathBuf,
    pub nav: Nav,
    /// Names to select once the listing lands; the first gets the cursor.
    pub select: Vec<OsString>,
    /// Start renaming the first selected name once it lands (a new folder).
    pub rename: bool,
    /// Where to scroll the list once it lands ([`Nav::Show`]).
    pub scroll: f32,
}

/// What a hidden tab keeps. No entries: its folder is read again when it's shown.
#[derive(Clone, Debug, PartialEq)]
pub struct TabMemo {
    pub path: PathBuf,
    pub back: Vec<PathBuf>,
    pub fwd: Vec<PathBuf>,
    /// The cursor's name first, then the rest of a multiple selection.
    pub select: Vec<OsString>,
    /// How far the list was scrolled, in points.
    pub scroll: f32,
    pub filter: String,
    pub sort: Sort,
    /// The user chose this tab's sort (see [`Folder::sort_set`]).
    pub sort_set: bool,
    /// It was showing a search for this: showing the tab again searches again (nothing of
    /// the results is kept).
    pub search: Option<String>,
}

/// A finished read, tagged with the request that asked for it and the sort it was sorted by.
pub struct Answer {
    pub req: u64,
    pub sort: Sort,
    pub listing: Listing,
}

/// Every folder's `version` comes from one counter, so a row handler from one folder's frame
/// never matches another folder's version (panes come and go).
static VERSION: AtomicU64 = AtomicU64::new(1);

fn next_version() -> u64 {
    VERSION.fetch_add(1, Relaxed)
}

/// Every read request in the process gets its own number, so an answer can only land for the
/// request that asked: never for a newer one of the same path, another tab or another pane.
static REQUEST: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_request() -> u64 {
    REQUEST.fetch_add(1, Relaxed)
}

pub struct Folder {
    /// The pane this folder shows in; element keys carry it ("list-1") so two panes never share
    /// a scroll viewport, and a pane keeps its scroll when the other one closes.
    pub id: u64,
    pub cwd: PathBuf,
    /// `cwd`'s identity, from the read that listed it (links followed): two panes on one folder
    /// by different paths compare equal by it (`shows`).
    pub dir_id: Option<crate::noclobber::FileId>,
    /// The whole folder, sorted.
    pub entries: Vec<Entry>,
    /// Indices into `entries` that pass the hidden-file and filter rules, in display order.
    pub visible: Vec<u32>,
    /// The cursor: index into `visible`.
    pub sel: usize,
    /// Extra selection by name (⇧-arrows, ⌘-click, ⌘A). Empty means "just the cursor".
    pub marked: HashSet<OsString>,
    pub(crate) anchor: usize,
    /// Typed filter. Starting with `/`, `~` (or a drive on Windows) it's a path to go to instead.
    pub filter: String,
    pub show_hidden: bool,
    pub sort: Sort,
    /// The user chose the sort (a header click or a sort command), even if back to the default,
    /// so settings that land late at startup leave it alone.
    pub sort_set: bool,
    pub back: Vec<PathBuf>,
    pub fwd: Vec<PathBuf>,
    /// Changes whenever `visible` changes, so virtual rows know their keys moved.
    pub version: u64,
    pub now: rune::LocalTime,
    /// Folder row a file drag is hovering.
    pub drop_target: Option<OsString>,
    pub(crate) loader: Latest<Answer>,
    pub(crate) pending: Option<Pending>,
    /// Where the list is scrolled to, written by the list's `on_scroll_offset` (no handler, no
    /// commit), read when the tab is hidden.
    pub(crate) scroll: Rc<Cell<f32>>,
    /// Set to stop the read in flight: when a newer one replaces it, or the tab it's for is
    /// hidden, closed or dropped. Each read gets its own.
    stop: Arc<AtomicBool>,
    /// A search of this folder, while it's on screen (`search.rs`): `entries` are its results,
    /// each `os` a path relative to `cwd`. Dropping it stops its walk.
    pub search: Option<Box<crate::search::Search>>,
}

impl Drop for Folder {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl Folder {
    pub fn new(id: u64, cwd: PathBuf, sort: Sort, show_hidden: bool, waker: Option<rune::Waker>) -> Folder {
        let loader = Latest::default();
        if let Some(w) = waker {
            loader.set_waker(w);
        }
        Folder {
            id,
            cwd,
            dir_id: None,
            entries: Vec::new(),
            visible: Vec::new(),
            sel: 0,
            marked: HashSet::new(),
            anchor: 0,
            filter: String::new(),
            show_hidden,
            sort,
            sort_set: false,
            back: Vec::new(),
            fwd: Vec::new(),
            version: next_version(),
            now: now(),
            drop_target: None,
            loader,
            pending: None,
            scroll: Rc::new(Cell::new(0.0)),
            stop: Arc::default(),
            search: None,
        }
    }

    /// Stops the read in flight (its answer could only be stale now) and arms a flag for the
    /// next one.
    pub(crate) fn stop_read(&mut self) -> Arc<AtomicBool> {
        self.stop.store(true, Relaxed);
        self.stop = Arc::default();
        self.stop.clone()
    }

    pub fn list_key(&self) -> String {
        format!("list-{}", self.id)
    }

    pub fn rows_key(&self) -> String {
        format!("rows-{}", self.id)
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.visible.get(self.sel).map(|&i| &self.entries[i as usize])
    }

    /// Whether row `i` (index into `visible`) is part of the selection.
    pub fn is_selected(&self, i: usize) -> bool {
        if self.marked.is_empty() {
            i == self.sel
        } else {
            self.visible.get(i).is_some_and(|&e| self.marked.contains(&self.entries[e as usize].os))
        }
    }

    /// The selection as entries, in display order.
    pub fn targets(&self) -> Vec<&Entry> {
        if self.marked.is_empty() {
            return self.selected().into_iter().collect();
        }
        self.visible.iter().map(|&i| &self.entries[i as usize]).filter(|e| self.marked.contains(&e.os)).collect()
    }

    pub fn target_paths(&self) -> Vec<PathBuf> {
        self.targets().into_iter().map(|e| self.cwd.join(&e.os)).collect()
    }

    /// The targets as items to delete permanently, each with the identity its listing (or the
    /// search's walk) read, so the delete leaves alone anything put at its path since.
    pub fn doomed(&self) -> Vec<crate::ops::Doomed> {
        self.targets()
            .into_iter()
            .map(|e| crate::ops::Doomed { path: self.cwd.join(&e.os), id: e.id, dir: e.is_dir && !e.is_link })
            .collect()
    }

    /// Files a drag out of row `i` carries: the whole selection if the row is in it, else the row.
    pub fn drag_paths(&self, i: usize) -> Vec<PathBuf> {
        if self.is_selected(i) {
            self.target_paths()
        } else {
            self.visible.get(i).map(|&e| self.cwd.join(&self.entries[e as usize].os)).into_iter().collect()
        }
    }

    /// Whether this folder is the one at `path` (with identity `id`): the same identity when
    /// both are known (a symlink and its target are one folder), else the same path.
    pub fn shows(&self, path: &Path, id: Option<crate::noclobber::FileId>) -> bool {
        match (self.dir_id, id) {
            (Some(a), Some(b)) => a == b,
            _ => self.cwd == path,
        }
    }

    pub fn path_mode(&self) -> bool {
        crate::app::is_path_like(&self.filter)
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }

    /// Asks for a fresh read of `path`; it lands in `State::poll` (a newer request wins).
    pub(crate) fn request(&mut self, path: PathBuf, nav: Nav, select: Vec<OsString>, rename: bool) {
        // Going anywhere ends a search (its walk stops).
        self.search = None;
        let (sort, req, stop) = (self.sort, next_request(), self.stop_read());
        self.pending = Some(Pending { req, path: path.clone(), nav, select, rename, scroll: 0.0 });
        self.loader.request_opt(path, move |p| Some(Answer { req, sort, listing: fs::list_until(p, sort, &stop)? }));
    }

    /// Asks for a fresh read of the folder as it is ([`Nav::Show`]), waiting here at most `wait`
    /// for it: a local folder lands in the same frame, a slow one later through `poll`, with
    /// the path and an empty list on screen until then.
    pub(crate) fn request_now(&mut self, wait: std::time::Duration) -> Option<Answer> {
        let req = self.pending.as_ref()?.req;
        let (path, sort, stop) = (self.cwd.clone(), self.sort, self.stop_read());
        read_waiting(&self.loader, wait, move || Some(Answer { req, sort, listing: fs::list_until(path, sort, &stop)? }))
    }

    /// This tab's lightweight state, to keep while it's hidden.
    pub fn memo(&self) -> TabMemo {
        // Searching: the plain listing's state, and the query to search for again.
        if let Some(s) = &self.search {
            return TabMemo { search: Some(s.edit.text.clone()).filter(|q| !q.trim().is_empty()), ..s.memo.clone() };
        }
        // Hidden again before its read landed: keep what it was going to restore.
        if let Some(p) = self.pending.as_ref().filter(|p| p.nav == Nav::Show) {
            return TabMemo {
                path: self.cwd.clone(),
                back: self.back.clone(),
                fwd: self.fwd.clone(),
                select: p.select.clone(),
                scroll: p.scroll,
                filter: self.filter.clone(),
                sort: self.sort,
                sort_set: self.sort_set,
                search: None,
            };
        }
        let cursor = self.cursor_name();
        let mut select: Vec<OsString> = cursor.clone().into_iter().collect();
        if self.marked.len() > 1 {
            // In display order, so it reads back the same.
            select.extend(self.targets().into_iter().map(|e| e.os.clone()).filter(|n| Some(n) != cursor.as_ref()));
        }
        TabMemo {
            path: self.cwd.clone(),
            back: self.back.clone(),
            fwd: self.fwd.clone(),
            select,
            scroll: self.scroll.get(),
            filter: self.filter.clone(),
            sort: self.sort,
            sort_set: self.sort_set,
            search: None,
        }
    }

    /// Shows a hidden tab: its path, history, filter and sort at once, an empty list until its
    /// fresh read lands (`request_now`), then its selection and scroll.
    pub fn show(&mut self, m: TabMemo) {
        // The tab that was here is hidden now: its read or search, if any, stops.
        self.stop_read();
        self.search = None;
        self.entries = Vec::new();
        self.visible.clear();
        self.marked.clear();
        self.sel = 0;
        self.anchor = 0;
        self.drop_target = None;
        self.bump();
        self.dir_id = None;
        self.cwd = m.path;
        self.back = m.back;
        self.fwd = m.fwd;
        self.filter = m.filter;
        self.sort = m.sort;
        self.sort_set = m.sort_set;
        self.scroll.set(m.scroll);
        self.pending = Some(Pending {
            req: next_request(),
            path: self.cwd.clone(),
            nav: Nav::Show,
            select: m.select,
            rename: false,
            scroll: m.scroll,
        });
    }

    /// The same folder as `other`, as a second pane opened on it: its listing as on screen,
    /// the cursor on the same name, no history or filter.
    pub fn same_as(id: u64, other: &Folder, waker: Option<rune::Waker>) -> Folder {
        let mut f = Folder::new(id, other.cwd.clone(), other.sort, other.show_hidden, waker);
        f.sort_set = other.sort_set;
        f.dir_id = other.dir_id;
        // A search's results stay its own: the new pane reads the folder.
        if other.is_loading() || other.searching() {
            f.request(other.cwd.clone(), Nav::Reload, Vec::new(), false);
        } else {
            f.entries = other.entries.clone();
            f.now = other.now;
            let keep = other.cursor_name();
            f.refilter(keep.as_deref());
            f.anchor = f.sel;
        }
        f
    }

    // ---- selection, sorting, filtering ------------------------------------------------------

    /// Recomputes `visible`. Keeps `select` (by name) as the cursor when it's still shown;
    /// otherwise the first name starting with the filter, else the first row.
    pub fn refilter(&mut self, select: Option<&OsStr>) {
        let q = if self.path_mode() { String::new() } else { self.filter.to_lowercase() };
        let hidden = self.show_hidden;
        // A search's list holds only the results it has taken in so far (`search.rs`).
        let n = self.search.as_ref().map_or(self.entries.len(), |s| s.upto.min(self.entries.len()));
        self.visible.clear();
        self.visible.extend(
            self.entries[..n]
                .iter()
                .enumerate()
                .filter(|(_, e)| (hidden || !e.hidden) && (q.is_empty() || e.lower.contains(&q)))
                .map(|(i, _)| i as u32),
        );
        let by_name = select.and_then(|n| self.visible.iter().position(|&i| self.entries[i as usize].os == n));
        let by_prefix =
            || (!q.is_empty()).then(|| self.visible.iter().position(|&i| self.entries[i as usize].lower.starts_with(&q))).flatten();
        self.sel = by_name.or_else(by_prefix).unwrap_or(0);
        self.bump();
    }

    /// `visible` changed: virtual rows look at every key again.
    pub(crate) fn bump(&mut self) {
        self.version = next_version();
    }

    pub fn cursor_name(&self) -> Option<OsString> {
        self.selected().map(|e| e.os.clone())
    }

    pub fn set_sort(&mut self, key: SortKey) {
        self.sort_set = true;
        // Search results are in the order found until a header is clicked: the first click
        // sorts by it, the way a new column does.
        let first = self.search.as_mut().is_some_and(|s| !std::mem::replace(&mut s.sorted, true));
        self.sort = if self.sort.key == key && !first {
            Sort { key, desc: !self.sort.desc }
        } else {
            // Newest and largest first feel right; names and kinds read A→Z.
            Sort { key, desc: matches!(key, SortKey::Size | SortKey::Modified) }
        };
        self.resort();
    }

    /// A new sort from the settings: a search's results keep their order (the listing behind
    /// it gets the sort when the search ends).
    pub fn apply_sort(&mut self, sort: Sort) {
        match self.search.as_mut() {
            Some(s) => s.memo.sort = sort,
            None => {
                self.sort = sort;
                self.resort();
            }
        }
    }

    /// Sorts the listing by `self.sort` again, keeping the cursor.
    pub fn resort(&mut self) {
        let keep = self.cursor_name();
        fs::sort_entries(&mut self.entries, self.sort);
        self.refilter(keep.as_deref());
    }

    pub fn set_hidden(&mut self, on: bool) {
        if self.show_hidden != on {
            self.show_hidden = on;
            let keep = self.cursor_name();
            self.refilter(keep.as_deref());
        }
    }

    /// A plain click: just this row.
    pub fn select(&mut self, i: usize) {
        if !self.visible.is_empty() {
            self.sel = i.min(self.visible.len() - 1);
            self.anchor = self.sel;
            self.marked.clear();
        }
    }

    /// ⌘-click: add or remove this row.
    pub fn toggle_mark(&mut self, i: usize) {
        if i >= self.visible.len() {
            return;
        }
        if self.marked.is_empty()
            && let Some(n) = self.cursor_name()
        {
            self.marked.insert(n);
        }
        let name = self.entries[self.visible[i] as usize].os.clone();
        if !self.marked.remove(&name) {
            self.marked.insert(name);
        }
        self.sel = i;
        self.anchor = i;
    }

    /// ⇧-click or ⇧-arrow: everything from the anchor to row `i`.
    pub fn extend_to(&mut self, i: usize) {
        if self.visible.is_empty() {
            return;
        }
        let i = i.min(self.visible.len() - 1);
        let (a, b) = (self.anchor.min(i), self.anchor.max(i));
        self.marked = self.visible[a..=b].iter().map(|&e| self.entries[e as usize].os.clone()).collect();
        self.sel = i;
    }

    pub fn select_all(&mut self) {
        self.marked = self.visible.iter().map(|&e| self.entries[e as usize].os.clone()).collect();
    }

    pub fn step(&mut self, by: isize, extend: bool, tx: &mut Tx) {
        if self.visible.is_empty() {
            return;
        }
        let max = self.visible.len() as isize - 1;
        let to = (self.sel as isize + by).clamp(0, max) as usize;
        if extend {
            self.extend_to(to);
        } else {
            self.select(to);
        }
        self.reveal(tx);
    }

    pub fn reveal(&self, tx: &mut Tx) {
        if let Some(e) = self.selected() {
            tx.scroll_into_view_in(self.rows_key(), e.key.clone());
        }
    }

    /// The item `old` was just renamed `new` on disk: shows it under its new name at once (a
    /// fresh read follows), keeping it selected if it was, so nothing on screen names a file
    /// that's gone or now another one.
    pub fn renamed(&mut self, old: &OsStr, new: &OsStr) {
        let Some(ix) = self.entries.iter().position(|e| e.os == old) else { return };
        let keep = self.visible.get(self.sel) == Some(&(ix as u32));
        let e = &mut self.entries[ix];
        e.os = new.to_os_string();
        e.name = new.to_string_lossy().into_owned();
        e.lower = e.name.to_lowercase();
        e.key = format!("f:{}", e.name);
        // A dot hides it, everywhere; other ways of hiding (a flag) come with the fresh read.
        e.hidden = e.name.starts_with('.') || (e.hidden && !old.to_string_lossy().starts_with('.'));
        if self.marked.remove(old) {
            self.marked.insert(new.to_os_string());
        }
        let cursor = if keep { Some(new.to_os_string()) } else { self.cursor_name() };
        self.refilter(cursor.as_deref());
    }

    /// Items renamed on disk (`(from, to)`, absolute, each within its own folder): shows the
    /// ones in this folder (or, in a search, among the results) under their new names at once,
    /// keeping them selected if they were (a fresh read follows). In a search, results inside a
    /// renamed folder follow it. One pass, however many were renamed.
    pub fn renamed_paths(&mut self, pairs: &[(PathBuf, PathBuf)]) {
        let searching = self.searching();
        let cwd = self.cwd.clone();
        let map: std::collections::HashMap<PathBuf, PathBuf> = pairs
            .iter()
            .filter_map(|(f, t)| {
                if searching {
                    Some((f.strip_prefix(&cwd).ok()?.to_path_buf(), t.strip_prefix(&cwd).ok()?.to_path_buf()))
                } else if f.parent() == Some(cwd.as_path()) {
                    Some((PathBuf::from(f.file_name()?), PathBuf::from(t.file_name()?)))
                } else {
                    None
                }
            })
            .collect();
        if map.is_empty() {
            return;
        }
        let moved = |rel: &Path| -> Option<(PathBuf, bool)> {
            if let Some(n) = map.get(rel) {
                return Some((n.clone(), true));
            }
            if !searching {
                return None;
            }
            let a = rel.ancestors().skip(1).find(|a| !a.as_os_str().is_empty() && map.contains_key(*a))?;
            Some((map[a].join(rel.strip_prefix(a).ok()?), false))
        };
        let cursor = self.cursor_name();
        let keep = cursor.as_ref().map(|c| moved(Path::new(c)).map_or_else(|| c.clone(), |(n, _)| n.into_os_string()));
        let mut marked = std::mem::take(&mut self.marked);
        for e in &mut self.entries {
            let Some((new, own)) = moved(Path::new(&e.os)) else { continue };
            if own {
                let old = std::mem::take(&mut e.name);
                let name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                e.hidden = name.starts_with('.') || (e.hidden && !old.starts_with('.'));
                e.lower = name.to_lowercase();
                if !searching {
                    e.key = format!("f:{name}");
                }
                e.name = name;
            }
            let was = std::mem::replace(&mut e.os, new.into_os_string());
            if marked.remove(&was) {
                marked.insert(e.os.clone());
            }
        }
        self.marked = marked;
        self.refilter(keep.as_deref());
    }

    pub fn type_filter(&mut self, text: &str) {
        let keep = self.cursor_name();
        self.filter.push_str(text);
        self.marked.clear();
        self.refilter(if self.path_mode() { keep.as_deref() } else { None });
    }
}

/// A tab's label: the folder's name, or the whole path for a root.
pub fn tab_label(p: &Path) -> String {
    crate::app::title(p)
}

pub(crate) fn now() -> rune::LocalTime {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    rune::LocalTime::at(ms)
}

/// Runs `work` on `loader`'s worker and waits here up to `wait` for it. If it's done in time,
/// the answer comes back from this call and never lands through the loader; otherwise `None`,
/// and it lands through `loader.take()` in `poll` like any request. The hand-off is decided
/// under one lock, so an answer is never lost or delivered twice. Work that stops early
/// (`None`) hands nothing back.
pub(crate) fn read_waiting<T: Send + 'static>(
    loader: &Latest<T>,
    wait: std::time::Duration,
    work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    use std::sync::{Arc, Condvar, Mutex};
    struct Hand<T> {
        waiting: bool,
        got: Option<T>,
    }
    let hand = Arc::new((Mutex::new(Hand { waiting: !wait.is_zero(), got: None }), Condvar::new()));
    let worker = hand.clone();
    loader.request_opt((), move |()| {
        let listing = work()?;
        let (lock, cv) = &*worker;
        let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
        if h.waiting {
            h.got = Some(listing);
            cv.notify_one();
            None
        } else {
            Some(listing)
        }
    });
    if wait.is_zero() {
        return None;
    }
    let deadline = std::time::Instant::now() + wait;
    let (lock, cv) = &*hand;
    let mut h = lock.lock().unwrap_or_else(|e| e.into_inner());
    while h.got.is_none() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        h = cv.wait_timeout(h, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    h.waiting = false;
    h.got.take()
}
