//! The column view (As Columns): Finder's column browser.
//!
//! The tab's folder is shown as a row of columns, one per folder level from a **root** down to
//! it, then the selected folder's contents (or the selected file's preview) on the right.
//!
//! **The active column is the tab's [`Folder`].** Its listing, cursor, selection, filter, sort
//! and history are the folder's own, so every action (rename, the context menu, drags, the
//! rectangle, Space, Get Info, F5/F6, undo, paste) resolves against the active column's folder
//! with no code of its own. The other columns are [`Column`]s: a path, a listing while the
//! column is on screen, and which item is highlighted. Pressing anything in another column
//! first makes that column the active one ([`State::adopt_column`]): its listing, already on
//! screen, becomes the folder's in the same frame (no read), and only then does the press act,
//! so it can't act on the folder that was active before.
//!
//! **The root** is the deepest sidebar place (a favorite or a volume) the folder is in, else
//! its volume's root (`/`, `C:\`). It stays while the folder is under it, so moving left and
//! right doesn't make columns jump; a folder outside it picks a new one.
//!
//! **No caching.** Each column's listing is a fresh read with the usual bounded wait
//! ([`WAIT`], shared by every column a change needs) and its own request numbers (a stale
//! answer is dropped). Only the columns on screen hold entries: one scrolled off drops them,
//! and is read again when it's scrolled back. The folder just left keeps its listing only to
//! become a column showing that same folder in the same frame (moving left or right), never
//! longer ([`Columns::spare`]). Each column builds only its visible rows.
//!
//! **Scrolling** is by whole columns: as many as fit the pane at the column width (one for the
//! window, the dividers drag it), the deepest shown after every move, and the horizontal wheel
//! (or Shift + wheel) scrolls back while the active column stays on screen.

use std::cell::Cell;
use std::ffi::OsString;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rune::{Latest, Tx};

use crate::app::State;
use crate::folder::{Answer, Folder, Nav};
use crate::fs::{self, Entry, Listing, Sort};
use crate::grid::ViewMode;

/// A column's width to start with, the narrowest and widest a divider drags it, in points.
pub const COL_W: f32 = 220.0;
pub const COL_MIN: f32 = 140.0;
pub const COL_MAX: f32 = 520.0;
/// The divider after each column: a 1 pt line in the middle of a strip the pointer can grab.
pub const DIVIDER: f32 = 5.0;
/// How long a change waits for the columns it needs, at most (all of them, together), before
/// drawing without them; a slower listing lands through `poll`. Less than a frame: a key that
/// moves the cursor onto a large folder still lands within one.
pub const WAIT: Duration = Duration::from_millis(4);
/// Horizontal wheel travel that scrolls by one column, in points.
const WHEEL_STEP: f32 = 48.0;

/// Where a column's listing is.
#[derive(Clone, Debug, PartialEq)]
pub enum Load {
    /// Off screen (or not read yet): no entries.
    Off,
    Loading,
    Ready,
    Failed(String),
}

/// Every column gets its own number, for its element keys: a column keeps it (and Rune its
/// scroll) for as long as it shows the same folder.
static COLUMN: AtomicU64 = AtomicU64::new(1);

/// A new column element's number.
pub(crate) fn next_uid() -> u64 {
    COLUMN.fetch_add(1, Relaxed)
}

/// One column other than the active one.
pub struct Column {
    pub path: PathBuf,
    pub uid: u64,
    pub dir_id: Option<crate::noclobber::DirId>,
    /// The whole folder, sorted (empty unless [`Load::Ready`]).
    pub entries: Vec<Entry>,
    /// Indices into `entries` the hidden-file rule shows.
    pub visible: Vec<u32>,
    /// The item highlighted: in a column left of the active one, the next folder on the way.
    pub pick: Option<OsString>,
    pub load: Load,
    /// Changes whenever `visible` does (virtual rows look at their keys again).
    pub version: u64,
    sort: Sort,
    hidden: bool,
    req: u64,
    stop: Arc<AtomicBool>,
    loader: Latest<Answer>,
    /// Where its rows are scrolled to, written by the view (geometry, for an in-window drop).
    pub(crate) scroll: std::rc::Rc<Cell<f32>>,
}

impl Drop for Column {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl Column {
    fn new(path: PathBuf, waker: Option<&rune::Waker>) -> Column {
        let loader = Latest::default();
        if let Some(w) = waker {
            loader.set_waker(w.clone());
        }
        Column {
            path,
            uid: next_uid(),
            dir_id: None,
            entries: Vec::new(),
            visible: Vec::new(),
            pick: None,
            load: Load::Off,
            version: crate::folder::next_version(),
            sort: Sort::new(fs::SortKey::Name, false),
            hidden: false,
            req: 0,
            stop: Arc::default(),
            loader,
            scroll: std::rc::Rc::new(Cell::new(0.0)),
        }
    }

    /// A column of a listing on screen this frame (the folder just left), sorted and shown as
    /// `shown` says (sort, hidden files), drawn as element `uid` (the one it was drawn as) when
    /// there was one, with its `rows` as they were shown when they're its whole listing.
    pub(crate) fn ready(
        path: PathBuf,
        entries: Vec<Entry>,
        dir_id: Option<crate::noclobber::DirId>,
        (sort, hidden): (Sort, bool),
        uid: Option<u64>,
        rows: Option<(Vec<u32>, u64)>,
        waker: Option<&rune::Waker>,
    ) -> Column {
        let mut c = Column::new(path, waker);
        if let Some(u) = uid {
            c.uid = u;
        }
        c.entries = entries;
        c.dir_id = dir_id;
        c.sort = sort;
        c.load = Load::Ready;
        match rows {
            // The rows the folder showed (no filter): the same, under the same version.
            Some((visible, version)) => {
                c.hidden = hidden;
                c.visible = visible;
                c.version = version;
            }
            None => c.refilter(hidden),
        }
        c
    }

    pub fn key(&self) -> String {
        format!("col-{}", self.uid)
    }

    pub fn rows_key(&self) -> String {
        format!("colrows-{}", self.uid)
    }

    /// The highlighted item's row.
    pub fn picked(&self) -> Option<usize> {
        let pick = self.pick.as_ref()?;
        self.visible.iter().position(|&i| self.entries[i as usize].os == *pick)
    }

    pub fn entry(&self, row: usize) -> Option<&Entry> {
        self.visible.get(row).map(|&i| &self.entries[i as usize])
    }

    fn refilter(&mut self, hidden: bool) {
        self.hidden = hidden;
        self.visible.clear();
        self.visible.extend(self.entries.iter().enumerate().filter(|(_, e)| hidden || !e.hidden).map(|(i, _)| i as u32));
        self.version = crate::folder::next_version();
    }

    /// Where a drop on this column's background goes: its folder, as its read knew it.
    pub fn dest(&self) -> crate::ops::Dest {
        crate::ops::Dest { path: self.path.clone(), id: self.dir_id.map(crate::ops::DestId::Folder) }
    }

    /// Where a drop on its folder row `e` goes: into it, as this column's read saw it.
    pub fn row_dest(&self, e: &Entry) -> crate::ops::Dest {
        crate::ops::Dest { path: self.path.join(&e.os), id: e.id.map(crate::ops::DestId::Item) }
    }

    /// Whether it has a listing to show: read, or being read again with the last one still on
    /// screen (after a file operation changed it, or ⌘R).
    pub fn listed(&self) -> bool {
        self.load == Load::Ready || (self.load == Load::Loading && !self.visible.is_empty())
    }

    /// Its folder changed (or ⌘R): read it again if it's on screen, its rows staying until the
    /// fresh listing lands.
    fn reread(&mut self) {
        self.stop.store(true, Relaxed);
        self.loader.cancel();
        self.load = Load::Off;
    }

    /// Scrolled off: its entries go, and any read stops.
    fn drop_entries(&mut self) {
        self.stop.store(true, Relaxed);
        self.loader.cancel();
        self.entries = Vec::new();
        self.visible = Vec::new();
        self.load = Load::Off;
        self.version = crate::folder::next_version();
    }

    /// Starts a fresh read, held for a shared wait ([`crate::bounded::start`]).
    fn read(&mut self, sort: Sort, hidden: bool) -> crate::bounded::Waiting<Answer> {
        self.stop.store(true, Relaxed);
        self.stop = Arc::default();
        let (stop, req, path) = (self.stop.clone(), crate::folder::next_request(), self.path.clone());
        self.req = req;
        self.sort = sort;
        self.hidden = hidden;
        self.load = Load::Loading;
        crate::bounded::start(&self.loader, true, move || Some(Answer { req, sort, listing: fs::list_until(path, sort, &stop)? }))
    }

    /// A finished read, if it's the one this column waits for. True when it landed.
    fn land(&mut self, a: Answer) -> bool {
        if a.req != self.req || a.listing.path != self.path || self.load != Load::Loading {
            return false;
        }
        match a.listing.result {
            Ok(mut entries) => {
                if a.sort != self.sort {
                    fs::sort_entries(&mut entries, self.sort);
                }
                self.entries = entries;
                self.dir_id = a.listing.dir_id;
                self.load = Load::Ready;
                let hidden = self.hidden;
                self.refilter(hidden);
            }
            Err(e) => {
                // An error isn't a listing: nothing from before stays behind it.
                self.entries = Vec::new();
                self.visible = Vec::new();
                self.load = Load::Failed(e);
                self.version = crate::folder::next_version();
            }
        }
        true
    }
}

/// A tab's columns while it shows as columns (`Folder::cols`).
pub struct Columns {
    pub root: PathBuf,
    /// From the root down to the active folder's parent.
    pub left: Vec<Column>,
    /// The selected folder's contents, while the cursor is on one folder.
    pub right: Option<Column>,
    /// The selected file's preview takes the place on the right (the active pane only).
    pub preview: bool,
    /// The first column on screen (counting the left ones, the active one, then the right one).
    pub first: usize,
    /// How many fit, as last worked out.
    pub fit: usize,
    /// The active folder and what's on its right the scroll was last set for: either
    /// changing shows the deepest columns again.
    shown_for: Option<(PathBuf, Option<PathBuf>, bool)>,
    /// The folder just left, as a column, for the next sync to reuse if it shows the same
    /// folder; dropped by that sync otherwise. Never kept past it.
    pub(crate) spare: Vec<Column>,
    /// Horizontal wheel travel not yet turned into a column.
    wheel: Cell<f32>,
    /// The folder a file drag hovers over in a column other than the active one.
    pub drop_target: Option<PathBuf>,
    /// The folder an in-window drag in this pane would land in now (a row's or a column's).
    pub(crate) drag_over: Option<PathBuf>,
    /// Where each column on screen is (window points), as last laid out: the view writes it,
    /// an in-window drop reads it (geometry, not file data).
    pub(crate) hits: std::cell::RefCell<Vec<Hit>>,
}

/// A column on screen as it was laid out: its rect, and which column it was then, by its
/// element's number (which moves with its listing when a column becomes the active one or
/// stops being it) and its folder's path and identity. A drop resolves against that column,
/// wherever its listing is now, never against whichever folder is active by then.
pub(crate) struct Hit {
    pub rect: [f32; 4],
    pub uid: u64,
    pub path: PathBuf,
    pub dir_id: Option<crate::noclobber::DirId>,
}

impl Columns {
    fn new(root: PathBuf) -> Columns {
        Columns {
            root,
            left: Vec::new(),
            right: None,
            preview: false,
            first: 0,
            fit: 1,
            shown_for: None,
            spare: Vec::new(),
            wheel: Cell::new(0.0),
            drop_target: None,
            drag_over: None,
            hits: Default::default(),
        }
    }

    /// How many columns there are: the left ones, the active one, and the right one or the
    /// preview.
    pub fn slots(&self) -> usize {
        self.left.len() + 1 + usize::from(self.right.is_some() || self.preview)
    }

    /// The active column's place.
    pub fn active(&self) -> usize {
        self.left.len()
    }

    /// The columns on screen.
    pub fn shown(&self) -> Range<usize> {
        let n = self.slots();
        self.first.min(n)..(self.first + self.fit).min(n)
    }

    /// The column showing `path` (not the active one).
    pub fn column(&self, path: &Path) -> Option<&Column> {
        self.left.iter().chain(self.right.as_ref()).find(|c| c.path == path)
    }

    /// The column numbered `uid` (`Column::uid`): what its rows look it up by, a number
    /// compared for each of a large folder's rows rather than a path.
    pub fn by_uid(&self, uid: u64) -> Option<&Column> {
        self.left.iter().chain(self.right.as_ref()).find(|c| c.uid == uid)
    }

    fn column_mut(&mut self, path: &Path) -> Option<&mut Column> {
        self.left.iter_mut().chain(self.right.as_mut()).find(|c| c.path == path)
    }

    /// Where the first column may be: the active one always on screen.
    fn bounds(&self) -> (usize, usize) {
        let (active, n, fit) = (self.active(), self.slots(), self.fit.max(1));
        ((active + 1).saturating_sub(fit), active.min(n.saturating_sub(fit)))
    }

    /// The columns showing one of `folders` (by identity when both are known, else by path)
    /// are read again, as the active folder is (`State::refresh`); every column with `all`.
    pub(crate) fn reread(&mut self, folders: &[(PathBuf, Option<crate::noclobber::DirId>)], all: bool) {
        for c in self.left.iter_mut().chain(self.right.as_mut()) {
            let hit = all
                || folders.iter().any(|(p, id)| match (c.dir_id, id) {
                    (Some(a), Some(b)) => a == *b,
                    _ => c.path == *p,
                });
            if hit && c.load != Load::Off {
                c.reread();
            }
        }
    }

    /// Whether a column's read is on its way (the bench waits for it).
    pub fn loading(&self) -> bool {
        self.left.iter().chain(self.right.as_ref()).any(|c| c.load == Load::Loading)
    }

    /// Every column holding entries (tests: nothing off screen holds any).
    #[cfg(test)]
    pub fn holding(&self) -> Vec<PathBuf> {
        self.left.iter().chain(self.right.as_ref()).filter(|c| !c.entries.is_empty()).map(|c| c.path.clone()).collect()
    }
}

/// Where a folder's columns start: the deepest of `places` it's in (a sidebar favorite or
/// volume, or the folder itself), else its volume's root.
pub fn root_for<'a>(cwd: &Path, places: impl Iterator<Item = &'a Path>) -> PathBuf {
    places
        .filter(|p| !p.as_os_str().is_empty() && cwd.starts_with(p))
        .max_by_key(|p| p.components().count())
        .map_or_else(|| cwd.ancestors().last().unwrap_or(cwd).to_path_buf(), Path::to_path_buf)
}

/// The folders from `root` down to `cwd`'s parent, root first (none when `cwd` is the root).
pub fn ancestors(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = cwd.ancestors().skip(1).take_while(|a| a.starts_with(root)).map(Path::to_path_buf).collect();
    v.reverse();
    v
}

/// Takes the column for `path` out of `pool`, if one is there (the readiest first).
fn take(pool: &mut Vec<Column>, path: &Path) -> Option<Column> {
    let i = pool
        .iter()
        .enumerate()
        .filter(|(_, c)| c.path == path)
        .max_by_key(|(_, c)| match c.load {
            Load::Ready => 3,
            Load::Loading => 2,
            Load::Failed(_) => 1,
            Load::Off => 0,
        })
        .map(|(i, _)| i)?;
    Some(pool.swap_remove(i))
}

impl State {
    /// Whether the column view's preview is wanted: the active pane shows columns (not a
    /// search), the cursor is on one file, the user hasn't turned it off (Space), and Get Info
    /// isn't in its place.
    pub(crate) fn column_preview_wanted(&self) -> bool {
        let f = self.cur();
        f.view == ViewMode::Columns
            && !f.searching()
            && !f.is_loading()
            && !self.col_preview_off
            && self.info.is_none()
            && f.marked.len() <= 1
            && f.selected().is_some_and(|e| !e.is_dir)
    }

    /// Whether the active pane's preview column is on screen (scrolled back, it isn't: the
    /// preview closes, its decode stops, and it's read again when it shows).
    pub(crate) fn column_preview_on_screen(&self) -> bool {
        self.cur().cols.as_ref().is_some_and(|c| c.preview && c.shown().end == c.slots())
    }

    /// Keeps every pane's columns to its folder and selection: the left ones from the root down
    /// to the folder, the selected folder's on the right, the scroll, what's read and what's
    /// dropped. Called with the preview's sync, after every key, command, click and landing,
    /// and from `poll`. Reads every column it starts within one shared [`WAIT`]. True when
    /// anything changed.
    pub(crate) fn sync_columns(&mut self) -> bool {
        let mut changed = false;
        let mut reads = Vec::new();
        for ix in 0..self.panes.len() {
            changed |= self.sync_columns_in(ix, &mut reads);
        }
        if !reads.is_empty() {
            self.col_reads += reads.len() as u64;
            let deadline = Instant::now() + self.column_wait;
            for (ix, path, w) in reads {
                let Some(a) = w.until(deadline) else { continue };
                if let Some(c) = self.panes[ix].f.cols.as_mut().and_then(|c| c.column_mut(&path))
                    && c.land(a)
                    && let Some(r) = reveal_of(c)
                {
                    self.col_reveals.push(r);
                }
            }
        }
        changed
    }

    fn sync_columns_in(&mut self, ix: usize, reads: &mut Vec<(usize, PathBuf, crate::bounded::Waiting<Answer>)>) -> bool {
        let preview = ix == self.active && self.column_preview_wanted();
        let col_w = self.col_w;
        // Covered by Get Info or the preview (the other pane's place when split): nothing of
        // it is on screen, so its columns drop their listings and stop their reads, and are
        // read again once it shows.
        let covered = self.split() && ix != self.active && self.panel_docked();
        let f = &self.panes[ix].f;
        if f.view != ViewMode::Columns || f.searching() {
            let f = &mut self.panes[ix].f;
            f.col_uid = None;
            return f.cols.take().is_some();
        }
        // A column being made active, its listing in hand: the columns change once it lands
        // (`State::flush_columns`), not before (the column taken out would look missing).
        if f.adopted.is_some() {
            return false;
        }
        if covered {
            let Some(cols) = self.panes[ix].f.cols.as_mut() else { return false };
            cols.spare.clear();
            let mut changed = false;
            for c in cols.left.iter_mut().chain(cols.right.as_mut()) {
                if c.load != Load::Off || !c.entries.is_empty() {
                    c.drop_entries();
                    changed = true;
                }
            }
            return changed;
        }
        // A new root only when needed: the sidebar's places are looked at then.
        let needs_root = f.cols.as_ref().is_none_or(|c| !f.cwd.starts_with(&c.root));
        let root = needs_root.then(|| {
            let places = self.places.iter().map(|(_, p)| p.as_path()).chain(self.volumes.iter().map(|v| v.path.as_path()));
            root_for(&f.cwd, places)
        });
        let waker = self.waker.clone();
        let f = &mut self.panes[ix].f;
        let mut changed = false;
        if f.col_uid.is_none() {
            f.col_uid = Some(next_uid());
        }
        // The right column: the selected folder's contents. While the folder's own read is on
        // its way, what's on the right stays.
        let loading = f.is_loading();
        let single = f.marked.len() <= 1;
        let want_right = f.selected().filter(|e| single && e.is_dir).map(|e| f.cwd.join(&e.os));
        let cols = f.cols.get_or_insert_with(|| {
            changed = true;
            Box::new(Columns::new(PathBuf::new()))
        });
        if let Some(root) = root {
            cols.root = root;
        }
        // The left columns: kept (with their listings) where they still show the same folder.
        let wanted = ancestors(&cols.root, &f.cwd);
        let mut reveals = Vec::new();
        // Columns that may show a folder again in a new place: the folder just left, and the
        // left ones no longer wanted there. Whatever isn't taken goes at the end of this sync.
        let mut pool: Vec<Column> = std::mem::take(&mut cols.spare);
        if cols.left.len() != wanted.len() || cols.left.iter().zip(&wanted).any(|(c, p)| c.path != *p) {
            pool.append(&mut cols.left);
            cols.left = wanted.iter().map(|p| take(&mut pool, p).unwrap_or_else(|| Column::new(p.clone(), waker.as_ref()))).collect();
            changed = true;
        }
        for k in 0..cols.left.len() {
            let next = wanted.get(k + 1).map_or(&f.cwd, |p| p).file_name().map(|n| n.to_os_string());
            let c = &mut cols.left[k];
            if c.pick != next {
                // The highlight only: the rows and their keys are as they were (no new version).
                c.pick = next;
                if c.load == Load::Ready
                    && let Some(r) = reveal_of(c)
                {
                    reveals.push(r);
                }
                changed = true;
            }
        }
        if !loading {
            let want = want_right;
            if cols.right.as_ref().map(|c| &c.path) != want.as_ref() {
                pool.extend(cols.right.take());
                cols.right = want.map(|p| take(&mut pool, &p).unwrap_or_else(|| Column::new(p, waker.as_ref())));
                changed = true;
            }
            if cols.preview != preview {
                cols.preview = preview;
                changed = true;
            }
        }
        drop(pool);
        // How many fit, and the scroll: the deepest after every move or resize, else where the
        // wheel left it, the active column always on screen.
        let width = match f.cols_w.get() {
            w if w > 0.0 => w,
            _ => f.list_w.get().max(800.0),
        };
        let fit = (((width + DIVIDER) / (col_w + DIVIDER)).floor() as usize).max(1);
        let resized = cols.fit != fit;
        if resized {
            cols.fit = fit;
            changed = true;
        }
        let at = (f.cwd.clone(), cols.right.as_ref().map(|c| c.path.clone()), cols.preview);
        let (lo, hi) = cols.bounds();
        let first = if resized || cols.shown_for.as_ref() != Some(&at) {
            cols.shown_for = Some(at);
            hi
        } else {
            cols.first.clamp(lo, hi)
        };
        if first != cols.first {
            cols.first = first;
            changed = true;
        }
        // Read what's on screen and not read; drop what isn't on screen.
        let shown = cols.shown();
        let active = cols.active();
        let (sort, hidden) = (f.sort, f.show_hidden);
        let right = cols.right.iter_mut().map(|c| (active + 1, c));
        for (k, c) in cols.left.iter_mut().enumerate().chain(right) {
            let on = shown.contains(&k);
            match (&c.load, on) {
                (Load::Off, true) => {
                    reads.push((ix, c.path.clone(), c.read(sort, hidden)));
                    changed = true;
                }
                (Load::Off, false) if c.entries.is_empty() => {}
                (_, false) => {
                    c.drop_entries();
                    changed = true;
                }
                (Load::Ready, true) if c.sort != sort => {
                    c.sort = sort;
                    fs::sort_entries(&mut c.entries, sort);
                    c.refilter(hidden);
                    changed = true;
                }
                (Load::Ready, true) if c.hidden != hidden => {
                    c.refilter(hidden);
                    changed = true;
                }
                // A read sorted the old way: read again.
                (Load::Loading | Load::Failed(_), true) if c.sort != sort => {
                    reads.push((ix, c.path.clone(), c.read(sort, hidden)));
                    changed = true;
                }
                _ => {}
            }
        }
        self.col_reveals.extend(reveals);
        changed
    }

    /// Columns that finished reading, from `poll`. True when one landed.
    pub(crate) fn poll_columns(&mut self) -> bool {
        let mut changed = false;
        for p in &mut self.panes {
            let Some(cols) = p.f.cols.as_mut() else { continue };
            for c in cols.left.iter_mut().chain(cols.right.as_mut()) {
                if let Some(a) = c.loader.take()
                    && c.land(a)
                {
                    changed = true;
                    self.col_lands += 1;
                    if let Some(r) = reveal_of(c) {
                        self.col_reveals.push(r);
                    }
                }
            }
        }
        changed
    }

    /// Going to `path` in the active pane, when it's one of its columns: the column becomes the
    /// active folder now. With its listing on screen, from that listing (no read), landing as a
    /// read would ([`State::flush_columns`]: history, selection, scroll, title). Without one
    /// (its read on its way, off screen, failed), empty and loading its own read, so nothing
    /// selected in another folder stays what actions act on. False when it isn't a column (the
    /// folder is read as usual).
    pub(crate) fn adopt_column(&mut self, path: &Path, nav: Nav, select: &[OsString]) -> bool {
        let id = self.cur().id;
        let waker = self.waker.clone();
        let f = self.cur_mut();
        if f.view != ViewMode::Columns || f.searching() {
            return false;
        }
        let Some(cols) = f.cols.as_mut() else { return false };
        let col = match cols.left.iter().position(|c| c.path == path) {
            Some(k) => cols.left.remove(k),
            None if cols.right.as_ref().is_some_and(|c| c.path == path) => cols.right.take().expect("checked just above"),
            None => return false,
        };
        if !col.listed() {
            let (old, entries, old_id) = f.enter_unread(col.path.clone(), nav, select.to_vec(), col.dir_id);
            // The folder takes the column's element; the folder left, its own.
            let old_uid = f.col_uid.replace(col.uid);
            if !entries.is_empty()
                && let Some(cols) = f.cols.as_mut()
            {
                let spare = Column::ready(old, entries, old_id, (f.sort, f.show_hidden), old_uid, None, waker.as_ref());
                spare.scroll.set(f.scroll.get());
                cols.spare.push(spare);
            }
            // The folder is drawn as the column's element now, scrolled as that was.
            f.scroll.set(col.scroll.get());
            self.shared.history.borrow_mut().record(path);
            self.note = None;
            self.col_reads += 1;
            return true;
        }
        let req = f.pending_for(path.to_path_buf(), nav, select.to_vec());
        let sort = col.sort;
        let mut col = col;
        let rows = (col.sort == f.sort && col.hidden == f.show_hidden).then(|| (std::mem::take(&mut col.visible), col.version));
        f.adopted = Some(crate::folder::Adopted { req, uid: col.uid, rows });
        let listing = Listing { path: col.path.clone(), result: Ok(std::mem::take(&mut col.entries)), dir_id: col.dir_id, missing: false };
        self.col_ready = Some((id, Answer { req, sort, listing }));
        // Landed by the next handler with a `Tx` (a key's, a command's, a press's), or else by
        // the `poll` this wakes.
        if let Some(w) = &self.waker {
            w();
        }
        true
    }

    /// What needs a `Tx` after the columns changed: a column made active lands
    /// ([`State::adopt_column`]), and columns that came on screen scroll to their highlighted
    /// item. Called by every handler that can change them, and from `poll`.
    pub(crate) fn flush_columns(&mut self, tx: &mut Tx) {
        if let Some((id, a)) = self.col_ready.take()
            && let Some(ix) = self.pane_ix(id)
        {
            self.land(ix, a, tx);
        }
        for (rows, key) in self.col_reveals.drain(..) {
            tx.scroll_into_view_in(rows, key);
        }
    }

    /// A press in pane `id`'s column for `path` (not the active one), on item `name` (or its
    /// empty space, `None`, which keeps its highlighted item): that column becomes the active
    /// one, with the item selected. False when it couldn't (the column has gone since).
    pub(crate) fn activate_column(&mut self, id: u64, path: &Path, name: Option<&OsString>, tx: &mut Tx) -> bool {
        tx.instant();
        if !self.activate_id(id, tx) {
            return false;
        }
        // A press elsewhere ends an inline rename by committing it, as in the list, then goes on
        // (unless the rename couldn't go yet, its field staying open).
        if self.rename.is_some() {
            self.commit_rename(tx);
            if self.rename.is_some() {
                return false;
            }
        }
        // Already the active column (a press queued behind the one that made it so): the item
        // pressed is still the one to select, now or when its listing lands.
        if self.cur().cwd == path && self.cur().pending.as_ref().is_none_or(|p| p.path == path) {
            if let Some(name) = name {
                let f = self.cur_mut();
                let picks = f.picks;
                match f.pending.as_mut() {
                    Some(p) => {
                        p.select = vec![name.clone()];
                        p.picks = picks;
                    }
                    None => {
                        if let Some(i) = f.visible.iter().position(|&e| f.entries[e as usize].os == *name) {
                            f.select(i);
                            f.reveal(tx);
                        }
                    }
                }
                self.sync_preview();
                self.flush_columns(tx);
            }
            return true;
        }
        let pick = name.cloned().or_else(|| self.cur().cols.as_ref().and_then(|c| c.column(path)?.pick.clone()));
        if !self.adopt_column(path, Nav::New, &pick.into_iter().collect::<Vec<_>>()) {
            return false;
        }
        self.flush_columns(tx);
        self.sync_preview();
        self.flush_columns(tx);
        true
    }

    /// The horizontal wheel (or Shift + a vertical one) over pane `id`'s columns: scrolls them
    /// by whole columns, the active one staying on screen. True when it took the delta.
    pub(crate) fn columns_wheel(&mut self, id: u64, ev: &rune::WheelEvent, tx: &mut Tx) -> bool {
        let d = if ev.dx != 0.0 && ev.dx.abs() >= ev.dy.abs() {
            ev.dx
        } else if ev.mods.shift && !ev.precise {
            ev.dy
        } else {
            return false;
        };
        let Some(ix) = self.pane_ix(id) else { return false };
        let Some(cols) = self.panes[ix].f.cols.as_mut() else { return false };
        let acc = cols.wheel.get() + d;
        let steps = (acc / WHEEL_STEP).trunc();
        cols.wheel.set(acc - steps * WHEEL_STEP);
        // A delta that scrolls nothing (too small yet, or past an end) takes the wheel and lets
        // Rune commit a view that changes nothing. Never `Tx::live_only`: outside a drag that a
        // release ends, it leaves a window with no live values drawing every frame
        // (RUNE-GAPS.md).
        if steps == 0.0 {
            return true;
        }
        // Content moving right (a positive delta) shows the columns to the left.
        let (lo, hi) = cols.bounds();
        let to = (cols.first as isize - steps as isize).clamp(lo as isize, hi as isize) as usize;
        if to == cols.first {
            return true;
        }
        tx.instant();
        cols.first = to;
        // The columns, and the preview with its column.
        self.sync_preview();
        self.flush_columns(tx);
        true
    }

    /// Where a drag in pane `id`'s columns let go at window point `at` would land: the folder
    /// row under it, else the folder of the column under it, as that column listed it; with
    /// the row's name when it's in the active column (its highlight). From geometry: each
    /// column's rect and scroll as last laid out, the rows a fixed height.
    pub(crate) fn column_drop_at(&self, id: u64, at: [f32; 2]) -> Option<(crate::ops::Dest, Option<OsString>)> {
        let f = &self.pane(id)?.f;
        let cols = f.cols.as_ref()?;
        let hits = cols.hits.borrow();
        let inside = |[x, y, w, h]: [f32; 4]| at[0] >= x && at[0] < x + w && at[1] >= y && at[1] < y + h;
        let hit = hits.iter().find(|h| inside(h.rect))?;
        let row = |scroll: f32| ((at[1] - hit.rect[1] + scroll) / crate::view::ROW_H).floor();
        // The column it was, wherever its listing is now, while it's still that folder.
        let same = |path: &Path, id: Option<crate::noclobber::DirId>| {
            path == hit.path && (hit.dir_id.is_none() || id.is_none() || id == hit.dir_id)
        };
        if f.col_uid == Some(hit.uid) {
            // The folder's own list (the active column now, whichever it was then).
            if !same(&f.cwd, f.dir_id) {
                return None;
            }
            let r = row(f.scroll.get());
            let e = (r >= 0.0).then(|| f.visible.get(r as usize)).flatten().map(|&k| &f.entries[k as usize]);
            Some(match e {
                Some(e) if e.is_dir => (f.row_dest(e), Some(e.os.clone())),
                _ => (f.dest(), None),
            })
        } else {
            let c = cols.by_uid(hit.uid).filter(|c| same(&c.path, c.dir_id))?;
            let r = row(c.scroll.get());
            let e = (r >= 0.0 && c.listed()).then(|| c.entry(r as usize)).flatten();
            Some(match e {
                Some(e) if e.is_dir => (c.row_dest(e), None),
                _ => (c.dest(), None),
            })
        }
    }

    /// Highlights where an in-window drag in pane `id`'s columns would land (`None`: nowhere).
    /// True when that changed.
    pub(crate) fn set_cols_hover(&mut self, id: u64, over: Option<(PathBuf, Option<OsString>)>) -> bool {
        let Some(ix) = self.pane_ix(id) else { return false };
        let f = &mut self.panes[ix].f;
        let Some(cols) = f.cols.as_mut() else { return false };
        let (path, row) = over.map_or((None, None), |(p, r)| (Some(p), r));
        if cols.drag_over == path && f.drop_target == row {
            return false;
        }
        cols.drop_target = path.clone();
        cols.drag_over = path;
        f.drop_target = row;
        true
    }

    /// Where an in-window drag in the active pane's columns would land now (tests).
    #[cfg(test)]
    pub(crate) fn cols_hover(&self) -> Option<PathBuf> {
        self.cur().cols.as_ref()?.drag_over.clone()
    }

    /// Rows `i` (drawn at `version`) of pane `id` dragged in the window onto `dest` (a folder
    /// row or column in the column view): into it, as a file drop, each item checked.
    pub(crate) fn drop_rows_into(&mut self, id: u64, i: usize, version: u64, dest: crate::ops::Dest, tx: &mut Tx) {
        let Some(src) = self.pane(id).filter(|p| p.f.version == version) else { return };
        let (files, expect) = (src.f.drag_paths(i), src.f.drag_checks(i));
        self.drop_into(&files, dest, expect, tx);
    }

    /// Scrolls pane `id`'s columns one to the left (the "‹" at their edge).
    pub(crate) fn columns_back(&mut self, id: u64, tx: &mut Tx) {
        tx.instant();
        let Some(ix) = self.pane_ix(id) else { return };
        let Some(cols) = self.panes[ix].f.cols.as_mut() else { return };
        let (lo, _) = cols.bounds();
        cols.first = cols.first.saturating_sub(1).max(lo);
        // The columns, and the preview with its column.
        self.sync_preview();
        self.flush_columns(tx);
    }
}

/// Where a column scrolls to show its highlighted item.
fn reveal_of(c: &Column) -> Option<(String, String)> {
    let row = c.picked()?;
    Some((c.rows_key(), c.entry(row)?.key.clone()))
}

impl Folder {
    /// Shows `path` at once, empty, its read asked for: a column without a listing made active
    /// ([`State::adopt_column`]). A navigation (`nav`'s history now), landing as a reload of
    /// the folder already shown, `select` selected. Gives back the folder left, its listing
    /// and identity, for the column it becomes.
    pub(crate) fn enter_unread(
        &mut self,
        path: PathBuf,
        nav: Nav,
        select: Vec<OsString>,
        dir_id: Option<crate::noclobber::DirId>,
    ) -> (PathBuf, Vec<Entry>, Option<crate::noclobber::DirId>) {
        let old = std::mem::replace(&mut self.cwd, path.clone());
        match nav {
            Nav::New => {
                self.back.push(old.clone());
                self.fwd.clear();
            }
            Nav::Back => {
                self.back.pop();
                self.fwd.push(old.clone());
            }
            Nav::Forward => {
                self.fwd.pop();
                self.back.push(old.clone());
            }
            Nav::Reload | Nav::Show | Nav::Fresh => {}
        }
        let entries = std::mem::take(&mut self.entries);
        let old_id = std::mem::replace(&mut self.dir_id, dir_id);
        self.visible.clear();
        self.marked.clear();
        self.sel = 0;
        self.anchor = 0;
        self.filter.clear();
        self.goto = false;
        self.drop_target = None;
        self.now = crate::folder::now();
        self.bump();
        self.request(path, Nav::Reload, select, false);
        (old, entries, old_id)
    }

    /// The folder is about to show `path`'s listing from a column ([`State::adopt_column`]):
    /// a request like any read's, but with nothing sent to a worker; its answer is in hand.
    pub(crate) fn pending_for(&mut self, path: PathBuf, nav: Nav, select: Vec<OsString>) -> u64 {
        // As `Folder::request`: the go-to field closes when the navigation lands.
        self.search = None;
        self.tiles = None;
        self.stop_read();
        self.loader.cancel();
        let req = crate::folder::next_request();
        let picks = self.picks;
        self.pending = Some(crate::folder::Pending { req, path, nav, select, rename: false, scroll: 0.0, picks });
        req
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::{settle_all, state_in};
    use crate::commands::Cmd;

    #[test]
    fn the_root_is_the_deepest_place_else_the_volume() {
        let places = [PathBuf::from("/home/u"), PathBuf::from("/home/u/Documents"), PathBuf::from("/media/usb")];
        let at = |p: &str| root_for(Path::new(p), places.iter().map(PathBuf::as_path));
        assert_eq!(at("/home/u/Documents/a/b"), PathBuf::from("/home/u/Documents"));
        assert_eq!(at("/home/u/Downloads"), PathBuf::from("/home/u"));
        assert_eq!(at("/home/u"), PathBuf::from("/home/u"), "a place is its own root");
        assert_eq!(at("/home/user2"), PathBuf::from("/"), "by whole names: /home/u isn't above /home/user2");
        assert_eq!(at("/etc/x"), PathBuf::from("/"));
        assert_eq!(ancestors(Path::new("/home/u"), Path::new("/home/u/a/b")), [PathBuf::from("/home/u"), PathBuf::from("/home/u/a")]);
        assert!(ancestors(Path::new("/home/u"), Path::new("/home/u")).is_empty());
        assert_eq!(ancestors(Path::new("/"), Path::new("/etc")), [PathBuf::from("/")]);
    }

    /// d/{a/{b/{c/{deep.txt}, x.txt}, x.txt, y.txt}, other/{o.txt}, top.txt}: two `x.txt`s in
    /// two columns.
    fn tree(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("arbor-cols-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b/c")).unwrap();
        std::fs::create_dir_all(d.join("other")).unwrap();
        std::fs::write(d.join("a/b/c/deep.txt"), "deep").unwrap();
        std::fs::write(d.join("a/b/x.txt"), "inner").unwrap();
        std::fs::write(d.join("a/x.txt"), "outer").unwrap();
        std::fs::write(d.join("a/y.txt"), "y").unwrap();
        std::fs::write(d.join("other/o.txt"), "o").unwrap();
        std::fs::write(d.join("top.txt"), "t").unwrap();
        d
    }

    /// A state in `d` showing as columns, its root `d` (a sidebar place), settled.
    fn columns_in(d: &Path) -> State {
        let mut s = state_in(d);
        settle_all(&mut s);
        s.places = vec![("Test".into(), d.to_path_buf())];
        // Plenty of room: every column fits unless a test narrows it.
        s.cur().cols_w.set(4000.0);
        s.column_wait = Duration::from_secs(5);
        s.set_view(ViewMode::Columns, &mut Tx::default());
        settle(&mut s);
        s
    }

    /// Polls as frames would until no folder or column is loading.
    fn settle(s: &mut State) {
        let until = Instant::now() + Duration::from_secs(10);
        let busy = |s: &State| {
            s.panes.iter().any(|p| {
                p.f.is_loading()
                    || p.f.cols.as_ref().is_some_and(|c| c.left.iter().chain(c.right.as_ref()).any(|c| c.load == Load::Loading))
            })
        };
        while busy(s) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(2));
            s.poll(&mut Tx::default());
        }
        assert!(!busy(s), "never landed");
    }

    fn select(s: &mut State, name: &str) {
        let f = s.cur_mut();
        let i = f.visible.iter().position(|&e| f.entries[e as usize].name == name).unwrap_or_else(|| panic!("no {name}"));
        f.select(i);
        s.sync_preview();
        s.flush_columns(&mut Tx::default());
    }

    fn run(s: &mut State, cmd: Cmd) {
        assert_eq!(s.unavailable(cmd), None, "{cmd:?}");
        s.run(cmd, &mut Tx::default());
    }

    fn cols(s: &State) -> &Columns {
        s.cur().cols.as_deref().expect("columns")
    }

    fn left(s: &State) -> Vec<(PathBuf, Option<String>)> {
        cols(s).left.iter().map(|c| (c.path.clone(), c.pick.as_ref().map(|p| p.to_string_lossy().into_owned()))).collect()
    }

    /// → opens the selected folder as the active column from the listing on its right, in the
    /// same frame (no read: nothing is loading after the key); ← goes back the same way and
    /// the folder left becomes the right column at once. Selecting a folder in a column to the
    /// left replaces every deeper column; a file shows its preview on the right instead.
    #[test]
    fn columns_open_and_close_as_the_selection_moves() {
        let d = tree("nav");
        let mut s = columns_in(&d);
        assert_eq!(cols(&s).root, d);
        assert!(cols(&s).left.is_empty(), "the root is the active column");
        select(&mut s, "a");
        settle(&mut s);
        let right = cols(&s).right.as_ref().expect("a's contents on the right");
        assert_eq!((right.path.clone(), right.load.clone()), (d.join("a"), Load::Ready));
        assert_eq!(right.visible.len(), 3, "b, x.txt, y.txt");
        let lands = s.lands;
        run(&mut s, Cmd::Into);
        assert!(!s.cur().is_loading(), "landed in the same frame, from the column's listing");
        assert_eq!((s.cur().cwd.clone(), s.lands), (d.join("a"), lands + 1));
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("b"), "the first item, as Finder does");
        assert_eq!(left(&s), [(d.clone(), Some("a".into()))]);
        assert_eq!(cols(&s).right.as_ref().map(|c| c.path.clone()), Some(d.join("a/b")));
        settle(&mut s);
        run(&mut s, Cmd::Into);
        assert_eq!(s.cur().cwd, d.join("a/b"));
        assert_eq!(left(&s), [(d.clone(), Some("a".into())), (d.join("a"), Some("b".into()))]);
        assert_eq!(s.cur().back, [d.clone(), d.join("a")], "each move is a navigation (⌘[ goes back)");
        // ←: a/b's listing becomes the right column at once.
        run(&mut s, Cmd::Parent);
        assert!(!s.cur().is_loading());
        assert_eq!(s.cur().cwd, d.join("a"));
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("b"), "the folder it came from");
        let right = cols(&s).right.as_ref().unwrap();
        assert_eq!((right.path.clone(), right.load.clone()), (d.join("a/b"), Load::Ready), "no read for it");
        // A file: its preview on the right, no column.
        select(&mut s, "x.txt");
        assert!(cols(&s).right.is_none() && cols(&s).preview);
        assert!(s.preview.is_some() && s.col_preview, "the column view's preview opened with the selection");
        assert_eq!(s.preview.as_ref().and_then(|p| p.target.as_ref()).map(|t| t.path()), Some(d.join("a/x.txt")));
        assert!(!s.panel_docked(), "drawn in the columns, not docked");
        // Space turns it off (and on); Esc doesn't close it.
        run(&mut s, Cmd::TogglePreview);
        assert!(s.preview.is_none() && !cols(&s).preview);
        run(&mut s, Cmd::TogglePreview);
        assert!(s.preview.is_some() && cols(&s).preview);
        select(&mut s, "b");
        assert!(s.preview.is_none(), "a folder: its contents instead");
        // Into the deepest, then a press on another folder in the root's column: everything
        // deeper goes.
        run(&mut s, Cmd::Into);
        settle(&mut s);
        run(&mut s, Cmd::Into);
        assert_eq!(s.cur().cwd, d.join("a/b/c"));
        assert_eq!(cols(&s).left.len(), 3);
        let id = s.cur().id;
        assert!(s.activate_column(id, &d, Some(&"other".into()), &mut Tx::default()));
        assert_eq!(s.cur().cwd, d);
        assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("other"));
        assert!(cols(&s).left.is_empty());
        assert_eq!(cols(&s).right.as_ref().map(|c| c.path.clone()), Some(d.join("other")));
        // Leaving the view drops every column.
        run(&mut s, Cmd::AsList);
        assert!(s.cur().cols.is_none() && s.preview.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A column's answer lands only for the read it's waiting for: an older read of it, one for
    /// another folder, or one after it went off screen is dropped.
    /// Another window's file operation reaches this window's columns: a column showing the
    /// folder it changed (here the one on the right, not the active one) reads it again, by
    /// identity. The other window's view and columns are its own.
    #[test]
    fn another_windows_operation_rereads_a_column_on_its_folder() {
        let d = tree("other-window");
        let mut s = columns_in(&d);
        select(&mut s, "a");
        settle(&mut s);
        let mut b = s.new_window(d.join("a"));
        crate::app::tests::settle_all(&mut b);
        assert_eq!(b.cur().view, ViewMode::List, "the new window's view is its own");
        assert!(b.cur().cols.is_none());
        b.new_folder();
        crate::app::tests::finish_job(&mut b);
        assert!(s.poll(&mut Tx::default()), "told");
        settle(&mut s);
        let right = cols(&s).right.as_ref().expect("a on the right");
        assert_eq!(right.path, d.join("a"));
        assert!(right.entries.iter().any(|e| e.name == "untitled folder"), "read again");
        assert_eq!(s.cur().view, ViewMode::Columns);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_stale_listing_never_lands_in_a_column() {
        let d = tree("stale");
        let mut c = Column::new(d.join("a"), None);
        let sort = Sort::new(fs::SortKey::Name, false);
        let answer = |req: u64, path: PathBuf| Answer { req, sort, listing: fs::list(path, sort) };
        let first = c.read(sort, false);
        drop(first);
        let old = c.req;
        let second = c.read(sort, false);
        drop(second);
        assert!(!c.land(answer(old, d.join("a"))), "an older read");
        assert!(!c.land(answer(c.req, d.join("other"))), "another folder's");
        assert_eq!(c.load, Load::Loading);
        c.drop_entries();
        assert!(!c.land(answer(c.req, d.join("a"))), "after it went off screen");
        assert!(c.entries.is_empty());
        let _third = c.read(sort, false);
        assert!(c.land(answer(c.req, d.join("a"))));
        assert_eq!((c.load.clone(), c.visible.len()), (Load::Ready, 3));
        // Through the state: the cursor moving on before a column's read is in, with the
        // column wait at zero so every read lands late; only the column still on screen fills.
        let mut s = columns_in(&d);
        s.column_wait = Duration::ZERO;
        select(&mut s, "a");
        select(&mut s, "other");
        settle(&mut s);
        let right = cols(&s).right.as_ref().unwrap();
        assert_eq!((right.path.clone(), right.visible.len()), (d.join("other"), 1));
        assert_eq!(cols(&s).holding(), [d.join("other")], "a's answer had nowhere to land");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Only the columns on screen hold entries: scrolled off, a column drops them; scrolled back
    /// (the wheel), it's read again. The active column always stays on screen.
    #[test]
    fn columns_off_screen_hold_no_entries() {
        let d = tree("offscreen");
        let mut s = columns_in(&d);
        for name in ["a", "b", "c"] {
            select(&mut s, name);
            settle(&mut s);
            run(&mut s, Cmd::Into);
            settle(&mut s);
        }
        assert_eq!(s.cur().cwd, d.join("a/b/c"));
        assert_eq!(cols(&s).holding(), [d.clone(), d.join("a"), d.join("a/b")]);
        // Room for two columns (and the preview of deep.txt on the right): only a/b and the
        // active one show.
        s.cur().cols_w.set(3.0 * (COL_W + DIVIDER));
        s.sync_preview();
        assert_eq!((cols(&s).fit, cols(&s).slots()), (3, 5), "d, a, a/b, c, the preview");
        assert_eq!(cols(&s).shown(), 2..5);
        assert_eq!(cols(&s).holding(), [d.join("a/b")], "d and a dropped their entries");
        assert!(cols(&s).left[..2].iter().all(|c| c.load == Load::Off && c.visible.is_empty()));
        // The wheel scrolls back a column at a time, as far as the active one stays on screen.
        let id = s.cur().id;
        let wheel = rune::WheelEvent::new(200.0, 0.0).precise(true);
        assert!(s.columns_wheel(id, &wheel, &mut Tx::default()));
        settle(&mut s);
        assert_eq!(cols(&s).shown(), 1..4, "the active column (3) stays on screen");
        assert_eq!(cols(&s).holding(), [d.join("a"), d.join("a/b")], "a read again");
        assert!(s.preview.is_none(), "the preview, scrolled off, closed");
        // A move shows the deepest again; scrolled back past it, the right column drops its
        // entries too.
        run(&mut s, Cmd::Parent);
        settle(&mut s);
        assert_eq!((cols(&s).slots(), cols(&s).shown()), (4, 1..4), "d, a, a/b (active), c");
        assert!(s.columns_wheel(id, &wheel, &mut Tx::default()));
        settle(&mut s);
        assert_eq!(cols(&s).shown(), 0..3);
        assert_eq!(cols(&s).holding(), [d.clone(), d.join("a")], "c, off screen on the right, holds nothing");
        select(&mut s, "x.txt");
        assert_eq!(cols(&s).shown(), 1..4, "the deepest: a, a/b and x.txt's preview");
        assert!(s.preview.is_some(), "on screen again: read again");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Actions resolve against the folder of the column they're in: a press on `x.txt` in a/'s
    /// column while a/b (which has its own `x.txt`) is active makes a/ the active folder first,
    /// so Duplicate copies a/x.txt, never a/b/x.txt.
    #[test]
    fn actions_act_on_the_column_they_are_in() {
        let d = tree("target");
        let mut s = columns_in(&d);
        select(&mut s, "a");
        settle(&mut s);
        run(&mut s, Cmd::Into);
        settle(&mut s);
        run(&mut s, Cmd::Into);
        select(&mut s, "x.txt");
        assert_eq!(s.target_paths(), [d.join("a/b/x.txt")]);
        let id = s.cur().id;
        // What the right-click handler does before opening the menu.
        assert!(s.activate_column(id, &d.join("a"), Some(&"x.txt".into()), &mut Tx::default()));
        assert_eq!(s.target_paths(), [d.join("a/x.txt")], "the outer one");
        run(&mut s, Cmd::Duplicate);
        crate::app::tests::finish_job(&mut s);
        assert!(
            d.join("a/x copy.txt").exists(),
            "{:?}",
            std::fs::read_dir(d.join("a")).unwrap().flatten().map(|e| e.file_name()).collect::<Vec<_>>()
        );
        assert!(!d.join("a/b/x copy.txt").exists());
        // Paste and New Folder go to the active column's folder too.
        assert_eq!(s.cur().cwd, d.join("a"));
        let _ = std::fs::remove_dir_all(&d);
    }
    /// A file operation that changes a folder shown as a column reads that column again (as it
    /// does the active folder), and so does ⌘R, the rows staying until the new listing lands.
    #[test]
    fn columns_read_again_after_a_change() {
        let d = tree("reread");
        let mut s = columns_in(&d);
        select(&mut s, "other");
        settle(&mut s);
        let rows = |s: &State| -> Vec<String> {
            let c = cols(s).right.as_ref().unwrap();
            c.visible.iter().map(|&i| c.entries[i as usize].name.clone()).collect()
        };
        assert_eq!(rows(&s), ["o.txt"]);
        // top.txt dropped on the folder whose contents are the right column.
        let dest = cols(&s).right.as_ref().unwrap().dest();
        assert_eq!(dest.path, d.join("other"));
        s.drop_into(&[d.join("top.txt")], dest, Default::default(), &mut Tx::default());
        crate::app::tests::finish_job(&mut s);
        settle(&mut s);
        assert_eq!(rows(&s), ["o.txt", "top.txt"]);
        std::fs::write(d.join("other/new.txt"), "n").unwrap();
        run(&mut s, Cmd::Reload);
        assert!(cols(&s).right.as_ref().unwrap().listed(), "still showing its rows while it's read again");
        settle(&mut s);
        assert_eq!(rows(&s), ["new.txt", "o.txt", "top.txt"]);
        let _ = std::fs::remove_dir_all(&d);
    }
    /// Moving between columns whose listings are on screen moves those listings, nothing
    /// more: no copy, no filtering or sorting again, the same rows under the same version (so
    /// Rune looks at no key) and the same element. That's what keeps such a move within a
    /// frame over a large folder (BENCH.md).
    #[test]
    fn a_move_between_listed_columns_keeps_their_rows() {
        let d = tree("keep-rows");
        for i in 0..300 {
            std::fs::write(d.join(format!("a/f{i:03}.txt")), "x").unwrap();
        }
        let mut s = columns_in(&d);
        select(&mut s, "a");
        settle(&mut s);
        let r = cols(&s).right.as_ref().unwrap();
        let (entries, rows, version, key) = (r.entries.as_ptr(), r.visible.as_ptr(), r.version, r.rows_key());
        run(&mut s, Cmd::Into);
        let f = s.cur();
        assert_eq!(f.cwd, d.join("a"));
        assert_eq!((f.entries.as_ptr(), f.visible.as_ptr()), (entries, rows), "moved, not copied or worked out again");
        assert_eq!((f.version, f.rows_key()), (version, key), "the same rows' version, the same element");
        let (entries, rows, version, key) = (f.entries.as_ptr(), f.visible.as_ptr(), f.version, f.rows_key());
        settle(&mut s);
        let reads = s.col_reads;
        run(&mut s, Cmd::Parent);
        assert_eq!(s.col_reads, reads, "nothing read: both listings were on screen");
        let r = cols(&s).right.as_ref().unwrap();
        assert_eq!(r.path, d.join("a"));
        assert_eq!((r.entries.as_ptr(), r.visible.as_ptr()), (entries, rows), "back on the right, as it was");
        assert_eq!((r.version, r.rows_key()), (version, key));
        // A filter in the folder left means its rows aren't the whole listing: worked out again.
        run(&mut s, Cmd::Into);
        s.cur_mut().type_filter("f0");
        let filtered = s.cur().version;
        run(&mut s, Cmd::Parent);
        let r = cols(&s).right.as_ref().unwrap();
        assert_eq!(r.visible.len(), 303, "the whole folder: b, x.txt, y.txt and the 300");
        assert_ne!(r.version, filtered);
        let _ = std::fs::remove_dir_all(&d);
    }
}
